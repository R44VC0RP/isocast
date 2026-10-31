//! The choices a setting naming a device, a display, an app or a window
//! has, from the engine's `devices` list (the web app's lib/devices.js).
//! They match setup values the way the engine does (an id, a name, or
//! part of a name), so the option for what a source already names is the
//! one shown; a value nothing matches stays as an option, "(not
//! connected)", and is never replaced on its own.

use isocast_protocol::devices::{AudioOutput, Camera, CameraKind, Devices, VideoFormat, Window};

/// A dropdown choice: value, label, detail.
pub type Choice = (String, String, String);

fn choice(value: impl Into<String>, label: impl Into<String>, detail: impl Into<String>) -> Choice {
    (value.into(), label.into(), detail.into())
}

fn camera_hint(kind: CameraKind) -> &'static str {
    match kind {
        CameraKind::BuiltIn => "built-in",
        CameraKind::External => "USB",
        CameraKind::Continuity => "iPhone",
        CameraKind::DeviceScreen => "iPhone or iPad screen",
        CameraKind::DeskView => "Desk View",
        CameraKind::Virtual => "virtual",
        CameraKind::Other => "",
    }
}

/// A device the engine lists: its id and name.
pub trait Named {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
}

impl Named for Camera {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for AudioOutput {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for isocast_protocol::devices::Microphone {
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
}

/// The device in `list` a setup value names: its id or name, the name
/// ignoring case, then the shortest name containing it (as the engine
/// matches cameras and microphones).
pub fn match_device<'a, T: Named>(list: &'a [T], query: &str) -> Option<&'a T> {
    if query.is_empty() {
        return None;
    }
    let lower = query.to_lowercase();
    list.iter()
        .find(|d| d.id() == query || d.name() == query)
        .or_else(|| list.iter().find(|d| d.name().to_lowercase() == lower))
        .or_else(|| {
            list.iter()
                .filter(|d| d.name().to_lowercase().contains(&lower))
                .min_by_key(|d| d.name().len())
        })
}

/// Options for devices: each by name (by id if two share the name, then
/// told apart as "1 of 2"), the one `value` names under `value` itself so
/// it shows as chosen.
fn device_options<T: Named>(list: &[T], value: &str, hint: impl Fn(&T) -> String) -> Vec<Choice> {
    let matched = match_device(list, value).map(|d| d.id().to_string());
    list.iter()
        .map(|d| {
            let twins: Vec<&T> = list.iter().filter(|o| o.name() == d.name()).collect();
            let twin = twins.len() > 1;
            let v = if matched.as_deref() == Some(d.id()) {
                value.to_string()
            } else if twin {
                d.id().to_string()
            } else {
                d.name().to_string()
            };
            let which = if twin {
                let at = twins.iter().position(|o| o.id() == d.id()).unwrap_or(0);
                format!("{} of {}", at + 1, twins.len())
            } else {
                String::new()
            };
            let detail = [hint(d), which]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(", ");
            choice(v, d.name(), detail)
        })
        .collect()
}

/// With `value` kept, as "(not connected)", if no option is it.
fn keep(mut options: Vec<Choice>, value: &str, missing: &str) -> Vec<Choice> {
    if value.is_empty() || options.iter().any(|o| o.0 == value) {
        return options;
    }
    options.insert(0, choice(value, format!("{value} ({missing})"), ""));
    options
}

/// With a "Choose…" first while nothing is chosen, so choosing the first
/// real option is a change.
fn prompt(mut options: Vec<Choice>, value: &str, text: &str) -> Vec<Choice> {
    if value.is_empty() {
        options.insert(0, choice("", text, ""));
    }
    options
}

pub fn camera_options(devices: Option<&Devices>, value: &str) -> Vec<Choice> {
    let cameras = devices.map(|d| d.cameras.as_slice()).unwrap_or_default();
    let options = device_options(cameras, value, |c| {
        [
            camera_hint(c.kind),
            if c.suspended { "unavailable" } else { "" },
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
    });
    prompt(
        keep(options, value, "not connected"),
        value,
        "Choose a camera…",
    )
}

pub fn microphone_options(devices: Option<&Devices>, value: &str) -> Vec<Choice> {
    let mics = devices
        .map(|d| d.microphones.as_slice())
        .unwrap_or_default();
    let mut options = vec![choice(
        "default",
        "System default",
        mics.iter()
            .find(|m| m.default)
            .map(|m| m.name.clone())
            .unwrap_or_default(),
    )];
    let named = if value == "default" { "" } else { value };
    options.extend(device_options(mics, named, |m| {
        if m.default {
            "default".into()
        } else {
            String::new()
        }
    }));
    keep(options, value, "not connected")
}

/// The microphone a source's `device` names ("default": the system's).
pub fn find_microphone<'a>(
    devices: Option<&'a Devices>,
    device: &str,
) -> Option<&'a isocast_protocol::devices::Microphone> {
    let mics = devices
        .map(|d| d.microphones.as_slice())
        .unwrap_or_default();
    if device == "default" {
        return mics.iter().find(|m| m.default);
    }
    match_device(mics, device)
}

/// The value a microphone's `channels` has in its dropdown: "" for the
/// default, else the channels joined with "," ("1", "3,4").
pub fn channels_value(channels: &serde_json::Value) -> String {
    channels
        .as_array()
        .map(|list| {
            list.iter()
                .map(crate::form::text_of)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default()
}

/// A microphone's channel choices: the default (its own stereo, a larger
/// interface's inputs 1 + 2), each
/// input alone as mono, then each odd-even pair as stereo, with the names
/// the device gives them. `value` (as [`channels_value`] writes it) stays
/// an option even if the device doesn't have it.
pub fn channel_options(devices: Option<&Devices>, device: &str, value: &str) -> Vec<Choice> {
    let mic = find_microphone(devices, device);
    let count = mic.map_or(0, |m| m.channels);
    let name = |c: u32| {
        mic.and_then(|m| m.channel_names.get(c as usize - 1))
            .filter(|n| !n.is_empty())
            .cloned()
    };
    let default_detail = match count {
        0 => String::new(),
        1 => "input 1".to_string(),
        _ => "inputs 1 + 2, stereo".to_string(),
    };
    let mut options = vec![choice("", "Default", default_detail)];
    for c in 1..=count {
        let detail = match name(c) {
            Some(n) => format!("{n}, mono"),
            None => "mono".to_string(),
        };
        options.push(choice(c.to_string(), format!("Input {c}"), detail));
    }
    for left in (1..count).step_by(2) {
        let right = left + 1;
        let detail = match (name(left), name(right)) {
            (Some(l), Some(r)) => format!("{l} + {r}, stereo"),
            _ => "stereo".to_string(),
        };
        options.push(choice(
            format!("{left},{right}"),
            format!("Inputs {left} + {right}"),
            detail,
        ));
    }
    if value.is_empty() || options.iter().any(|o| o.0 == value) {
        return options;
    }
    let label = match value.split_once(',') {
        Some((l, r)) => format!("Inputs {l} + {r}"),
        None => format!("Input {value}"),
    };
    let detail = if mic.is_some() {
        "not on this device"
    } else {
        ""
    };
    options.push(choice(value, label, detail));
    options
}

/// Audio outputs (monitoring): "Default" (whichever is current), then
/// each by name.
pub fn output_options(devices: Option<&Devices>, value: &str) -> Vec<Choice> {
    let outputs = devices
        .map(|d| d.audio_outputs.as_slice())
        .unwrap_or_default();
    let mut options = vec![choice(
        "default",
        "Default",
        outputs
            .iter()
            .find(|o| o.default)
            .map(|o| o.name.clone())
            .unwrap_or_default(),
    )];
    let named = if value == "default" { "" } else { value };
    options.extend(device_options(outputs, named, |o| {
        if o.default {
            "current".into()
        } else {
            String::new()
        }
    }));
    keep(options, value, "not connected")
}

/// Displays: "main", then each by its id (stable, where names like "LG
/// HDR 4K (1)" may swap), the one `value` names under `value` itself.
pub fn display_options(devices: Option<&Devices>, value: &str) -> Vec<Choice> {
    let displays = devices.map(|d| d.displays.as_slice()).unwrap_or_default();
    let main = displays.iter().find(|d| d.main);
    let lower = value.to_lowercase();
    let matched = if value == "main" {
        None
    } else {
        displays
            .iter()
            .find(|d| d.id == value || d.name == value)
            .or_else(|| displays.iter().find(|d| d.name.to_lowercase() == lower))
    };
    let mut options = vec![choice(
        "main",
        "Main display",
        main.map(|m| format!("{}, {}×{}", m.name, m.width, m.height))
            .unwrap_or_default(),
    )];
    // A display whose id is "main" (Linux's desktop-portal screen) is the
    // row above already.
    options.extend(displays.iter().filter(|d| d.id != "main").map(|d| {
        let v = if matched.is_some_and(|m| m.id == d.id) {
            value.to_string()
        } else {
            d.id.clone()
        };
        choice(v, &d.name, format!("{}×{}", d.width, d.height))
    }));
    if PICKED_IN_DIALOG {
        options.push(pick_option("screen"));
    }
    keep(options, value, "not connected")
}

/// Screens and windows are chosen in the desktop's sharing dialog
/// (`screen_pick`), since the engine can't list them (Linux).
pub const PICKED_IN_DIALOG: bool = cfg!(target_os = "linux");

/// The choice that opens the desktop's sharing dialog for the source.
pub const PICK: &str = "\u{200b}pick in the dialog";

/// The choice that opens the desktop's sharing dialog for a `what`
/// ("screen" or "window").
fn pick_option(what: &str) -> Choice {
    choice(PICK, format!("Choose a {what}…"), "in the desktop’s dialog")
}

/// A window source's choices where the desktop's dialog chooses the window
/// (`PICKED_IN_DIALOG`): the one it has, named by `app` (a label there),
/// and the dialog.
pub fn picked_window_options(app: &str) -> Vec<Choice> {
    vec![
        choice(app, "Chosen in the desktop’s dialog", app),
        pick_option("window"),
    ]
}

/// An app with windows that can be captured.
pub struct App<'a> {
    /// Its bundle id, or its name without one.
    pub key: String,
    pub name: String,
    pub windows: Vec<&'a Window>,
}

/// The apps with windows, sorted by name.
pub fn apps(devices: Option<&Devices>) -> Vec<App<'_>> {
    let mut found: Vec<App> = Vec::new();
    for w in devices.map(|d| d.windows.as_slice()).unwrap_or_default() {
        if w.app.is_empty() {
            continue;
        }
        let key = if w.bundle_id.is_empty() {
            w.app.clone()
        } else {
            w.bundle_id.clone()
        };
        match found.iter_mut().find(|a| a.key == key) {
            Some(app) => app.windows.push(w),
            None => found.push(App {
                key,
                name: w.app.clone(),
                windows: vec![w],
            }),
        }
    }
    found.sort_by_key(|a| a.name.to_lowercase());
    found
}

/// The app a setup's `app` names (a bundle id, a name, or part of a name,
/// ignoring case), as the engine matches it.
pub fn match_app<'a>(devices: Option<&'a Devices>, value: &str) -> Option<App<'a>> {
    if value.is_empty() {
        return None;
    }
    let lower = value.to_lowercase();
    let all = apps(devices);
    let at = all
        .iter()
        .position(|a| a.key.to_lowercase() == lower || a.name.to_lowercase() == lower)
        .or_else(|| {
            all.iter()
                .position(|a| a.name.to_lowercase().contains(&lower))
        })?;
    all.into_iter().nth(at)
}

/// Apps, each by bundle id (the engine takes a name as part of a name, so
/// "Code" would also match Xcode).
pub fn app_options(devices: Option<&Devices>, value: &str) -> Vec<Choice> {
    let matched = match_app(devices, value).map(|a| a.key);
    let all = apps(devices);
    let options = all
        .iter()
        .map(|a| {
            let twin = all.iter().filter(|o| o.name == a.name).count() > 1;
            let n = a.windows.len();
            let count = format!("{n} window{}", if n == 1 { "" } else { "s" });
            let v = if matched.as_deref() == Some(a.key.as_str()) {
                value.to_string()
            } else {
                a.key.clone()
            };
            let detail = if twin {
                format!("{}, {count}", a.key)
            } else {
                count
            };
            choice(v, &a.name, detail)
        })
        .collect();
    prompt(keep(options, value, "not open"), value, "Choose an app…")
}

/// Apps whose sound can be captured or left out: those that have opened
/// audio (playing ones first), then other apps with windows. Each by
/// bundle id where it has one, else its name (tools like `afplay`).
fn audio_apps(devices: Option<&Devices>) -> Vec<(String, String, String)> {
    let mut found: Vec<(String, String, String)> = Vec::new();
    for a in devices.map(|d| d.audio_apps.as_slice()).unwrap_or_default() {
        let key = a.bundle_id.clone().unwrap_or_else(|| a.name.clone());
        if found.iter().all(|f| f.0 != key) {
            let detail = if a.playing { "playing" } else { "audio" };
            found.push((key, a.name.clone(), detail.into()));
        }
    }
    for a in apps(devices) {
        if found.iter().all(|f| f.0 != a.key) {
            found.push((a.key, a.name, String::new()));
        }
    }
    found
}

/// Apps for an application audio source's `app`.
pub fn audio_app_options(devices: Option<&Devices>, value: &str) -> Vec<Choice> {
    let lower = value.to_lowercase();
    let options = audio_apps(devices)
        .into_iter()
        .map(|(key, name, detail)| {
            // Keep a setup's own spelling of the app it names.
            let v = if !value.is_empty()
                && (key.to_lowercase() == lower || name.to_lowercase() == lower)
            {
                value.to_string()
            } else {
                key
            };
            choice(v, name, detail)
        })
        .collect();
    prompt(keep(options, value, "not open"), value, "Choose an app…")
}

/// Apps a system audio source could leave out, besides those it does.
pub fn exclude_options(devices: Option<&Devices>, excluded: &[String]) -> Vec<Choice> {
    let left_out = |key: &str, name: &str| {
        excluded
            .iter()
            .any(|e| e.eq_ignore_ascii_case(key) || e.eq_ignore_ascii_case(name))
    };
    let options = audio_apps(devices)
        .into_iter()
        .filter(|(key, name, _)| !left_out(key, name))
        .map(|(key, name, detail)| choice(key, name, detail))
        .collect();
    prompt(options, "", "Leave out an app…")
}

/// The name an excluded app is shown by: its name if it's running.
pub fn app_name(devices: Option<&Devices>, value: &str) -> String {
    audio_apps(devices)
        .into_iter()
        .find(|(key, name, _)| key.eq_ignore_ascii_case(value) || name.eq_ignore_ascii_case(value))
        .map_or_else(|| value.to_string(), |(_, name, _)| name)
}

/// The window of `app` a window source with `title` captures: the
/// largest whose title contains it (ignoring case), as the engine picks.
pub fn match_window<'a>(
    devices: Option<&'a Devices>,
    app: &str,
    title: &str,
) -> Option<&'a Window> {
    let found = match_app(devices, app)?;
    let lower = title.to_lowercase();
    found
        .windows
        .into_iter()
        .filter(|w| {
            lower.is_empty()
                || w.title
                    .as_deref()
                    .is_some_and(|t| t.to_lowercase().contains(&lower))
        })
        .max_by_key(|w| w.width as u64 * w.height as u64)
}

/// The option standing for part of a title, typed.
pub const TITLE_PART: &str = "\u{200b}part of a title";

/// A window source's title choices: any window of the app, one of its
/// windows' whole titles, or part of a title (`TITLE_PART`).
pub fn title_options(devices: Option<&Devices>, app: &str, value: &str) -> Vec<Choice> {
    let found = match_app(devices, app);
    let windows: Vec<&Window> = found
        .map(|a| {
            a.windows
                .into_iter()
                .filter(|w| w.title.is_some())
                .collect()
        })
        .unwrap_or_default();
    let mut titles: Vec<&str> = Vec::new();
    for w in &windows {
        let t = w.title.as_deref().unwrap_or_default();
        if !titles.contains(&t) {
            titles.push(t);
        }
    }
    let mut options = vec![choice("", "Any window", "the largest")];
    for t in &titles {
        let largest = windows
            .iter()
            .filter(|w| w.title.as_deref() == Some(*t))
            .max_by_key(|w| w.width as u64 * w.height as u64)
            .map(|w| format!("{}×{}", w.width, w.height))
            .unwrap_or_default();
        options.push(choice(*t, *t, largest));
    }
    if !value.is_empty() && !titles.contains(&value) {
        let detail = match_window(devices, app, value)
            .and_then(|w| w.title.clone())
            .unwrap_or_else(|| "no window has it now".into());
        options.push(choice(TITLE_PART, "Part of a title", detail));
    } else {
        options.push(choice(TITLE_PART, "Part of a title…", ""));
    }
    options
}

// ---------- Camera formats ----------

/// The sizes a camera offers, largest first, without repeats.
pub fn camera_sizes(camera: Option<&Camera>) -> Vec<(u32, u32)> {
    let mut sizes: Vec<(u32, u32)> = Vec::new();
    for f in camera.map(|c| c.formats.as_slice()).unwrap_or_default() {
        if !sizes.contains(&(f.width, f.height)) {
            sizes.push((f.width, f.height));
        }
    }
    sizes.sort_by(|a, b| {
        (b.0 as u64 * b.1 as u64)
            .cmp(&(a.0 as u64 * a.1 as u64))
            .then(b.0.cmp(&a.0))
    });
    sizes
}

/// A format's frame-rate ranges (just its top rates from an engine that
/// doesn't list ranges).
pub fn rate_ranges(f: &VideoFormat) -> Vec<(f64, f64)> {
    if f.frame_rate_ranges.is_empty() {
        f.frame_rates.iter().map(|r| (*r, *r)).collect()
    } else {
        f.frame_rate_ranges.iter().map(|r| (r.min, r.max)).collect()
    }
}

/// Rates people pick, listed when a range holds them.
const COMMON_RATES: [f64; 5] = [24.0, 25.0, 30.0, 50.0, 60.0];

/// The frame rates a camera offers at `size` (every size's, without one),
/// highest first, as (the whole number a setup holds, its label).
pub fn camera_rates(camera: Option<&Camera>, size: Option<(u32, u32)>) -> Vec<(i64, String)> {
    let mut rates: Vec<(i64, f64)> = Vec::new();
    let mut add = |r: f64| {
        let whole = r.round() as i64;
        // 30 and 29.97 are both 30 to a setup; show the exact one if offered.
        let exact = (r - whole as f64).abs() < 0.005;
        let shown = if exact {
            whole as f64
        } else {
            (r * 100.0).round() / 100.0
        };
        match rates.iter_mut().find(|(w, _)| *w == whole) {
            Some(entry) => {
                if exact {
                    entry.1 = shown;
                }
            }
            None => rates.push((whole, shown)),
        }
    };
    for f in camera.map(|c| c.formats.as_slice()).unwrap_or_default() {
        if size.is_some_and(|s| s != (f.width, f.height)) {
            continue;
        }
        for (min, max) in rate_ranges(f) {
            add(max);
            for r in COMMON_RATES {
                if r >= min - 0.5 && r <= max + 0.5 {
                    add(r);
                }
            }
        }
    }
    rates.sort_by_key(|r| std::cmp::Reverse(r.0));
    rates
        .into_iter()
        .map(|(whole, shown)| (whole, format!("{} fps", fmt_number(shown))))
        .collect()
}

/// 30 → "30", 29.97 → "29.97".
pub fn fmt_number(n: f64) -> String {
    if (n - n.round()).abs() < 1e-9 {
        format!("{}", n.round() as i64)
    } else {
        let text = format!("{n:.4}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Whether `camera` offers `size` (any if none) at `fps` (any if none).
pub fn camera_supports(
    camera: Option<&Camera>,
    size: Option<(u32, u32)>,
    fps: Option<i64>,
) -> bool {
    if camera.is_none() {
        return false;
    }
    if let Some(size) = size
        && !camera_sizes(camera).contains(&size)
    {
        return false;
    }
    if let Some(fps) = fps
        && !camera_rates(camera, size).iter().any(|(r, _)| *r == fps)
    {
        return false;
    }
    true
}

/// What Auto picks on this canvas, as the engine does: the highest rate up
/// to the canvas's (or exactly `fps`), then the largest size that fits on
/// the canvas either way round (else the smallest larger one), turned as
/// the canvas is if it can be. (width, height, fps)
pub fn auto_format(
    camera: &Camera,
    canvas: (u32, u32, u32),
    size: Option<(u32, u32)>,
    fps: Option<i64>,
) -> Option<(u32, u32, i64)> {
    let (cw, ch, cfps) = canvas;
    let target = fps.map(|f| f as f64).unwrap_or(cfps as f64);
    let mut best: Option<([i64; 4], (u32, u32, i64))> = None;
    for f in &camera.formats {
        if size.is_some_and(|s| s != (f.width, f.height)) {
            continue;
        }
        for (min, max) in rate_ranges(f) {
            let rate = if target >= min - 0.01 && target <= max + 0.01 {
                target
            } else if max < target {
                max
            } else {
                min
            };
            if fps.is_some() && (rate - target).abs() > 0.5 {
                continue;
            }
            let fits = f.width.max(f.height) <= cw.max(ch) && f.width.min(f.height) <= cw.min(ch);
            let area = f.width as i64 * f.height as i64;
            let turned = (f.width >= f.height) == (cw >= ch);
            let key = [
                (rate.round() as i64).min(cfps as i64),
                fits as i64,
                if fits { area } else { -area },
                turned as i64,
            ];
            if best.as_ref().is_none_or(|(b, _)| key > *b) {
                best = Some((key, (f.width, f.height, rate.round() as i64)));
            }
        }
    }
    best.map(|(_, v)| v)
}

/// The aspect ratio's name, if it is a common one.
pub fn aspect_name(width: u32, height: u32) -> &'static str {
    const ASPECTS: [(u32, u32, &str); 6] = [
        (16, 9, "16:9"),
        (4, 3, "4:3"),
        (3, 2, "3:2"),
        (1, 1, "1:1"),
        (9, 16, "9:16"),
        (21, 9, "21:9"),
    ];
    ASPECTS
        .iter()
        .find(|(w, h, _)| (width as f64 / height as f64 - *w as f64 / *h as f64).abs() < 0.01)
        .map(|a| a.2)
        .unwrap_or("")
}
