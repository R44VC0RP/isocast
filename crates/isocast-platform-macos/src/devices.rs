//! Listing displays, windows, cameras and microphones (audio outputs are
//! listed in `output`).

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Once, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use block2::RcBlock;
use isocast_protocol::devices::{
    Camera, CameraKind, Devices, Display, FrameRateRange, Microphone, Permission, PermissionKind,
    Permissions, VideoFormat, Window,
};
use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2::{MainThreadMarker, Message};
use objc2_app_kit::NSScreen;
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureDevice, AVCaptureDeviceDiscoverySession,
    AVCaptureDevicePosition, AVCaptureDeviceType, AVCaptureDeviceTypeBuiltInWideAngleCamera,
    AVCaptureDeviceTypeContinuityCamera, AVCaptureDeviceTypeDeskViewCamera,
    AVCaptureDeviceTypeExternal, AVCaptureDeviceTypeMicrophone, AVMediaType, AVMediaTypeAudio,
    AVMediaTypeMuxed, AVMediaTypeVideo,
};
use objc2_core_foundation::{CFRetained, CFUUID};
use objc2_core_graphics::{
    CGDirectDisplayID, CGDisplayCopyDisplayMode, CGDisplayIsBuiltin, CGDisplayIsMain,
    CGDisplayMode, CGGetActiveDisplayList, CGPreflightScreenCaptureAccess,
    CGRequestScreenCaptureAccess,
};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMVideoFormatDescriptionGetDimensions,
};
use objc2_foundation::{NSArray, NSError, NSNumber, NSString};
use objc2_screen_capture_kit::{SCShareableContent, SCWindow};

/// How long to wait for the main thread to answer.
const MAIN_QUEUE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to wait for ScreenCaptureKit to list what it can capture.
const CONTENT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for a device that isn't listed yet. iPhone and iPad
/// screens appear a moment after they are allowed.
const DEVICE_WAIT: Duration = Duration::from_secs(3);

#[repr(C)]
struct CmioPropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "CoreMediaIO", kind = "framework")]
unsafe extern "C" {
    fn CMIOObjectSetPropertyData(
        object: u32,
        address: *const CmioPropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: u32,
        data: *const c_void,
    ) -> i32;
}

/// Let iPhone and iPad screens connected over USB show up as capture
/// devices, as QuickTime does. They are hidden from every process until it
/// opts in.
pub(crate) fn allow_device_screens() {
    static ALLOW: Once = Once::new();
    ALLOW.call_once(|| {
        let address = CmioPropertyAddress {
            selector: u32::from_be_bytes(*b"yes "), // AllowScreenCaptureDevices
            scope: u32::from_be_bytes(*b"glob"),
            element: 0,
        };
        let allow: u32 = 1;
        // The system object; failure only means screens stay hidden.
        unsafe {
            CMIOObjectSetPropertyData(
                1,
                &address,
                0,
                std::ptr::null(),
                4,
                (&raw const allow).cast(),
            )
        };
    });
}

#[link(name = "ColorSync", kind = "framework")]
unsafe extern "C" {
    /// Returns a +1 `CFUUID` that stays the same for a display across
    /// reboots and reconnects, or null.
    fn CGDisplayCreateUUIDFromDisplayID(display: CGDirectDisplayID) -> *mut CFUUID;
}

/// The privacy permissions as they stand, without asking.
pub(crate) fn permissions() -> Result<Permissions> {
    Ok(Permissions {
        screen_recording: if CGPreflightScreenCaptureAccess() {
            Permission::Granted
        } else {
            Permission::NotGranted
        },
        camera: authorization(unsafe { AVMediaTypeVideo })?,
        microphone: authorization(unsafe { AVMediaTypeAudio })?,
        system_audio: crate::tap::permission(),
    })
}

pub fn devices() -> Result<Devices> {
    let permissions = permissions()?;
    let screen_recording = permissions.screen_recording;

    // What can't be recorded yet, and what to do about it.
    let mut notes: Vec<String> = crate::permissions::report(permissions.clone())
        .details
        .into_iter()
        .filter_map(|detail| {
            let step = detail.next_step?;
            let windows = if detail.permission == PermissionKind::ScreenRecording {
                " (and windows aren't listed)"
            } else {
                ""
            };
            Some(format!(
                "{} permission is {}, so {} can't be recorded{windows}; {step}",
                detail.permission.label(),
                detail.state.label(),
                detail.needed_for
            ))
        })
        .collect();
    let windows = if screen_recording == Permission::Granted {
        windows()?
    } else {
        Vec::new()
    };

    let audio_outputs = crate::output::audio_outputs().unwrap_or_else(|err| {
        notes.push(format!("audio outputs aren't listed: {err:#}"));
        Vec::new()
    });

    let audio_apps = match crate::tap::backend() {
        crate::tap::Backend::ProcessTap => crate::tap::audio_apps(),
        crate::tap::Backend::ScreenCaptureKit => Vec::new(),
    };

    Ok(Devices {
        displays: displays()?.into_iter().map(|(_, d)| d).collect(),
        windows,
        cameras: cameras()?,
        microphones: microphones()?,
        audio_outputs,
        audio_apps,
        permissions,
        notes,
    })
}

/// Diagnostics inventory: never ask ScreenCaptureKit to enumerate windows,
/// which can trigger a picker-consent prompt. Device formats need no capture.
pub(crate) fn inventory() -> Result<Devices> {
    Ok(Devices {
        displays: displays()?.into_iter().map(|(_, d)| d).collect(),
        windows: Vec::new(),
        cameras: cameras()?,
        microphones: microphones()?,
        audio_outputs: crate::output::audio_outputs()?,
        // Like window titles, which apps run is private.
        audio_apps: Vec::new(),
        permissions: permissions()?,
        notes: vec!["Window titles deliberately not collected by diagnostics".into()],
    })
}

/// Ask macOS for screen recording, camera and microphone access where it
/// hasn't been decided (only `only`, if given), showing its prompts, and
/// wait up to `timeout` for the answers. Returns the permissions afterwards.
pub(crate) fn request_permissions(
    timeout: Duration,
    only: Option<PermissionKind>,
) -> Result<Permissions> {
    let wanted = |kind| only.is_none_or(|only| only == kind);
    // Opens the prompt (or System Settings) the first time; no answer to
    // wait for, since macOS applies it to the next launch.
    if wanted(PermissionKind::ScreenRecording) && !CGPreflightScreenCaptureAccess() {
        CGRequestScreenCaptureAccess();
    }
    let (tx, rx) = mpsc::channel();
    let mut asked = 0;
    let media = [
        (PermissionKind::Camera, unsafe { AVMediaTypeVideo }),
        (PermissionKind::Microphone, unsafe { AVMediaTypeAudio }),
    ];
    for (kind, media_type) in media {
        if !wanted(kind) || authorization(media_type)? != Permission::NotDetermined {
            continue;
        }
        let tx = tx.clone();
        let asked_at = Instant::now();
        let handler = RcBlock::new(move |granted: Bool| {
            // Refused before a prompt could even be read: macOS showed none.
            crate::permissions::answered(kind, granted.as_bool(), asked_at.elapsed());
            let _ = tx.send(());
        });
        let media_type = media_type.context("AVFoundation media type constant is missing")?;
        unsafe {
            AVCaptureDevice::requestAccessForMediaType_completionHandler(media_type, &handler)
        };
        asked += 1;
    }
    if wanted(PermissionKind::SystemAudio) && crate::tap::permission() == Permission::NotDetermined
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let asked_at = Instant::now();
            let state = crate::tap::request_permission(timeout);
            crate::permissions::answered(
                PermissionKind::SystemAudio,
                state == Permission::Granted,
                asked_at.elapsed(),
            );
            let _ = tx.send(());
        });
        asked += 1;
    }
    let deadline = Instant::now() + timeout;
    for _ in 0..asked {
        let left = deadline.saturating_duration_since(Instant::now());
        if rx.recv_timeout(left).is_err() {
            break;
        }
    }
    permissions()
}

fn authorization(media_type: Option<&'static AVMediaType>) -> Result<Permission> {
    let media_type = media_type.context("AVFoundation media type constant is missing")?;
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media_type) };
    Ok(match status {
        AVAuthorizationStatus::Authorized => Permission::Granted,
        AVAuthorizationStatus::Denied => Permission::Denied,
        AVAuthorizationStatus::Restricted => Permission::Restricted,
        _ => Permission::NotDetermined,
    })
}

/// Active displays with their CoreGraphics ids, main display first.
fn displays() -> Result<Vec<(CGDirectDisplayID, Display)>> {
    let mut ids = [0; 32];
    let mut count = 0;
    let status = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if status.0 != 0 {
        anyhow::bail!("could not list displays (CoreGraphics error {})", status.0);
    }
    let names = display_names();
    let mut displays: Vec<(CGDirectDisplayID, Display)> = ids[..count as usize]
        .iter()
        .map(|&id| {
            let mode = CGDisplayCopyDisplayMode(id);
            let mode = mode.as_deref();
            let display = Display {
                id: display_uuid(id).unwrap_or_else(|| format!("display-{id}")),
                name: names
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| format!("Display {id}")),
                main: CGDisplayIsMain(id),
                built_in: CGDisplayIsBuiltin(id),
                width: CGDisplayMode::pixel_width(mode) as u32,
                height: CGDisplayMode::pixel_height(mode) as u32,
                refresh_rate: CGDisplayMode::refresh_rate(mode),
            };
            (id, display)
        })
        .collect();
    displays.sort_by_key(|(_, d)| !d.main);
    Ok(displays)
}

/// Find a display by "main", id, or name (exact, then ignoring case).
pub(crate) fn find_display(query: &str) -> Result<(CGDirectDisplayID, Display)> {
    let displays = displays()?;
    let found = displays
        .iter()
        .find(|(_, d)| (query == "main" && d.main) || d.id == query || d.name == query)
        .or_else(|| {
            displays
                .iter()
                .find(|(_, d)| d.name.eq_ignore_ascii_case(query))
        });
    match found {
        Some(found) => Ok(found.clone()),
        None => {
            let valid: Vec<String> = displays
                .iter()
                .map(|(_, d)| format!("{:?} (id {})", d.name, d.id))
                .collect();
            anyhow::bail!(
                "no display matches {query:?}; valid: \"main\", {}",
                valid.join(", ")
            )
        }
    }
}

pub(crate) fn display_uuid(id: CGDirectDisplayID) -> Option<String> {
    let uuid = NonNull::new(unsafe { CGDisplayCreateUUIDFromDisplayID(id) })?;
    // SAFETY: the function follows the create rule, so we own this reference.
    let uuid: CFRetained<CFUUID> = unsafe { CFRetained::from_raw(uuid) };
    let string = CFUUID::new_string(None, Some(&uuid))?;
    Some(string.to_string())
}

/// Display names from AppKit, by display id. AppKit only answers on the
/// main thread, so off it the question goes there (a daemon serves it; see
/// `run_main_queue`). If nobody answers, displays get generic names.
fn display_names() -> BTreeMap<CGDirectDisplayID, String> {
    crate::on_main_thread(MAIN_QUEUE_TIMEOUT, main_thread_display_names).unwrap_or_default()
}

fn main_thread_display_names() -> BTreeMap<CGDirectDisplayID, String> {
    let Some(mtm) = MainThreadMarker::new() else {
        return BTreeMap::new();
    };
    let key = NSString::from_str("NSScreenNumber");
    NSScreen::screens(mtm)
        .iter()
        .filter_map(|screen| {
            let number = screen.deviceDescription().objectForKey(&key)?;
            let number = number.downcast::<NSNumber>().ok()?;
            Some((number.as_u32(), screen.localizedName().to_string()))
        })
        .collect()
}

fn windows() -> Result<Vec<Window>> {
    Ok(list_windows(&*shareable_content()?))
}

/// What ScreenCaptureKit can capture: on-screen windows and displays.
/// Needs screen recording permission.
pub(crate) fn shareable_content() -> Result<Retained<SCShareableContent>> {
    /// The content object is immutable, so it can cross to our thread.
    struct Content(Retained<SCShareableContent>);
    unsafe impl Send for Content {}

    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let result = match unsafe { content.as_ref() } {
                Some(content) => Ok(Content(content.retain())),
                None => Err(unsafe { error.as_ref() }
                    .map(|e| e.localizedDescription().to_string())
                    .unwrap_or_else(|| "unknown error".into())),
            };
            let _ = tx.send(result);
        },
    );
    unsafe {
        SCShareableContent::getShareableContentExcludingDesktopWindows_onScreenWindowsOnly_completionHandler(
            true, true, &handler,
        )
    };
    let content = rx
        .recv_timeout(CONTENT_TIMEOUT)
        .context("ScreenCaptureKit did not answer in time")?
        .map_err(|e| anyhow::anyhow!("ScreenCaptureKit: {e}"))?;
    Ok(content.0)
}

fn list_windows(content: &SCShareableContent) -> Vec<Window> {
    let mut windows: Vec<Window> = unsafe { content.windows() }
        .iter()
        // Layer 0 holds normal app windows; menus, the Dock and overlays
        // live on other layers.
        .filter(|w| unsafe { w.windowLayer() } == 0 && !is_ours(w))
        .filter_map(|w| {
            let app = unsafe { w.owningApplication() }?;
            let frame = unsafe { w.frame() };
            Some(Window {
                id: unsafe { w.windowID() },
                app: unsafe { app.applicationName() }.to_string(),
                bundle_id: unsafe { app.bundleIdentifier() }.to_string(),
                title: unsafe { w.title() }
                    .map(|t| t.to_string())
                    .filter(|t| !t.is_empty()),
                width: frame.size.width as u32,
                height: frame.size.height as u32,
            })
        })
        .collect();
    windows.sort_by(|a, b| (&a.app, &a.title).cmp(&(&b.app, &b.title)));
    windows
}

/// Whether `window` is this process's own, like a browser source's hidden
/// page, which is not something to capture.
pub(crate) fn is_ours(window: &SCWindow) -> bool {
    unsafe { window.owningApplication() }
        .is_some_and(|app| unsafe { app.processID() } as u32 == std::process::id())
}

/// Every camera-like device: cameras, capture cards, Continuity Camera,
/// Desk View, and iPhone or iPad screens (which carry video and audio
/// together, so they are "muxed" rather than "video" devices).
fn camera_devices() -> Vec<Retained<AVCaptureDevice>> {
    allow_device_screens();
    let types = unsafe {
        [
            AVCaptureDeviceTypeBuiltInWideAngleCamera,
            AVCaptureDeviceTypeExternal,
            AVCaptureDeviceTypeContinuityCamera,
            AVCaptureDeviceTypeDeskViewCamera,
        ]
    };
    discover(&types, None)
        .iter()
        .filter(|device| {
            [unsafe { AVMediaTypeVideo }, unsafe { AVMediaTypeMuxed }]
                .into_iter()
                .flatten()
                .any(|media| unsafe { device.hasMediaType(media) })
        })
        .collect()
}

pub(crate) fn is_device_screen(device: &AVCaptureDevice) -> bool {
    unsafe { AVMediaTypeMuxed }.is_some_and(|muxed| unsafe { device.hasMediaType(muxed) })
}

/// Find a camera by id, name, or part of a name; the shortest name that
/// contains `query` wins, so "iPhone" picks "Ryan iPhone Camera" over
/// "Ryan iPhone Desk View Camera". Waits briefly for devices that appear
/// late.
pub(crate) fn find_camera(query: &str) -> Result<Retained<AVCaptureDevice>> {
    let deadline = Instant::now() + DEVICE_WAIT;
    loop {
        let devices = camera_devices();
        if let Some(device) = match_device(&devices, query) {
            return Ok(device);
        }
        if Instant::now() >= deadline {
            let names: Vec<String> = devices
                .iter()
                .map(|d| {
                    let (id, name, ..) = identity(d);
                    format!("{name:?} (id {id})")
                })
                .collect();
            anyhow::bail!(
                "no camera matches {query:?}; cameras: {}",
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn match_device(
    devices: &[Retained<AVCaptureDevice>],
    query: &str,
) -> Option<Retained<AVCaptureDevice>> {
    let lower = query.to_lowercase();
    let named: Vec<(String, String)> = devices
        .iter()
        .map(|d| {
            let (id, name, ..) = identity(d);
            (id, name)
        })
        .collect();
    let index = named
        .iter()
        .position(|(id, name)| id == query || name == query)
        .or_else(|| {
            named
                .iter()
                .position(|(_, name)| name.to_lowercase() == lower)
        })
        .or_else(|| {
            named
                .iter()
                .enumerate()
                .filter(|(_, (_, name))| name.to_lowercase().contains(&lower))
                .min_by_key(|(_, (_, name))| name.len())
                .map(|(i, _)| i)
        })?;
    Some(devices[index].clone())
}

fn cameras() -> Result<Vec<Camera>> {
    let devices = camera_devices();
    Ok(devices
        .iter()
        .map(|device| {
            let device_type = unsafe { device.deviceType() };
            let is = |t: &AVCaptureDeviceType| *t == *device_type;
            let kind = unsafe {
                if is_device_screen(device) {
                    CameraKind::DeviceScreen
                } else if device.isContinuityCamera() || is(AVCaptureDeviceTypeContinuityCamera) {
                    CameraKind::Continuity
                } else if is(AVCaptureDeviceTypeDeskViewCamera) {
                    CameraKind::DeskView
                } else if is_virtual(device) {
                    CameraKind::Virtual
                } else if is(AVCaptureDeviceTypeBuiltInWideAngleCamera) {
                    CameraKind::BuiltIn
                } else if is(AVCaptureDeviceTypeExternal) {
                    CameraKind::External
                } else {
                    CameraKind::Other
                }
            };
            let (id, name, model, manufacturer) = identity(device);
            Camera {
                id,
                name,
                model,
                manufacturer,
                kind,
                formats: video_formats(device),
                suspended: unsafe { device.isSuspended() },
            }
        })
        .collect())
}

/// Whether a camera is made by an app (OBS Virtual Camera, NDI, …) rather
/// than hardware: CoreMediaIO extensions report the virtual transport;
/// older plug-ins say so in their names.
fn is_virtual(device: &AVCaptureDevice) -> bool {
    let transport = unsafe { device.transportType() } as u32;
    if transport == u32::from_be_bytes(*b"virt") {
        return true;
    }
    let (_, name, model, _) = identity(device);
    [name, model]
        .iter()
        .any(|text| text.to_lowercase().contains("virtual"))
}

/// The device's formats, merged by size and pixel format, largest first.
fn video_formats(device: &AVCaptureDevice) -> Vec<VideoFormat> {
    let mut merged: BTreeMap<(u32, u32, String), Vec<FrameRateRange>> = BTreeMap::new();
    for format in unsafe { device.formats() }.iter() {
        let description = unsafe { format.formatDescription() };
        let size = unsafe { CMVideoFormatDescriptionGetDimensions(&description) };
        let pixel_format = fourcc(unsafe { description.media_sub_type() });
        let ranges = merged
            .entry((size.width as u32, size.height as u32, pixel_format))
            .or_default();
        for range in unsafe { format.videoSupportedFrameRateRanges() }.iter() {
            let round = |rate: f64| (rate * 1000.0).round() / 1000.0;
            let range = unsafe {
                FrameRateRange {
                    min: round(range.minFrameRate()),
                    max: round(range.maxFrameRate()),
                }
            };
            if !ranges.contains(&range) {
                ranges.push(range);
            }
        }
    }
    let mut formats: Vec<VideoFormat> = merged
        .into_iter()
        .map(|((width, height, pixel_format), mut frame_rate_ranges)| {
            frame_rate_ranges.sort_by(|a, b| b.max.total_cmp(&a.max).then(b.min.total_cmp(&a.min)));
            let mut frame_rates: Vec<f64> = frame_rate_ranges.iter().map(|r| r.max).collect();
            frame_rates.sort_by(f64::total_cmp);
            frame_rates.dedup();
            VideoFormat {
                width,
                height,
                pixel_format,
                frame_rates,
                frame_rate_ranges,
            }
        })
        .collect();
    formats.sort_by_key(|f| std::cmp::Reverse((f.width * f.height, f.width)));
    formats
}

fn microphone_devices() -> Vec<Retained<AVCaptureDevice>> {
    discover(&[unsafe { AVCaptureDeviceTypeMicrophone }], unsafe {
        AVMediaTypeAudio
    })
    .iter()
    // A running system or app audio capture's own device (tap.rs) isn't a
    // microphone to offer or to announce as connected.
    .filter(|d| !crate::tap::is_ours(&unsafe { d.uniqueID() }.to_string()))
    .collect()
}

/// Find a microphone by "default", id, name, or part of a name.
pub(crate) fn find_microphone(query: &str) -> Result<Retained<AVCaptureDevice>> {
    if query == "default" {
        let audio = unsafe { AVMediaTypeAudio }.context("missing AVMediaTypeAudio")?;
        return unsafe { AVCaptureDevice::defaultDeviceWithMediaType(audio) }
            .context("this Mac has no default microphone");
    }
    let devices = microphone_devices();
    match_device(&devices, query).ok_or_else(|| {
        let names: Vec<String> = devices
            .iter()
            .map(|d| {
                let (id, name, ..) = identity(d);
                format!("{name:?} (id {id})")
            })
            .collect();
        anyhow::anyhow!(
            "no microphone matches {query:?}; valid: \"default\", {}",
            names.join(", ")
        )
    })
}

fn microphones() -> Result<Vec<Microphone>> {
    let audio = unsafe { AVMediaTypeAudio };
    let devices = microphone_devices();
    let default_id = audio
        .and_then(|audio| unsafe { AVCaptureDevice::defaultDeviceWithMediaType(audio) })
        .map(|d| unsafe { d.uniqueID() }.to_string());
    Ok(devices
        .iter()
        .map(|device| {
            let mut sample_rates = Vec::new();
            let mut channels = 0;
            for format in unsafe { device.formats() }.iter() {
                let description = unsafe { format.formatDescription() };
                let asbd =
                    unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&description) };
                if let Some(asbd) = unsafe { asbd.as_ref() } {
                    if !sample_rates.contains(&asbd.mSampleRate) {
                        sample_rates.push(asbd.mSampleRate);
                    }
                    channels = channels.max(asbd.mChannelsPerFrame);
                }
            }
            sample_rates.sort_by(f64::total_cmp);
            let (id, name, model, manufacturer) = identity(device);
            Microphone {
                default: default_id.as_deref() == Some(id.as_str()),
                id,
                name,
                model,
                manufacturer,
                sample_rates,
                channels,
            }
        })
        .collect())
}

fn discover(
    types: &[&AVCaptureDeviceType],
    media_type: Option<&AVMediaType>,
) -> Retained<NSArray<AVCaptureDevice>> {
    unsafe {
        AVCaptureDeviceDiscoverySession::discoverySessionWithDeviceTypes_mediaType_position(
            &NSArray::from_slice(types),
            media_type,
            AVCaptureDevicePosition::Unspecified,
        )
        .devices()
    }
}

fn identity(device: &AVCaptureDevice) -> (String, String, String, String) {
    unsafe {
        (
            device.uniqueID().to_string(),
            device.localizedName().to_string(),
            device.modelID().to_string(),
            device.manufacturer().to_string(),
        )
    }
}

/// A four-character code as text, like `420v`.
fn fourcc(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("{code:#010x}")
    }
}
