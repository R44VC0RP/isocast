//! Displays, windows, cameras and microphones for `isocast devices`, and
//! screen target resolution. Read-only: nothing here captures, prompts, opens
//! a GPU device, writes settings or changes the process's DPI awareness.
//! Camera listing activates each source to read its native modes but never
//! reads a sample or changes another app's format.
//!
//! Display sizes come from the DisplayConfig source mode and window sizes
//! from DWM's extended frame bounds; both are physical pixels whatever the
//! caller's DPI awareness, so no thread DPI context is switched.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use isocast_engine::{CaptureFormat, Probe, ScreenTarget};
use isocast_protocol::devices::{
    AudioOutput, Camera, CameraKind, Devices, Display, FrameRateRange, Microphone, VideoFormat,
    Window,
};
use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_SOURCE_MODE,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
    QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowThreadProcessId,
    InternalGetWindowText, IsIconic, IsWindowVisible, MONITORINFOF_PRIMARY, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, Owned, PWSTR};

use crate::CaptureTarget;
use crate::camera::{CameraDevice, camera_devices};

/// Bounds on everything read across the FFI boundary.
const MAX_ENTRIES: usize = 4096;
const MAX_TEXT: usize = 32_768;
/// Bound on the options listed in one error message.
const MAX_CHOICES: usize = 40;

struct DisplayInfo {
    display: Display,
    /// HMONITOR: valid only for this desktop configuration, never an id.
    monitor: usize,
}

struct WindowInfo {
    /// HWND: valid only while the window exists, never persisted.
    hwnd: usize,
    /// Executable file name without `.exe`.
    app: String,
    /// Full executable path.
    path: String,
    title: Option<String>,
    width: u32,
    height: u32,
}

impl WindowInfo {
    fn label(&self) -> String {
        match &self.title {
            Some(title) => format!("{} — {title}", self.app),
            None => format!("{} (untitled)", self.app),
        }
    }
}

/// HWND enumeration: the callback only pushes into preallocated capacity,
/// so it neither allocates nor panics across the FFI boundary.
struct Collected {
    items: Vec<usize>,
    truncated: bool,
}

impl Collected {
    fn new() -> Self {
        Self {
            items: Vec::with_capacity(MAX_ENTRIES),
            truncated: false,
        }
    }

    fn push(&mut self, handle: usize) -> BOOL {
        if self.items.len() >= MAX_ENTRIES {
            self.truncated = true;
            return false.into();
        }
        self.items.push(handle);
        true.into()
    }
}

unsafe extern "system" fn collect_window(hwnd: HWND, data: LPARAM) -> BOOL {
    // SAFETY: `data` is the &mut Collected passed to EnumWindows below.
    unsafe { &mut *(data.0 as *mut Collected) }.push(hwnd.0 as usize)
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: `data` is the &mut Collected passed to EnumDisplayMonitors.
    unsafe { &mut *(data.0 as *mut Collected) }.push(monitor.0 as usize)
}

pub(crate) fn devices() -> Result<Devices> {
    let mut notes = vec![
        "Permissions are unknown: Windows has no documented authoritative query-only consent \
         check for this unpackaged desktop app. Unknown means neither denied nor not asked yet. \
         Windows Camera / Microphone privacy settings can block desktop apps."
            .to_string(),
    ];
    let displays = displays(&mut notes).unwrap_or_else(|error| {
        notes.push(format!("Displays could not be listed: {error:#}"));
        Vec::new()
    });
    if displays.is_empty() {
        notes.push(
            "No displays are listed. Services run in session 0 without an interactive desktop; \
             run Isocast in the signed-in user's session."
                .into(),
        );
    }
    let windows = windows(&mut notes).unwrap_or_else(|error| {
        notes.push(format!("Windows could not be listed: {error:#}"));
        Vec::new()
    });
    if windows.is_empty() {
        notes.push(
            "No windows are listed: none qualify, or this process has no interactive desktop \
             (session 0)."
                .into(),
        );
    }
    let mut unaddressable = 0;
    let windows = windows
        .into_iter()
        .filter_map(|window| {
            // The protocol id is u32: list only handles that round-trip.
            let Ok(id) = u32::try_from(window.hwnd) else {
                unaddressable += 1;
                return None;
            };
            Some(Window {
                id,
                app: window.app,
                bundle_id: window.path,
                title: window.title,
                width: window.width,
                height: window.height,
            })
        })
        .collect();
    if unaddressable > 0 {
        notes.push(format!(
            "{unaddressable} window(s) left out: their handle does not fit a u32 id. Setups can \
             still match them by app and title."
        ));
    }
    notes.push(
        "Windows are visible, uncloaked, unowned, non-tool, non-minimized top-level windows. \
         id is the HWND, valid only while the window exists; bundle_id is the full executable \
         path (Windows has no bundle ids); sizes are physical pixels. A listed window may still \
         refuse Windows Graphics Capture."
            .into(),
    );
    let cameras = camera_devices()
        .map(|devices| devices.into_iter().map(camera).collect())
        .unwrap_or_else(|error| {
            notes.push(format!("Cameras could not be listed: {error:#}"));
            Vec::new()
        });
    notes.push(
        "Camera id is the Media Foundation symbolic link. formats lists supported NV12, \
         YUY2/UYVY, RGB24/RGB32/ARGB32 and MJPG modes. At matching size/rate, native NV12 \
         wins, then other uncompressed formats, then MJPEG. CPU pixels use one GPU upload; \
         colour conversion is on the GPU and MJPEG uses Media Foundation decoding. \
         Missing SDR colour metadata defaults to limited BT.709 for HD or BT.601 for SD; \
         RGB defaults full range. Frame rates are nominal; the exact ratio is in the manifest. \
         When another app already controls a camera, Isocast attaches read-only through the \
         Windows Camera Frame Server and keeps that app's current mode, which need not be \
         listed; a source's width, height and fps must then match \
         it or be left out. kind is not reported (other)."
            .into(),
    );
    let audio = crate::audio_devices().unwrap_or_else(|error| {
        notes.push(format!("Audio devices could not be listed: {error:#}"));
        Vec::new()
    });
    let audio_outputs = audio
        .iter()
        .filter(|device| !device.input)
        .map(|device| AudioOutput {
            id: device.id.clone(),
            name: device.name.clone(),
            default: device.default,
            sample_rate: f64::from(device.sample_rate),
            channels: device.channels,
        })
        .collect();
    let microphones = audio
        .into_iter()
        .filter(|device| device.input)
        .map(|device| Microphone {
            id: device.id,
            name: device.name,
            model: String::new(),
            manufacturer: String::new(),
            default: device.default,
            sample_rates: (device.sample_rate > 0)
                .then(|| f64::from(device.sample_rate))
                .into_iter()
                .collect(),
            channels: device.channels,
            channel_names: Vec::new(),
        })
        .collect();
    notes.push(
        "Microphone sample_rates is each endpoint's current shared-mode mix rate; captures are \
         always delivered at 48000 Hz, resampled by Windows from other rates. Model and \
         manufacturer are not reported."
            .into(),
    );
    let audio_apps = crate::audio_apps::audio_apps().unwrap_or_else(|error| {
        notes.push(format!("Audio applications could not be listed: {error:#}"));
        Vec::new()
    });
    notes.push("Audio application id is the executable path; listed names and binaries also match. Use the full executable path to select an app before launch. Process-loopback capture requires Windows 10 2004 (build 19041) or later and follows the selected process tree across restarts. Long-take alignment is unqualified: application audio drifted about 46 ppm relative to desktop audio (about 165 ms/hour) on the tested Windows host; absolute video alignment remains unverified.".into());
    Ok(Devices {
        displays: displays.into_iter().map(|info| info.display).collect(),
        windows,
        cameras,
        microphones,
        audio_outputs,
        audio_apps,
        permissions: crate::permissions::states(),
        notes,
    })
}

/// One camera as listed: sizes largest first, each with its native rates.
fn camera(device: CameraDevice) -> Camera {
    let mut formats: Vec<VideoFormat> = Vec::new();
    for mode in &device.formats {
        let fps = f64::from(mode.fps);
        match formats.iter_mut().find(|format| {
            (format.width, format.height) == (mode.width, mode.height)
                && format.pixel_format == mode.pixel_format.name()
        }) {
            Some(format) if format.frame_rates.contains(&fps) => {}
            Some(format) => format.frame_rates.push(fps),
            None => formats.push(VideoFormat {
                width: mode.width,
                height: mode.height,
                pixel_format: mode.pixel_format.name().into(),
                frame_rates: vec![fps],
                frame_rate_ranges: Vec::new(),
            }),
        }
    }
    for format in &mut formats {
        format.frame_rates.sort_by(f64::total_cmp);
        // Native modes are fixed rates: each range is a single rate.
        format.frame_rate_ranges = format
            .frame_rates
            .iter()
            .rev()
            .map(|&rate| FrameRateRange {
                min: rate,
                max: rate,
            })
            .collect();
    }
    formats.sort_by_key(|format| {
        std::cmp::Reverse(u64::from(format.width) * u64::from(format.height))
    });
    Camera {
        id: device.id,
        name: device.name,
        model: String::new(),
        manufacturer: String::new(),
        kind: CameraKind::Other,
        formats,
        suspended: false,
    }
}

/// The camera a source names, without starting it. The delivered format is
/// left unknown: it is the selected native mode, or the controlling app's
/// shared mode when the camera is busy, and only starting it tells which.
/// Matching follows the capture's resolution: exact symbolic link, else an
/// exact name, else one name containing the query, ignoring case.
pub(crate) fn camera_probe(query: &str) -> Result<Probe> {
    let cameras = camera_devices()?;
    let found = match cameras.iter().find(|camera| camera.id == query) {
        Some(camera) => vec![camera],
        None => {
            let lower = query.to_lowercase();
            let usable = !query.is_empty()
                && !query.contains('\0')
                && !query.eq_ignore_ascii_case("default");
            let exact: Vec<_> = cameras
                .iter()
                .filter(|camera| usable && camera.name.to_lowercase() == lower)
                .collect();
            if exact.is_empty() {
                cameras
                    .iter()
                    .filter(|camera| usable && camera.name.to_lowercase().contains(&lower))
                    .collect()
            } else {
                exact
            }
        }
    };
    match found.as_slice() {
        [camera] => Ok(Probe {
            device: camera.name.clone(),
            video: None,
            channels: None,
        }),
        found => bail!(
            "camera.device: {query:?} matched {} cameras; use an exact id, exact name or \
             unambiguous part of a name; valid: {}",
            found.len(),
            choices(
                cameras
                    .iter()
                    .map(|camera| format!("{:?} ({})", camera.name, camera.id))
            )
        ),
    }
}

/// Resolve a setup's screen target to a native handle, right before capture.
pub(crate) fn screen_target(target: ScreenTarget<'_>) -> Result<CaptureTarget> {
    match target {
        // Resolved by MonitorFromPoint, even if discovery left it out.
        ScreenTarget::Display(name) if name.eq_ignore_ascii_case("main") => {
            Ok(CaptureTarget::PrimaryDisplay)
        }
        ScreenTarget::Display(name) => {
            let displays = displays(&mut Vec::new())?;
            select_display(&displays, name).map(|info| CaptureTarget::Monitor(info.monitor))
        }
        ScreenTarget::Window { app, title } => {
            let windows = windows(&mut Vec::new())?;
            find_window(&windows, app, title).map(|window| CaptureTarget::Window(window.hwnd))
        }
        ScreenTarget::Application { .. } => application_unsupported(),
    }
}

/// What a screen target would deliver, by the same selection as
/// [`screen_target`], without capturing: physical pixels at `fps`.
pub(crate) fn screen_probe(target: ScreenTarget<'_>, fps: u32) -> Result<Probe> {
    let (device, width, height) = match target {
        ScreenTarget::Display(name) if name.eq_ignore_ascii_case("main") => {
            // A mirrored source can still be captured as "main", even when
            // it has no single physical target to list under a stable id.
            let (paths, modes) = display_config()?;
            let (gdi, _, _) = monitors()
                .into_iter()
                .find(|(_, _, main)| *main)
                .context("screen.display: no primary desktop monitor")?;
            let mode = paths
                .iter()
                .filter(|path| source_name(path).as_deref() == Some(gdi.as_str()))
                .find_map(|path| source_mode(path, &modes))
                .context("screen.display: Windows did not report the primary source mode")?;
            (gdi, mode.width, mode.height)
        }
        ScreenTarget::Display(name) => {
            let displays = displays(&mut Vec::new())?;
            let info = select_display(&displays, name)?;
            (
                info.display.name.clone(),
                info.display.width,
                info.display.height,
            )
        }
        ScreenTarget::Window { app, title } => {
            let windows = windows(&mut Vec::new())?;
            let window = find_window(&windows, app, title)?;
            (window.label(), window.width, window.height)
        }
        ScreenTarget::Application { .. } => application_unsupported()?,
    };
    Ok(Probe {
        device,
        video: Some(CaptureFormat { width, height, fps }),
        channels: None,
    })
}

fn application_unsupported<T>() -> Result<T> {
    bail!(
        "screen.app: capturing every window of an application is not supported on Windows; \
         capture a display or one window"
    )
}

/// "main" is the primary display; otherwise an exact id, else an exact
/// name, ignoring case. A name shared by several displays is rejected.
fn select_display<'a>(displays: &'a [DisplayInfo], name: &str) -> Result<&'a DisplayInfo> {
    let query = name.to_lowercase();
    let mut matches: Vec<&DisplayInfo> = if name.eq_ignore_ascii_case("main") {
        displays.iter().filter(|info| info.display.main).collect()
    } else {
        displays
            .iter()
            .filter(|info| info.display.id.eq_ignore_ascii_case(name))
            .collect()
    };
    if matches.is_empty() {
        matches = displays
            .iter()
            .filter(|info| info.display.name.to_lowercase() == query)
            .collect();
    }
    let options = |list: &[&DisplayInfo]| {
        choices(
            list.iter()
                .map(|info| format!("{:?} ({})", info.display.name, info.display.id)),
        )
    };
    match matches.as_slice() {
        [one] => Ok(one),
        [] => bail!(
            "screen.display: no listed display {name:?}; valid: \"main\", {}",
            options(&displays.iter().collect::<Vec<_>>())
        ),
        many => bail!(
            "screen.display: {name:?} names {} displays; use an id: {}",
            many.len(),
            options(many)
        ),
    }
}

/// One window, matching the documented WindowSource contract (as macOS):
/// app is the program's name, file name or full path, or part of its name;
/// title is part of the window's title; both ignore case. The largest match
/// wins; equal areas prefer an exact title, then the lowest handle, so the
/// pick is deterministic.
fn find_window<'a>(
    windows: &'a [WindowInfo],
    app: &str,
    title: Option<&str>,
) -> Result<&'a WindowInfo> {
    if app.trim().is_empty() || app.contains('\0') {
        bail!("screen.app: give a nonempty application name or executable path without NUL");
    }
    let query = app.to_lowercase();
    let wanted = title.map(str::to_lowercase);
    let by_app: Vec<&WindowInfo> = windows
        .iter()
        .filter(|w| {
            let name = w.app.to_lowercase();
            name == query
                || name.contains(&query)
                || w.path.to_lowercase() == query
                || file_name(&w.path).to_lowercase() == query
        })
        .collect();
    if by_app.is_empty() {
        let mut apps: Vec<&str> = windows.iter().map(|w| w.app.as_str()).collect();
        apps.sort_unstable();
        apps.dedup();
        bail!(
            "screen.app: no window of {app:?} is open; apps with windows: {}",
            choices(apps.into_iter().map(str::to_string))
        );
    }
    let lowered = |w: &WindowInfo| w.title.as_deref().map(str::to_lowercase);
    let best = by_app
        .iter()
        .filter(|w| {
            wanted
                .as_ref()
                .is_none_or(|t| lowered(w).is_some_and(|wt| wt.contains(t)))
        })
        .max_by_key(|w| {
            let exact = wanted
                .as_ref()
                .is_some_and(|t| lowered(w).is_some_and(|wt| wt == *t));
            (
                u64::from(w.width) * u64::from(w.height),
                exact,
                std::cmp::Reverse(w.hwnd),
            )
        });
    match best {
        Some(window) => Ok(*window),
        None => bail!(
            "screen.title: no window of {app:?} titled {:?}; its windows: {}",
            title.unwrap_or_default(),
            choices(by_app.iter().map(|w| w.label()))
        ),
    }
}

/// Active displays with a stable identity: the monitor's device interface
/// path (EDID-based instance), never an HMONITOR or `\\.\DISPLAYn`.
fn displays(notes: &mut Vec<String>) -> Result<Vec<DisplayInfo>> {
    let (paths, modes) = display_config()?;
    let monitors = monitors();
    type SourcePaths<'a> = ((u32, i32, u32), Vec<&'a DISPLAYCONFIG_PATH_INFO>);
    let mut sources: Vec<SourcePaths<'_>> = Vec::new();
    for path in &paths {
        let source = &path.sourceInfo;
        let key = (
            source.adapterId.LowPart,
            source.adapterId.HighPart,
            source.id,
        );
        match sources.iter_mut().find(|(k, _)| *k == key) {
            Some((_, group)) => group.push(path),
            None => sources.push((key, vec![path])),
        }
    }
    let mut displays = Vec::new();
    for (_, group) in sources {
        let path = group[0];
        let Some(gdi) = source_name(path) else {
            notes.push("Left out a display whose source name Windows did not report.".into());
            continue;
        };
        if group.len() > 1 {
            notes.push(format!(
                "Left out {gdi}: it is mirrored to {} monitors, so no one monitor identifies it.",
                group.len()
            ));
            continue;
        }
        let Some(target) = target_name(path) else {
            notes.push(format!(
                "Left out {gdi}: Windows did not report its monitor."
            ));
            continue;
        };
        let id = wide(&target.monitorDevicePath);
        if id.is_empty() {
            notes.push(format!(
                "Left out {gdi}: its monitor has no device path to identify it by."
            ));
            continue;
        }
        let Some(&(_, monitor, main)) = monitors.iter().find(|(name, ..)| *name == gdi) else {
            notes.push(format!("Left out {gdi}: it is not part of the desktop."));
            continue;
        };
        let Some(mode) = source_mode(path, &modes) else {
            notes.push(format!("Left out {gdi}: Windows did not report its mode."));
            continue;
        };
        let rate = path.targetInfo.refreshRate;
        let name = wide(&target.monitorFriendlyDeviceName);
        displays.push(DisplayInfo {
            display: Display {
                id,
                name: if name.is_empty() { gdi } else { name },
                main,
                built_in: [
                    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL,
                    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
                    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED,
                    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS,
                ]
                .contains(&target.outputTechnology),
                width: mode.width,
                height: mode.height,
                refresh_rate: if rate.Denominator == 0 {
                    0.0
                } else {
                    f64::from(rate.Numerator) / f64::from(rate.Denominator)
                },
            },
            monitor,
        });
    }
    Ok(displays)
}

fn source_mode(
    path: &DISPLAYCONFIG_PATH_INFO,
    modes: &[DISPLAYCONFIG_MODE_INFO],
) -> Option<DISPLAYCONFIG_SOURCE_MODE> {
    // Queries do not request virtual-mode awareness: this union holds the
    // ordinary source-mode index, not the virtual-mode packed indices.
    let index = unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize;
    let mode = modes.get(index).filter(|mode| {
        mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
            && mode.id == path.sourceInfo.id
            && mode.adapterId.LowPart == path.sourceInfo.adapterId.LowPart
            && mode.adapterId.HighPart == path.sourceInfo.adapterId.HighPart
    })?;
    // SAFETY: the checked discriminant says the union holds a source mode.
    let source = unsafe { mode.Anonymous.sourceMode };
    (source.width > 0 && source.height > 0).then_some(source)
}

fn display_config() -> Result<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>)> {
    // The topology can change between sizing and querying; retry a little.
    for _ in 0..3 {
        let (mut path_count, mut mode_count) = (0u32, 0u32);
        unsafe {
            GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
        }
        .to_hresult()
        .ok()
        .context("GetDisplayConfigBufferSizes")?;
        if path_count as usize > MAX_ENTRIES || mode_count as usize > MAX_ENTRIES {
            bail!("the display configuration reports {path_count} paths and {mode_count} modes");
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let status = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if status == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        status.to_hresult().ok().context("QueryDisplayConfig")?;
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);
        return Ok((paths, modes));
    }
    bail!("the display configuration kept changing while it was read")
}

fn source_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
    let mut request = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut request.header) };
    (status == 0).then(|| wide(&request.viewGdiDeviceName))
}

fn target_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<DISPLAYCONFIG_TARGET_DEVICE_NAME> {
    let mut request = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut request.header) };
    (status == 0).then_some(request)
}

/// Desktop monitors: GDI device name, HMONITOR, primary.
fn monitors() -> Vec<(String, usize, bool)> {
    let mut handles = Collected::new();
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut handles as *mut Collected as isize),
        )
    };
    handles
        .items
        .into_iter()
        .filter_map(|handle| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            let found = unsafe {
                GetMonitorInfoW(
                    HMONITOR(handle as *mut _),
                    &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
                )
            };
            found.as_bool().then(|| {
                let main = info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;
                (wide(&info.szDevice), handle, main)
            })
        })
        .collect()
}

/// Capture candidates on this desktop. This process's own windows are kept
/// on purpose (unlike macOS): the platform's capture probe is one.
fn windows(notes: &mut Vec<String>) -> Result<Vec<WindowInfo>> {
    let mut handles = Collected::new();
    let listed = unsafe {
        EnumWindows(
            Some(collect_window),
            LPARAM(&mut handles as *mut Collected as isize),
        )
    };
    if handles.truncated {
        notes.push(format!(
            "Only the first {MAX_ENTRIES} top-level windows were inspected."
        ));
    } else {
        listed.context("EnumWindows")?;
    }
    let mut text = vec![0u16; MAX_TEXT];
    let mut programs: HashMap<u32, Option<String>> = HashMap::new();
    let mut unreadable = 0;
    let mut windows = Vec::new();
    for handle in handles.items {
        let hwnd = HWND(handle as *mut _);
        let candidate = unsafe {
            IsWindowVisible(hwnd).as_bool()
                && !IsIconic(hwnd).as_bool()
                && GetWindow(hwnd, GW_OWNER).is_err()
                && GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 == 0
        };
        if !candidate || cloaked(hwnd) {
            continue;
        }
        let Some((width, height)) = frame_size(hwnd).filter(|&(w, h)| w > 0 && h > 0) else {
            continue;
        };
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let path = programs
            .entry(pid)
            .or_insert_with(|| process_path(pid, &mut text))
            .clone();
        let Some(path) = path else {
            unreadable += 1;
            continue;
        };
        // Reads the cached title without sending WM_GETTEXT to any window.
        let length = unsafe { InternalGetWindowText(hwnd, &mut text) };
        let length = usize::try_from(length).unwrap_or(0).min(text.len());
        let title = String::from_utf16_lossy(&text[..length]);
        windows.push(WindowInfo {
            hwnd: handle,
            app: app_name(&path),
            path,
            title: (!title.is_empty()).then_some(title),
            width,
            height,
        });
    }
    if unreadable > 0 {
        notes.push(format!(
            "{unreadable} window(s) left out: their program could not be identified (for \
             example, a protected process)."
        ));
    }
    windows.sort_by_cached_key(|w| (w.app.to_lowercase(), w.title.clone(), w.hwnd));
    Ok(windows)
}

fn cloaked(hwnd: HWND) -> bool {
    let mut value = 0u32;
    let read = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut value as *mut u32 as *mut _,
            size_of::<u32>() as u32,
        )
    };
    read.is_ok() && value != 0
}

/// DWM's visible frame in physical pixels; GetWindowRect would include the
/// invisible resize borders and be DPI-virtualized for unaware callers.
fn frame_size(hwnd: HWND) -> Option<(u32, u32)> {
    let mut rect = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    let width = u32::try_from(rect.right.checked_sub(rect.left)?).ok()?;
    let height = u32::try_from(rect.bottom.checked_sub(rect.top)?).ok()?;
    Some((width, height))
}

fn process_path(pid: u32, buffer: &mut [u16]) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    // SAFETY: a newly opened handle, closed when `process` drops.
    let process = unsafe { Owned::new(process) };
    let mut length = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    unsafe {
        QueryFullProcessImageNameW(
            *process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .ok()?;
    let length = (length as usize).min(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..length]))
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `C:\…\Code.exe` → `Code`.
fn app_name(path: &str) -> String {
    let name = file_name(path);
    let stem = name.len().checked_sub(4).filter(|&at| {
        name.get(at..)
            .is_some_and(|ext| ext.eq_ignore_ascii_case(".exe"))
    });
    match stem {
        Some(at) if at > 0 => name[..at].to_string(),
        _ => name,
    }
}

fn wide(text: &[u16]) -> String {
    let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

fn choices(items: impl Iterator<Item = String>) -> String {
    let items: Vec<String> = items.collect();
    let mut list = items
        .iter()
        .take(MAX_CHOICES)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > MAX_CHOICES {
        list.push_str(&format!(", … and {} more", items.len() - MAX_CHOICES));
    }
    if list.is_empty() {
        list.push_str("(none)");
    }
    list
}
