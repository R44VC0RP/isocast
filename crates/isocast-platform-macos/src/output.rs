//! Audio output devices: listing them, and playing monitored audio on one
//! through a Core Audio output unit. "default" uses the system's default
//! output unit, which follows the system output when it changes; a named
//! device gets the HAL output unit pointed at it. The unit converts the
//! engine's 48 kHz stereo floats to whatever the device runs at.

use std::ffi::c_void;
use std::mem::{MaybeUninit, size_of};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::{self, NonNull};
use std::slice;

use anyhow::{Result, anyhow, bail};
use isocast_engine::{AudioRender, SAMPLE_RATE};
use isocast_protocol::devices::AudioOutput;
use objc2_audio_toolbox::{
    AURenderCallbackStruct, AudioComponentDescription, AudioComponentFindNext,
    AudioComponentInstanceDispose, AudioComponentInstanceNew, AudioOutputUnitStart,
    AudioOutputUnitStop, AudioUnit, AudioUnitInitialize, AudioUnitRenderActionFlags,
    AudioUnitSetProperty, AudioUnitUninitialize, kAudioOutputUnitProperty_CurrentDevice,
    kAudioUnitManufacturer_Apple, kAudioUnitProperty_SetRenderCallback,
    kAudioUnitProperty_StreamFormat, kAudioUnitScope_Global, kAudioUnitScope_Input,
    kAudioUnitSubType_DefaultOutput, kAudioUnitSubType_HALOutput, kAudioUnitType_Output,
};
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyStreamConfiguration,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
};
use objc2_core_audio_types::{
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, kAudioFormatFlagIsFloat,
    kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
};
use objc2_core_foundation::{CFRetained, CFString};

/// The output devices, the system's default output first.
pub fn audio_outputs() -> Result<Vec<AudioOutput>> {
    let default = default_output();
    let mut outputs: Vec<AudioOutput> = output_devices()?
        .into_iter()
        .map(|(device, channels)| AudioOutput {
            id: string_property(device, kAudioDevicePropertyDeviceUID).unwrap_or_default(),
            name: device_name(device),
            default: Some(device) == default,
            sample_rate: property::<f64>(
                device,
                kAudioDevicePropertyNominalSampleRate,
                kAudioObjectPropertyScopeGlobal,
            )
            .unwrap_or(0.0),
            channels,
        })
        .collect();
    outputs.sort_by_key(|output| !output.default);
    Ok(outputs)
}

/// Every device with output channels, and how many it has.
fn output_devices() -> Result<Vec<(AudioObjectID, u32)>> {
    let bytes = property_bytes(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
    )
    .ok_or_else(|| anyhow!("Core Audio did not list the audio devices"))?;
    let devices = bytes.as_chunks::<4>().0.iter();
    Ok(devices
        .map(|id| AudioObjectID::from_ne_bytes(*id))
        .map(|device| (device, output_channels(device)))
        .filter(|(_, channels)| *channels > 0)
        .collect())
}

/// The output channels of `device` (0 for an input-only device).
fn output_channels(device: AudioObjectID) -> u32 {
    let Some(bytes) = property_bytes(
        device,
        kAudioDevicePropertyStreamConfiguration,
        kAudioObjectPropertyScopeOutput,
    ) else {
        return 0;
    };
    if bytes.len() < size_of::<AudioBufferList>() {
        return 0;
    }
    // SAFETY: Core Audio filled the buffer with an AudioBufferList whose
    // buffers follow it; `property_bytes` aligns it for one.
    unsafe {
        let list = &*bytes.as_ptr().cast::<AudioBufferList>();
        let count = (list.mNumberBuffers as usize).min(
            (bytes.len() - size_of::<u64>()) / size_of::<objc2_core_audio_types::AudioBuffer>(),
        );
        slice::from_raw_parts(list.mBuffers.as_ptr(), count)
            .iter()
            .map(|buffer| buffer.mNumberChannels)
            .sum()
    }
}

pub(crate) fn default_output() -> Option<AudioObjectID> {
    property::<AudioObjectID>(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    )
    .filter(|device| *device != 0)
}

pub(crate) fn device_name(device: AudioObjectID) -> String {
    string_property(device, kAudioObjectPropertyName).unwrap_or_else(|| format!("Device {device}"))
}

/// Find an output device by id, name, name ignoring case, or the shortest
/// name containing `query`, as microphones and cameras are matched.
fn find_output(query: &str) -> Result<(AudioObjectID, String)> {
    let named: Vec<(AudioObjectID, String, String)> = output_devices()?
        .into_iter()
        .map(|(device, _)| {
            let id = string_property(device, kAudioDevicePropertyDeviceUID).unwrap_or_default();
            (device, id, device_name(device))
        })
        .collect();
    let lower = query.to_lowercase();
    let found = named
        .iter()
        .find(|(_, id, name)| id == query || name == query)
        .or_else(|| {
            named
                .iter()
                .find(|(_, _, name)| name.to_lowercase() == lower)
        })
        .or_else(|| {
            named
                .iter()
                .filter(|(_, _, name)| name.to_lowercase().contains(&lower))
                .min_by_key(|(_, _, name)| name.len())
        });
    match found {
        Some((device, _, name)) => Ok((*device, name.clone())),
        None => {
            let valid: Vec<String> = named
                .iter()
                .map(|(_, id, name)| format!("{name:?} (id {id})"))
                .collect();
            bail!(
                "no audio output matches {query:?}; valid: \"default\", {}",
                valid.join(", ")
            )
        }
    }
}

pub(crate) fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// A fixed-size property of a Core Audio object.
pub(crate) fn property<T: Copy>(object: AudioObjectID, selector: u32, scope: u32) -> Option<T> {
    let address = address(selector, scope);
    let mut value = MaybeUninit::<T>::uninit();
    let mut size = size_of::<T>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(value.as_mut_ptr().cast()).expect("not null"),
        )
    };
    (status == 0 && size as usize == size_of::<T>()).then(|| unsafe { value.assume_init() })
}

/// A variable-size property, as bytes aligned for any of its structs.
pub(crate) fn property_bytes(object: AudioObjectID, selector: u32, scope: u32) -> Option<Vec<u8>> {
    let address = address(selector, scope);
    let mut size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 || size == 0 {
        return None;
    }
    // u64s keep the buffer 8-byte aligned.
    let mut words = vec![0u64; (size as usize).div_ceil(size_of::<u64>())];
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(words.as_mut_ptr().cast()).expect("not null"),
        )
    };
    if status != 0 {
        return None;
    }
    let bytes = words.iter().flat_map(|w| w.to_ne_bytes());
    Some(bytes.take(size as usize).collect())
}

/// A string property (Core Audio hands over a retained CFString).
pub(crate) fn string_property(object: AudioObjectID, selector: u32) -> Option<String> {
    let string = property::<*const CFString>(object, selector, kAudioObjectPropertyScopeGlobal)?;
    let string = unsafe { CFRetained::from_raw(NonNull::new(string.cast_mut())?) };
    Some(string.to_string())
}

/// Audio playing on an output device. Dropping it stops and closes the
/// unit.
pub struct Playback {
    unit: AudioUnit,
    /// What the unit's callback calls; boxed so its address stays put.
    _renderer: Box<Renderer>,
}

// SAFETY: the unit is only started on creation and stopped and disposed
// on drop, which Core Audio allows from any thread.
unsafe impl Send for Playback {}

impl Drop for Playback {
    fn drop(&mut self) {
        unsafe {
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
        }
    }
}

struct Renderer {
    render: AudioRender,
}

/// Start playing on `device` ("default" for the system output), pulling
/// samples from `render`. Returns the playback and the device's name.
pub fn play_audio(device: &str, render: AudioRender) -> Result<(Playback, String)> {
    let (subtype, target) = match device {
        "default" => (kAudioUnitSubType_DefaultOutput, None),
        query => (kAudioUnitSubType_HALOutput, Some(find_output(query)?)),
    };
    let description = AudioComponentDescription {
        componentType: kAudioUnitType_Output,
        componentSubType: subtype,
        componentManufacturer: kAudioUnitManufacturer_Apple,
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    let component = unsafe { AudioComponentFindNext(ptr::null_mut(), NonNull::from(&description)) };
    if component.is_null() {
        bail!("Core Audio has no output unit");
    }
    let mut unit: AudioUnit = ptr::null_mut();
    check(
        unsafe { AudioComponentInstanceNew(component, NonNull::from(&mut unit)) },
        "create an output unit",
    )?;
    let mut playback = Playback {
        unit,
        _renderer: Box::new(Renderer { render }),
    };
    let name = match target {
        Some((device, name)) => {
            set_property(
                unit,
                kAudioOutputUnitProperty_CurrentDevice,
                kAudioUnitScope_Global,
                &device,
            )
            .map_err(|err| anyhow!("could not use {name:?}: {err}"))?;
            name
        }
        None => default_output().map_or_else(|| "the system output".into(), device_name),
    };
    let format = AudioStreamBasicDescription {
        mSampleRate: f64::from(SAMPLE_RATE),
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        mBytesPerPacket: 8,
        mFramesPerPacket: 1,
        mBytesPerFrame: 8,
        mChannelsPerFrame: 2,
        mBitsPerChannel: 32,
        mReserved: 0,
    };
    set_property(
        unit,
        kAudioUnitProperty_StreamFormat,
        kAudioUnitScope_Input,
        &format,
    )?;
    let callback = AURenderCallbackStruct {
        inputProc: Some(render_callback),
        inputProcRefCon: (&mut *playback._renderer as *mut Renderer).cast(),
    };
    set_property(
        unit,
        kAudioUnitProperty_SetRenderCallback,
        kAudioUnitScope_Input,
        &callback,
    )?;
    check(
        unsafe { AudioUnitInitialize(unit) },
        "initialize the output unit",
    )?;
    check(
        unsafe { AudioOutputUnitStart(unit) },
        "start the output unit",
    )?;
    Ok((playback, name))
}

fn set_property<T>(unit: AudioUnit, property: u32, scope: u32, value: &T) -> Result<()> {
    let status = unsafe {
        AudioUnitSetProperty(
            unit,
            property,
            scope,
            0,
            (value as *const T).cast::<c_void>(),
            size_of::<T>() as u32,
        )
    };
    check(status, &format!("set output unit property {property}"))
}

fn check(status: i32, what: &str) -> Result<()> {
    match status {
        0 => Ok(()),
        status => Err(anyhow!("could not {what} (Core Audio error {status})")),
    }
}

/// Called by the output unit on its real-time thread for each buffer it
/// needs: interleaved stereo floats, in one buffer.
unsafe extern "C-unwind" fn render_callback(
    refcon: NonNull<c_void>,
    _flags: NonNull<AudioUnitRenderActionFlags>,
    _time: NonNull<AudioTimeStamp>,
    _bus: u32,
    _frames: u32,
    data: *mut AudioBufferList,
) -> i32 {
    // SAFETY: the refcon is the playback's boxed renderer, which outlives
    // the unit, and only this callback uses it.
    let renderer = unsafe { refcon.cast::<Renderer>().as_mut() };
    let Some(list) = (unsafe { data.as_mut() }) else {
        return 0;
    };
    if list.mNumberBuffers == 0 || list.mBuffers[0].mData.is_null() {
        return 0;
    }
    let buffer = &mut list.mBuffers[0];
    let samples = buffer.mDataByteSize as usize / size_of::<f32>();
    let out = unsafe { slice::from_raw_parts_mut(buffer.mData.cast::<f32>(), samples) };
    // A panic must not unwind into Core Audio; play silence instead.
    if catch_unwind(AssertUnwindSafe(|| (renderer.render)(out))).is_err() {
        out.fill(0.0);
    }
    0
}
