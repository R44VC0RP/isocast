//! Media Foundation: H.264 access units to RGBA pictures. Windows' H.264
//! decoder MFT decodes on the GPU (DXVA) through a D3D11 device of our own,
//! in low-latency mode (a picture out for each one in); its NV12 pictures
//! are read back and converted to RGBA here (half a million pixels at 960
//! wide). Without a usable GPU the same MFT decodes in software.
//!
//! Not `Send`: made, used and dropped on the one thread that receives the
//! video (it joins the MTA and starts Media Foundation).

use std::mem::ManuallyDrop;

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11CreateDevice, ID3D11Device, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::core::Interface;

use super::{AccessUnit, Decoder, ParameterSets, Picture, nal_units};

/// Output types to look through: decoders offer a handful.
const MAX_TYPES: u32 = 64;

struct Started {
    sets: ParameterSets,
    transform: IMFTransform,
    hardware: bool,
    _activation: Activation,
}

impl Drop for Started {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

struct Activation(IMFActivate);

impl Drop for Activation {
    fn drop(&mut self) {
        let _ = unsafe { self.0.ShutdownObject() };
    }
}

pub struct MediaFoundation {
    /// The GPU the decoder uses (none: software).
    manager: Option<(ID3D11Device, IMFDXGIDeviceManager)>,
    started: Option<Started>,
}

impl MediaFoundation {
    pub fn new() -> Result<Self> {
        unsafe {
            // Already in another mode on this thread: fine, COM works.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_LITE).context("could not start Media Foundation")?;
        }
        let manager = match gpu() {
            Ok(manager) => Some(manager),
            Err(error) => {
                isocast_diagnostics::diagnostic!("warn"; "video: no GPU decoding, software instead: {error:#}");
                None
            }
        };
        Ok(Self {
            manager,
            started: None,
        })
    }

    fn start(&self, sets: &ParameterSets, unit: &AccessUnit) -> Result<Started> {
        let mut failures = Vec::new();
        for activate in decoders()? {
            let activation = Activation(activate);
            match self.open(&activation, unit) {
                Ok((transform, hardware)) => {
                    return Ok(Started {
                        sets: sets.clone(),
                        transform,
                        hardware,
                        _activation: activation,
                    });
                }
                Err(error) => failures.push(format!("{error:#}")),
            }
        }
        // Windows N without the Media Feature Pack has none at all.
        Err(super::NoDecoder(format!(
            "no usable H.264 decoder{}",
            match failures.is_empty() {
                true => " is installed".to_string(),
                false => format!(": {}", failures.join("; ")),
            }
        ))
        .into())
    }

    fn open(&self, activation: &Activation, unit: &AccessUnit) -> Result<(IMFTransform, bool)> {
        unsafe {
            let transform: IMFTransform = activation.0.ActivateObject()?;
            let attributes = transform.GetAttributes().ok();
            if let Some(attributes) = &attributes {
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let mut hardware = false;
            if let (Some((_, manager)), Some(attributes)) = (&self.manager, &attributes)
                && attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0
            {
                hardware = transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                    .is_ok();
            }
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            // Unknown sizes (0) come from the SPS once decoding starts.
            if unit.width > 0 && unit.height > 0 {
                input.SetUINT64(
                    &MF_MT_FRAME_SIZE,
                    (u64::from(unit.width) << 32) | u64::from(unit.height),
                )?;
            }
            input.SetUINT64(&MF_MT_FRAME_RATE, (30u64 << 32) | 1)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform
                .SetInputType(0, &input, 0)
                .context("the decoder refused H.264 input")?;
            choose_nv12(&transform)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok((transform, hardware))
        }
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        // The decoder and the device first, then Media Foundation.
        self.started = None;
        self.manager = None;
        let _ = unsafe { MFShutdown() };
    }
}

impl Decoder for MediaFoundation {
    fn name(&self) -> String {
        match &self.started {
            Some(started) if started.hardware => "Media Foundation (DXVA)".into(),
            Some(_) => "Media Foundation (software)".into(),
            None => "Media Foundation".into(),
        }
    }

    fn decode(&mut self, sets: &ParameterSets, unit: &AccessUnit) -> Result<Option<Picture>> {
        if self.started.as_ref().is_none_or(|s| s.sets != *sets) {
            if !unit.keyframe {
                return Ok(None);
            }
            self.started = None;
            let started = self.start(sets, unit)?;
            isocast_diagnostics::diagnostic!("info"; "video: decoding {}x{} H.264 with Media Foundation ({})", unit.width, unit.height, if started.hardware { "DXVA" } else { "software" });
            self.started = Some(started);
        }
        let transform = self.started.as_ref().unwrap().transform.clone();
        let result = decode(&transform, sets, unit);
        if result.is_err() {
            // Start again at the next keyframe.
            self.started = None;
        }
        result
    }
}

fn decode(
    transform: &IMFTransform,
    sets: &ParameterSets,
    unit: &AccessUnit,
) -> Result<Option<Picture>> {
    let sample = input_sample(sets, unit)?;
    let mut picture = None;
    unsafe {
        if let Err(error) = transform.ProcessInput(0, &sample, 0) {
            if error.code() != MF_E_NOTACCEPTING {
                return Err(error).context("the decoder refused a picture");
            }
            // A picture waits to come out first.
            picture = drain(transform, unit)?;
            transform
                .ProcessInput(0, &sample, 0)
                .context("the decoder refused a picture")?;
        }
    }
    Ok(drain(transform, unit)?.or(picture))
}

/// Every picture the decoder has ready: the newest.
fn drain(transform: &IMFTransform, unit: &AccessUnit) -> Result<Option<Picture>> {
    let mut newest = None;
    let mut changes = 0;
    loop {
        let info = unsafe { transform.GetOutputStreamInfo(0)? };
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        let ours = match provides {
            true => None,
            // Software decoding: a 2-D NV12 buffer of the output's size, so
            // its pitch is known.
            false => unsafe {
                let size = transform
                    .GetOutputCurrentType(0)?
                    .GetUINT64(&MF_MT_FRAME_SIZE)?;
                let buffer = MFCreate2DMediaBuffer(
                    (size >> 32) as u32,
                    size as u32,
                    MFVideoFormat_NV12.data1,
                    false,
                )
                .or_else(|_| MFCreateMemoryBuffer(info.cbSize.max(1)))?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                Some(sample)
            },
        };
        let mut buffer = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(ours),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0;
        let result =
            unsafe { transform.ProcessOutput(0, std::slice::from_mut(&mut buffer), &mut status) };
        let produced = unsafe { ManuallyDrop::take(&mut buffer.pSample) };
        drop(unsafe { ManuallyDrop::take(&mut buffer.pEvents) });
        match result {
            Ok(()) => {
                if let Some(sample) = produced {
                    newest = Some(picture(transform, &sample, unit)?);
                }
            }
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(newest),
            Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE && changes < 3 => {
                changes += 1;
                choose_nv12(transform)?;
            }
            Err(error) => return Err(error).context("the decoder failed"),
        }
    }
}

/// `unit` as an Annex B sample (start codes, the parameter sets before a
/// keyframe), as Windows' H.264 decoder reads.
fn input_sample(sets: &ParameterSets, unit: &AccessUnit) -> Result<IMFSample> {
    let mut bytes = Vec::with_capacity(unit.data.len() + 64);
    let mut put = |nal: &[u8]| {
        bytes.extend_from_slice(&[0, 0, 0, 1]);
        bytes.extend_from_slice(nal);
    };
    if unit.keyframe {
        sets.sps.iter().chain(&sets.pps).for_each(|nal| put(nal));
    }
    nal_units(&unit.data).for_each(&mut put);
    unsafe {
        let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
        let mut at = std::ptr::null_mut();
        buffer.Lock(&mut at, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime((unit.time / 100) as i64)?;
        if unit.keyframe {
            sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
        }
        Ok(sample)
    }
}

/// A decoded NV12 sample as RGBA, cropped to the picture's size.
fn picture(transform: &IMFTransform, sample: &IMFSample, unit: &AccessUnit) -> Result<Picture> {
    unsafe {
        let current = transform.GetOutputCurrentType(0)?;
        let size = current.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let (coded_width, coded_height) = ((size >> 32) as u32, size as u32);
        // The picture is the stream's size when known; else the type's
        // display aperture (the coded size is padded to whole macroblocks:
        // 1088 for 1080); else the coded size.
        let (width, height) = match (unit.width, unit.height) {
            (0, _) | (_, 0) => aperture(&current).unwrap_or((coded_width, coded_height)),
            (w, h) => (w, h),
        };
        let (width, height) = (width.min(coded_width), height.min(coded_height));
        let buffer = sample.GetBufferByIndex(0)?;
        // A GPU surface's chroma follows its own height (often aligned
        // further than the type says).
        let plane_height = match buffer.cast::<IMFDXGIBuffer>() {
            Ok(dxgi) => {
                let mut texture: Option<ID3D11Texture2D> = None;
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut texture as *mut _ as *mut *mut _)?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture
                    .context("a decoded surface without a texture")?
                    .GetDesc(&mut desc);
                desc.Height
            }
            Err(_) => coded_height,
        };
        let convert = |scan0: *const u8, pitch: usize, length: usize| {
            let luma_bytes = pitch * plane_height as usize;
            let chroma_bytes = pitch * (plane_height as usize).div_ceil(2);
            if scan0.is_null() || pitch < width as usize || length < luma_bytes + chroma_bytes {
                return Err(anyhow!("a decoded picture without pixels"));
            }
            let luma = std::slice::from_raw_parts(scan0, luma_bytes);
            let chroma = std::slice::from_raw_parts(scan0.add(luma_bytes), chroma_bytes);
            Ok(nv12_to_rgba(luma, chroma, pitch, width, height))
        };
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
            let mut scan0 = std::ptr::null_mut();
            let mut pitch = 0i32;
            two_d.Lock2D(&mut scan0, &mut pitch)?;
            let result = match pitch > 0 {
                true => convert(scan0, pitch as usize, usize::MAX),
                false => Err(anyhow!("a bottom-up decoded picture")),
            };
            let _ = two_d.Unlock2D();
            return result;
        }
        // A plain buffer: rows of the type's default stride (else the
        // width).
        let pitch = current
            .GetUINT32(&MF_MT_DEFAULT_STRIDE)
            .map(|stride| stride as i32)
            .unwrap_or(coded_width as i32);
        if pitch <= 0 {
            bail!("a bottom-up decoded picture");
        }
        let mut at = std::ptr::null_mut();
        let mut length = 0u32;
        buffer.Lock(&mut at, None, Some(&mut length))?;
        let result = convert(at, pitch as usize, length as usize);
        let _ = buffer.Unlock();
        result
    }
}

/// The type's minimum display aperture, if it says.
fn aperture(media_type: &IMFMediaType) -> Option<(u32, u32)> {
    let mut area = MFVideoArea::default();
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(
            &mut area as *mut MFVideoArea as *mut u8,
            std::mem::size_of::<MFVideoArea>(),
        )
    };
    unsafe { media_type.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, None) }.ok()?;
    (area.Area.cx > 0 && area.Area.cy > 0).then_some((area.Area.cx as u32, area.Area.cy as u32))
}

/// BT.709 limited-range NV12 (what the engine's encoders write) to RGBA:
/// two pixels per chroma sample.
fn nv12_to_rgba(luma: &[u8], chroma: &[u8], pitch: usize, width: u32, height: u32) -> Picture {
    let (width, height) = (width as usize, height as usize);
    let mut picture = Picture::new(width as u32, height as u32);
    if width == 0 || height == 0 {
        return picture;
    }
    let out = picture.make_mut_bytes();
    let clamp = |value: i32| ((value + 128) >> 8).clamp(0, 255) as u8;
    for (y, dest) in out.chunks_exact_mut(width * 4).enumerate().take(height) {
        let row = &luma[y * pitch..y * pitch + width];
        let uv = &chroma[(y / 2) * pitch..(y / 2) * pitch + width.next_multiple_of(2)];
        for ((pixels, ys), &[u, v]) in dest
            .chunks_mut(8)
            .zip(row.chunks(2))
            .zip(uv.as_chunks::<2>().0)
        {
            let (u, v) = (i32::from(u) - 128, i32::from(v) - 128);
            let (r, g, b) = (459 * v, -55 * u - 136 * v, 541 * u);
            for (pixel, &luma) in pixels.as_chunks_mut::<4>().0.iter_mut().zip(ys) {
                let c = (i32::from(luma) - 16) * 298;
                *pixel = [clamp(c + r), clamp(c + g), clamp(c + b), 255];
            }
        }
    }
    picture
}

/// Our own D3D11 device for DXVA, shared with Media Foundation.
fn gpu() -> Result<(ID3D11Device, IMFDXGIDeviceManager)> {
    unsafe {
        let mut device = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .context("could not make a D3D11 video device")?;
        let device: ID3D11Device = device.context("no D3D11 device")?;
        // Media Foundation uses the device from its own threads too.
        let _ = device
            .cast::<ID3D11Multithread>()?
            .SetMultithreadProtected(true);
        let mut token = 0;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager: IMFDXGIDeviceManager = manager.context("no DXGI device manager")?;
        manager.ResetDevice(&device, token)?;
        Ok((device, manager))
    }
}

/// Synchronous H.264 decoders, best first.
fn decoders() -> Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut list = std::ptr::null_mut();
    let mut count = 0;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut list,
            &mut count,
        )
    }
    .context("could not list H.264 decoders")?;
    let mut result = Vec::new();
    if !list.is_null() {
        for item in unsafe { std::slice::from_raw_parts_mut(list, count as usize) } {
            if let Some(item) = item.take() {
                result.push(item);
            }
        }
        unsafe { CoTaskMemFree(Some(list.cast())) };
    }
    Ok(result)
}

/// Make NV12 the decoder's output.
fn choose_nv12(transform: &IMFTransform) -> Result<()> {
    for index in 0..MAX_TYPES {
        let available = match unsafe { transform.GetOutputAvailableType(0, index) } {
            Ok(available) => available,
            Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
            Err(error) => return Err(error).context("could not list the decoder's outputs"),
        };
        if unsafe { available.GetGUID(&MF_MT_SUBTYPE) }? == MFVideoFormat_NV12 {
            unsafe { transform.SetOutputType(0, &available, 0) }
                .context("the decoder refused NV12 output")?;
            return Ok(());
        }
    }
    bail!("the decoder doesn't offer NV12")
}

#[cfg(test)]
mod tests {
    #[test]
    fn nv12_greys_and_colours() {
        // 2x2: Y 16 (black) and 235 (white), neutral chroma.
        let picture = super::nv12_to_rgba(&[16, 235, 16, 235], &[128, 128], 2, 2, 2);
        let bytes = picture.as_bytes();
        assert_eq!(&bytes[..8], &[0, 0, 0, 255, 255, 255, 255, 255]);
    }
}
