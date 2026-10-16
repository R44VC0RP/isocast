//! What this Mac is, and which devices a setup's sources resolve to.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::ptr;

use anyhow::Result;
use isocast_engine::{CameraRequest, CaptureFormat, Probe};
use isocast_protocol::api::Machine;
use isocast_protocol::{Canvas, SourceKind};
use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFRetained, CFString, CFType};
use objc2_video_toolbox::{
    VTCopyVideoEncoderList, kVTVideoEncoderList_DisplayName,
    kVTVideoEncoderList_IsHardwareAccelerated,
};

use crate::camera::choose_format;
use crate::capture::native_size;
use crate::devices::{
    allow_device_screens, find_camera, find_display, find_microphone, is_device_screen,
};

pub fn machine() -> Machine {
    Machine {
        chip: sysctl_string("machdep.cpu.brand_string").unwrap_or_else(|| "unknown".into()),
        memory_gb: sysctl_u64("hw.memsize").unwrap_or(0) as f64 / (1u64 << 30) as f64,
        cores: sysctl_u64("hw.ncpu").unwrap_or(0) as u32,
        os: format!(
            "macOS {}",
            sysctl_string("kern.osproductversion").unwrap_or_default()
        ),
        hardware_encoders: hardware_encoders(),
    }
}

/// Names of the video encoders that run on the media engines.
fn hardware_encoders() -> Vec<String> {
    let mut list = ptr::null();
    let status = unsafe { VTCopyVideoEncoderList(None, ptr::NonNull::from(&mut list)) };
    let Some(list) = ptr::NonNull::new(list.cast_mut()).filter(|_| status == 0) else {
        return Vec::new();
    };
    // SAFETY: the copy rule; an array of dictionaries.
    let list: CFRetained<CFArray> = unsafe { CFRetained::from_raw(list) };
    let list: &CFArray<CFDictionary<CFString, CFType>> = unsafe { list.cast_unchecked() };
    let mut names = BTreeSet::new();
    for encoder in list.iter() {
        let hardware = encoder
            .get(unsafe { kVTVideoEncoderList_IsHardwareAccelerated })
            .and_then(|value| value.downcast::<CFBoolean>().ok())
            .is_some_and(|value| value.as_bool());
        let name = encoder
            .get(unsafe { kVTVideoEncoderList_DisplayName })
            .and_then(|value| value.downcast::<CFString>().ok());
        if let (true, Some(name)) = (hardware, name) {
            names.insert(name.to_string());
        }
    }
    names.into_iter().collect()
}

pub(crate) fn sysctl_string(name: &str) -> Option<String> {
    let name = CString::new(name).ok()?;
    let mut size = 0;
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            ptr::null_mut(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    let mut buffer = vec![0u8; size];
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buffer.truncate(size);
    let text = String::from_utf8_lossy(&buffer);
    Some(text.trim_end_matches('\0').trim().to_string())
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let name = CString::new(name).ok()?;
    let mut value = 0u64;
    let mut size = size_of::<u64>();
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (&raw mut value).cast(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    // hw.ncpu is a 32-bit value; the high bytes stay zero.
    (status == 0).then_some(value)
}

/// The device a source resolves to and what it would deliver, without
/// opening it.
pub fn probe(source: &SourceKind, canvas: &Canvas) -> Result<Option<Probe>> {
    Ok(match source {
        SourceKind::Display(display) => {
            let (id, info) = find_display(&display.display)?;
            let (width, height) = native_size(id).unwrap_or((info.width, info.height));
            Some(Probe {
                device: info.name,
                video: Some(CaptureFormat {
                    width,
                    height,
                    fps: canvas.fps,
                }),
                channels: None,
            })
        }
        SourceKind::Camera(settings) => {
            allow_device_screens();
            let camera = find_camera(&settings.device)?;
            let name = unsafe { camera.localizedName() }.to_string();
            // An iPhone or iPad screen's size is only known once it streams.
            if is_device_screen(&camera) {
                return Ok(Some(Probe {
                    device: name,
                    video: None,
                    channels: None,
                }));
            }
            let request = CameraRequest {
                size: settings.width.zip(settings.height),
                fps: settings.fps,
            };
            let choice = choose_format(&camera, request, canvas)?;
            Some(Probe {
                device: name,
                video: Some(CaptureFormat {
                    width: choice.width,
                    height: choice.height,
                    fps: choice.fps,
                }),
                channels: None,
            })
        }
        SourceKind::Microphone(settings) => {
            let microphone = find_microphone(&settings.device)?;
            Some(Probe {
                device: unsafe { microphone.localizedName() }.to_string(),
                video: None,
                channels: Some(crate::audio::microphone_channels(&microphone)),
            })
        }
        SourceKind::Window(window) => {
            let content = crate::devices::shareable_content()?;
            let found =
                crate::capture::find_window(&content, &window.app, window.title.as_deref())?;
            let frame = unsafe { found.frame() };
            let name =
                unsafe { found.title() }.map_or_else(|| window.app.clone(), |t| t.to_string());
            Some(Probe {
                device: name,
                // Points; the capture is at the window's pixel size.
                video: Some(CaptureFormat {
                    width: frame.size.width as u32,
                    height: frame.size.height as u32,
                    fps: canvas.fps,
                }),
                channels: None,
            })
        }
        SourceKind::Application(application) => {
            let content = crate::devices::shareable_content()?;
            crate::capture::find_applications(&content, &application.app)?;
            let (id, info) = find_display(&application.display)?;
            let (width, height) = native_size(id).unwrap_or((info.width, info.height));
            Some(Probe {
                device: format!("{} on {}", application.app, info.name),
                video: Some(CaptureFormat {
                    width,
                    height,
                    fps: canvas.fps,
                }),
                channels: None,
            })
        }
        SourceKind::ApplicationAudio(application) => {
            let device = match crate::tap::backend() {
                crate::tap::Backend::ProcessTap => crate::tap::check_app(&application.app)?,
                crate::tap::Backend::ScreenCaptureKit => {
                    let content = crate::devices::shareable_content()?;
                    crate::capture::find_applications(&content, &application.app)?;
                    application.app.clone()
                }
            };
            Some(Probe {
                device,
                video: None,
                channels: Some(2),
            })
        }
        SourceKind::SystemAudio(_) => Some(Probe {
            device: "system audio".into(),
            video: None,
            channels: Some(2),
        }),
        _ => None,
    })
}
