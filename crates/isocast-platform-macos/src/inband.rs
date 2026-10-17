//! One video sample description per joined file (#94).
//!
//! Frames an export encodes again (C.3's cuts, angles, crops) come out of
//! VideoToolbox with their own parameter sets (H.264 SPS/PPS, HEVC
//! VPS/SPS/PPS), which differ from the recording's in a few fields.
//! AVAssetWriter gave them a second sample description in the file, and
//! Chrome (whose MP4 reader keeps the first) failed to decode HEVC exports
//! there. Instead, every sample is written with the joined file's format,
//! and every keyframe carries the parameter sets it was encoded with
//! in-band, in front of its frame: the encoded-again ones theirs, the
//! copied ones the recording's (so a seek to any keyframe decodes, whatever
//! came before). The copied frames' own bytes are unchanged. Chrome,
//! Safari, QuickTime (AVFoundation) and ffmpeg all decode such files, bit
//! for bit as before (checked 2026-10-16, see docs/notes.md).
//!
//! ProRes has no parameter sets: its encoded-again samples just take the
//! joined file's format. A sample in another codec, size or profile (bit
//! depth) than the joined file is left as it is (its own sample
//! description, as before).

use std::ptr::{self, NonNull};

use anyhow::{Result, bail};
use objc2_core_foundation::{CFBoolean, CFMutableDictionary, CFRetained, CFString, Type};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime,
    CMVideoFormatDescriptionGetDimensions, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex, kCMBlockBufferAssureMemoryNowFlag,
    kCMSampleAttachmentKey_NotSync, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};

use crate::recode::is_sync;

/// A video format's parameter sets and how its samples give NAL lengths.
struct ParameterSets {
    sets: Vec<Vec<u8>>,
    /// Bytes per NAL unit length in a sample.
    length_size: usize,
    /// The SPS's profile (H.264 `profile_idc`; HEVC `general_profile_idc`).
    profile: Option<u8>,
}

fn parameter_sets(format: &CMFormatDescription) -> Option<ParameterSets> {
    let codec = unsafe { format.media_sub_type() };
    let hevc = codec == kCMVideoCodecType_HEVC;
    if !hevc && codec != kCMVideoCodecType_H264 {
        return None;
    }
    let get = |index: usize,
               pointer: *mut *const u8,
               size: *mut usize,
               count: *mut usize,
               length: *mut i32| unsafe {
        match hevc {
            true => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                format, index, pointer, size, count, length,
            ),
            false => CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format, index, pointer, size, count, length,
            ),
        }
    };
    let (mut count, mut length_size) = (0usize, 0i32);
    if get(
        0,
        ptr::null_mut(),
        ptr::null_mut(),
        &mut count,
        &mut length_size,
    ) != 0
        || count == 0
        || !matches!(length_size, 1 | 2 | 4)
    {
        return None;
    }
    let mut sets = Vec::new();
    let mut profile = None;
    for index in 0..count {
        let (mut pointer, mut size) = (ptr::null(), 0usize);
        if get(
            index,
            &mut pointer,
            &mut size,
            ptr::null_mut(),
            ptr::null_mut(),
        ) != 0
            || pointer.is_null()
            || size == 0
        {
            return None;
        }
        let nal = unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec();
        match hevc {
            // NAL header (2 bytes), VPS id/sub-layers (1), then
            // profile_space(2) tier(1) profile_idc(5).
            true if (nal[0] >> 1) & 0x3f == 33 && nal.len() > 3 => {
                profile = Some(nal[3] & 0x1f);
            }
            false if nal[0] & 0x1f == 7 && nal.len() > 1 => profile = Some(nal[1]),
            _ => {}
        }
        sets.push(nal);
    }
    Some(ParameterSets {
        sets,
        length_size: length_size as usize,
        profile,
    })
}

/// Whether `format` is HEVC Main 10.
pub(crate) fn main10(format: &CMFormatDescription) -> bool {
    (unsafe { format.media_sub_type() }) == kCMVideoCodecType_HEVC
        && parameter_sets(format).and_then(|sets| sets.profile) == Some(2)
}

/// Puts every sample of a joined file in its one format.
pub(crate) struct OneFormat {
    hint: CFRetained<CMFormatDescription>,
    sets: Option<ParameterSets>,
    /// Keyframes carry their parameter sets in-band: the file holds
    /// frames encoded again (else every sample is copied as it is).
    inband: bool,
}

impl OneFormat {
    pub fn new(hint: &CMFormatDescription, inband: bool) -> Self {
        let sets = parameter_sets(hint);
        Self {
            inband: inband && sets.is_some(),
            hint: hint.retain(),
            sets,
        }
    }

    /// Whether a sample of `format` is in the joined file's format already.
    fn same(&self, format: &CMFormatDescription) -> bool {
        unsafe { CMFormatDescription::equal(Some(format), Some(&self.hint)) }
    }

    /// `sample` in the joined file's format: the same sample if nothing
    /// needs changing.
    pub fn adapt(&self, sample: CFRetained<CMSampleBuffer>) -> Result<CFRetained<CMSampleBuffer>> {
        let Some(format) = (unsafe { sample.format_description() }) else {
            return Ok(sample);
        };
        let same = self.same(&format);
        let sync = is_sync(&sample);
        let Some(joined) = &self.sets else {
            // ProRes (or a format without parameter sets).
            if same || !self.compatible(&format) {
                return Ok(sample);
            }
            return rewrap(&sample, &self.hint, None, sync);
        };
        if same && !(self.inband && sync) {
            return Ok(sample);
        }
        if !same && !self.compatible(&format) {
            return Ok(sample);
        }
        let Some(own) = parameter_sets(&format) else {
            return Ok(sample);
        };
        if !same && own.profile != joined.profile {
            // Another bit depth or chroma format: its own description.
            return Ok(sample);
        }
        if !sync && own.length_size == joined.length_size {
            // The same bytes, described by the joined file's format.
            return rewrap(&sample, &self.hint, None, sync);
        }
        let data = sample_bytes(&sample)?;
        let mut out = Vec::with_capacity(data.len() + 256);
        if sync {
            for set in &own.sets {
                push_nal(&mut out, set, joined.length_size)?;
            }
        }
        let mut rest = &data[..];
        while !rest.is_empty() {
            if rest.len() < own.length_size {
                bail!("a frame ends inside a NAL unit length");
            }
            let length = rest[..own.length_size]
                .iter()
                .fold(0usize, |n, b| (n << 8) | usize::from(*b));
            let nal = rest
                .get(own.length_size..own.length_size + length)
                .ok_or_else(|| anyhow::anyhow!("a frame's NAL unit runs past its end"))?;
            push_nal(&mut out, nal, joined.length_size)?;
            rest = &rest[own.length_size + length..];
        }
        rewrap(&sample, &self.hint, Some(&out), sync)
    }

    /// Same codec and picture size as the joined file.
    fn compatible(&self, format: &CMFormatDescription) -> bool {
        let size = |f: &CMFormatDescription| {
            let d = unsafe { CMVideoFormatDescriptionGetDimensions(f) };
            (d.width, d.height)
        };
        let codec = unsafe { format.media_sub_type() };
        codec == unsafe { self.hint.media_sub_type() } && size(format) == size(&self.hint)
    }
}

fn push_nal(out: &mut Vec<u8>, nal: &[u8], length_size: usize) -> Result<()> {
    if length_size < 4 && nal.len() >> (8 * length_size) != 0 {
        bail!("a NAL unit is too long for {length_size}-byte lengths");
    }
    out.extend_from_slice(&(nal.len() as u32).to_be_bytes()[4 - length_size..]);
    out.extend_from_slice(nal);
    Ok(())
}

fn sample_bytes(sample: &CMSampleBuffer) -> Result<Vec<u8>> {
    let Some(block) = (unsafe { sample.data_buffer() }) else {
        bail!("a video frame without data");
    };
    let len = unsafe { block.data_length() };
    let mut data = vec![0u8; len];
    if len > 0 {
        let status = unsafe {
            block.copy_data_bytes(0, len, NonNull::new(data.as_mut_ptr().cast()).expect("vec"))
        };
        if status != 0 {
            bail!("could not read a video frame (CoreMedia error {status})");
        }
    }
    Ok(data)
}

/// `sample` with `format`, and `data` instead of its own bytes if given.
fn rewrap(
    sample: &CMSampleBuffer,
    format: &CMFormatDescription,
    data: Option<&[u8]>,
    sync: bool,
) -> Result<CFRetained<CMSampleBuffer>> {
    let block = match data {
        None => match unsafe { sample.data_buffer() } {
            Some(block) => block,
            None => bail!("a video frame without data"),
        },
        Some(data) => {
            let mut block = ptr::null_mut();
            let status = unsafe {
                CMBlockBuffer::create_with_memory_block(
                    None,
                    ptr::null_mut(),
                    data.len(),
                    None,
                    ptr::null(),
                    0,
                    data.len(),
                    kCMBlockBufferAssureMemoryNowFlag,
                    NonNull::from(&mut block),
                )
            };
            let Some(block) = NonNull::new(block).filter(|_| status == 0) else {
                bail!("could not allocate a video frame (CoreMedia error {status})");
            };
            let block = unsafe { CFRetained::from_raw(block) };
            let source = NonNull::new(data.as_ptr().cast_mut()).expect("slice pointer");
            let status =
                unsafe { CMBlockBuffer::replace_data_bytes(source.cast(), &block, 0, data.len()) };
            if status != 0 {
                bail!("could not copy a video frame (CoreMedia error {status})");
            }
            block
        }
    };
    let mut timing = CMSampleTimingInfo {
        duration: unsafe { CMTime::new(0, 1) },
        presentationTimeStamp: unsafe { CMTime::new(0, 1) },
        decodeTimeStamp: unsafe { CMTime::new(0, 1) },
    };
    let mut entries = 0;
    let status = unsafe { sample.sample_timing_info_array(1, &mut timing, &mut entries) };
    if status != 0 || entries != 1 {
        bail!("a video frame without one timing (CoreMedia error {status})");
    }
    let size = unsafe { block.data_length() };
    let mut out = ptr::null_mut();
    let status = unsafe {
        CMSampleBuffer::create(
            None,
            Some(&block),
            true,
            None,
            ptr::null_mut(),
            Some(format),
            1,
            1,
            &timing,
            1,
            &size,
            NonNull::from(&mut out),
        )
    };
    let Some(out) = NonNull::new(out).filter(|_| status == 0) else {
        bail!("could not wrap a video frame (CoreMedia error {status})");
    };
    let out = unsafe { CFRetained::<CMSampleBuffer>::from_raw(out) };
    if !sync {
        let Some(attachments) = (unsafe { out.sample_attachments_array(true) }) else {
            bail!("a video frame without attachments");
        };
        let first = unsafe { attachments.value_at_index(0) } as *const CFMutableDictionary;
        let key: &CFString = unsafe { kCMSampleAttachmentKey_NotSync };
        unsafe {
            CFMutableDictionary::set_value(
                first.as_ref(),
                (key as *const CFString).cast(),
                (CFBoolean::new(true) as *const CFBoolean).cast(),
            )
        };
    }
    Ok(out)
}
