//! A good starting setup made from this machine's devices (`isocast setup
//! init`): the main display, the best real camera, the default microphone
//! and system audio, in three scenes ("Screen + Camera", "Camera",
//! "Screen"), recording the program and ISO files of the screen and camera.
//!
//! Devices whose permission is refused become placeholders (a plain colour
//! in the same place, not recorded as an ISO file) or are left out, and the
//! notes say what to do, so the result is always a valid setup that
//! records.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::devices::{Camera, CameraKind, Devices, Permission};
use crate::*;

/// Choices for [`init`]; each is picked for you when absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InitOptions {
    /// The display to capture: "main", or an id or name from `isocast
    /// devices`. The main display if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// The camera: an id, a name or part of one from `isocast devices`, or
    /// "none". If absent, the best real camera: a USB or other external
    /// camera, else Continuity Camera, else the built-in one; virtual
    /// cameras (OBS, NDI) and Desk View only if there is nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<String>,
    /// The microphone: "default" (the system's default input, if absent),
    /// an id, a name or part of one, or "none".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone: Option<String>,
    /// Put what this Mac plays (system audio) in the program; yes if
    /// absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_audio: Option<bool>,
    /// Where the camera goes in the "Screen + Camera" scene.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
    /// The canvas, 1920×1080 if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Canvas frame rate: the daemon defaults to 30 on <=8-core or <=8-GB
    /// machines, otherwise 60 (or the display's slower refresh rate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// Where recordings go; `~/Movies/Isocast` if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// The setup's name; "My setup" if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Where the camera sits in the "Screen + Camera" scene. On a portrait
/// (tall) canvas every layout stacks the screen and the camera, the camera
/// below, or above for the top layouts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    /// A picture-in-picture a quarter of the canvas wide, in a corner.
    #[default]
    BottomRight,
    BottomLeft,
    TopRight,
    TopLeft,
    /// The screen and the camera next to each other, the camera cropped
    /// to a tall frame on the right.
    SideBySide,
}

impl Layout {
    pub const NAMES: &[&str] = &[
        "bottom_right",
        "bottom_left",
        "top_right",
        "top_left",
        "side_by_side",
    ];
}

/// What [`init`] made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Initialized {
    pub setup: Setup,
    /// The devices picked and why, and anything to do before recording.
    pub notes: Vec<String>,
    /// Problems `setup check` finds in it; none normally.
    pub problems: Vec<Issue>,
    /// The change, when it was applied to the live setup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<crate::api::Applied>,
}

/// Recording folder when none is given: Movies on macOS, Videos elsewhere
/// (Windows' library folder, Linux's XDG videos folder by its usual name).
pub const DEFAULT_FOLDER: &str = if cfg!(target_os = "macos") {
    "~/Movies/Isocast"
} else {
    "~/Videos/Isocast"
};

/// Conservative starting cadence for a new setup/profile, never an edit to
/// an existing one. Memory and CPU counts are portable; they don't pretend
/// to describe NVENC/QSV/AMF session limits or certify software encoding.
pub fn starting_fps(machine: &crate::api::Machine) -> u32 {
    if (machine.memory_gb > 0.0 && machine.memory_gb <= 8.0)
        || (machine.cores > 0 && machine.cores <= 8)
    {
        30
    } else {
        60
    }
}

/// Make a starting setup from `devices` (with their permissions).
/// Fails, naming the valid choices, if an option names a device that
/// isn't there or a value out of range.
pub fn init(devices: &Devices, options: &InitOptions) -> Result<Initialized, String> {
    let mut notes = Vec::new();
    let permissions = &devices.permissions;
    let (width, height) = match (options.width, options.height) {
        (None, None) => (1920, 1080),
        (Some(w), Some(h)) if (16..=8192).contains(&w) && (16..=8192).contains(&h) => {
            (w & !1, h & !1)
        }
        (Some(_), Some(_)) => return Err("width and height must be from 16 to 8192".into()),
        _ => return Err("give the canvas width and height together".into()),
    };
    if let Some(fps) = options.fps
        && !(1..=240).contains(&fps)
    {
        return Err(format!("fps must be from 1 to 240, not {fps}"));
    }

    // The display, or a placeholder in its place.
    let display = match options.display.as_deref() {
        None | Some("main") => devices
            .displays
            .iter()
            .find(|d| d.main)
            .or(devices.displays.first())
            .map(|d| (d, "main".to_string())),
        Some(query) => {
            let found = find(&devices.displays, query, |d| (&d.id, &d.name)).ok_or_else(|| {
                format!(
                    "display {query:?} matches no display; displays: {}, or \"main\"",
                    list(devices.displays.iter().map(|d| (&d.name, &d.id)))
                )
            })?;
            Some((found, found.id.clone()))
        }
    };
    // Unknown can't be checked without trying, so capture is attempted.
    let screen_allowed = matches!(
        permissions.screen_recording,
        Permission::Granted | Permission::Unknown
    );
    let fps = options.fps.unwrap_or_else(|| {
        // A display that refreshes more slowly than 60 Hz sets the pace.
        match display.as_ref().map(|(d, _)| d.refresh_rate.round() as u32) {
            Some(rate) if (24..60).contains(&rate) => rate,
            _ => 60,
        }
    });
    let mut sources = Vec::new();
    let mut iso = Vec::new();
    let screen_kind = match display {
        Some((found, query)) if screen_allowed => {
            notes.push(format!(
                "Screen: {:?}{}.",
                found.name,
                if found.main { ", the main display" } else { "" }
            ));
            if permissions.screen_recording == Permission::Unknown {
                notes.push(
                    crate::platform_text::select_linux(
                        "Screen recording access can't be checked without capturing, so it is \
                         tried when recording starts.",
                        "Screen capture access can't be checked without capturing, so it is \
                         tried when recording starts.",
                        "Linux asks which screen to share (the desktop's screen-sharing dialog) \
                         when capture starts, for the preview or a recording; pick the one to \
                         record there.",
                    )
                    .into(),
                );
            }
            iso.push("screen".to_string());
            SourceKind::Display(DisplaySource {
                display: query,
                cursor: true,
            })
        }
        Some(_) => {
            notes.push(
                "Screen recording isn't allowed yet, so the screen is a placeholder (a plain \
                 colour) and desktop audio is left out: allow it (`isocast permissions` says \
                 where), restart isocastd (`isocast restart`), then run `isocast setup init` \
                 again."
                    .into(),
            );
            placeholder()
        }
        None => {
            notes.push("No display was found, so the screen is a placeholder.".into());
            placeholder()
        }
    };
    sources.push(source("screen", "Screen", screen_kind));

    // The camera: the best one, or the one asked for.
    let camera = match options.camera.as_deref() {
        Some("none") => None,
        Some(query) => Some(
            find(&devices.cameras, query, |c| (&c.id, &c.name)).ok_or_else(|| {
                format!(
                    "camera {query:?} matches no camera; cameras: {}, or \"none\"",
                    list(devices.cameras.iter().map(|c| (&c.name, &c.id)))
                )
            })?,
        ),
        None => best_camera(&devices.cameras),
    };
    let has_camera = camera.is_some();
    if let Some(camera) = camera {
        let device = unique_name(camera, &devices.cameras);
        // Other cameras worth knowing about, each name once.
        let mut others: Vec<String> = Vec::new();
        for other in &devices.cameras {
            let label = format!(
                "{:?} ({}{})",
                other.name,
                other.kind.label(),
                if other.suspended { ", suspended" } else { "" }
            );
            let skip = matches!(other.kind, CameraKind::DeskView | CameraKind::DeviceScreen);
            if other.name != camera.name && !skip && !others.contains(&label) {
                others.push(label);
            }
        }
        notes.push(format!(
            "Camera: {:?} ({}{}){}.",
            camera.name,
            camera.kind.label(),
            if camera.suspended {
                ", suspended now: it shows no picture until it is available"
            } else {
                ""
            },
            if others.is_empty() {
                String::new()
            } else {
                format!(
                    "; also here: {} (pick with the camera option)",
                    others.join(", ")
                )
            }
        ));
        let kind = match permissions.camera {
            Permission::Granted | Permission::NotDetermined | Permission::Unknown => {
                iso.push("camera".to_string());
                if permissions.camera == Permission::NotDetermined {
                    notes.push(
                        crate::platform_text::select_linux("macOS asks for camera access the first time the camera is used; \
                         `isocast permissions --request` asks now.", "Windows Camera privacy settings must allow desktop apps. isocastd.exe is the recording engine; `isocast permissions` reports access.",
"Linux opens cameras as V4L2 devices (/dev/video*) your user must be able to read; `isocast permissions` reports access.")
                            .into(),
                    );
                }
                if permissions.camera == Permission::Unknown {
                    notes.push(
                        crate::platform_text::select_linux(
                            "Camera access can't be checked without using the camera, so it is \
                             tried when recording starts; if it shows no picture, allow camera \
                             access in the system's privacy settings.",
                            "Camera access can't be checked without using the camera, so it is \
                             tried when recording starts; if it shows no picture, turn on camera \
                             access for desktop apps in Settings › Privacy & security › Camera.",
                            "Camera access can't be checked without opening the camera, so it is \
                             tried when recording starts; if it shows no picture, check that your \
                             user can open /dev/video* (`isocast permissions` reports access).",
                        )
                        .into(),
                    );
                }
                SourceKind::Camera(CameraSource {
                    device,
                    width: None,
                    height: None,
                    fps: None,
                })
            }
            refused => {
                notes.push(format!(
                    "Camera access is {}, so the camera is a placeholder (a plain colour): \
                     allow it (`isocast permissions` says where), then run `isocast setup \
                     init` again.",
                    refused.label()
                ));
                placeholder()
            }
        };
        sources.push(source("camera", "Camera", kind));
    } else if options.camera.is_none() {
        notes.push("No camera was found, so the setup has only the screen.".into());
    }

    // The microphone and desktop audio.
    let mut audio = Vec::new();
    let microphone = match options.microphone.as_deref() {
        Some("none") => None,
        None | Some("default") => devices
            .microphones
            .iter()
            .find(|m| m.default)
            .map(|m| (m, "default".to_string())),
        Some(query) => {
            let found =
                find(&devices.microphones, query, |m| (&m.id, &m.name)).ok_or_else(|| {
                    format!(
                        "microphone {query:?} matches no microphone; microphones: {}, \
                         \"default\" or \"none\"",
                        list(devices.microphones.iter().map(|m| (&m.name, &m.id)))
                    )
                })?;
            let names = devices.microphones.iter();
            let device = match names.filter(|m| m.name == found.name).count() {
                1 => found.name.clone(),
                _ => found.id.clone(),
            };
            Some((found, device))
        }
    };
    match (microphone, permissions.microphone) {
        (None, _) if options.microphone.as_deref() == Some("none") => {}
        (None, _) => notes.push("No microphone was found, so there is none in the setup.".into()),
        (
            Some((found, device)),
            Permission::Granted | Permission::NotDetermined | Permission::Unknown,
        ) => {
            notes.push(if device == "default" {
                format!(
                    "Microphone: the system's default input, now {:?}.",
                    found.name
                )
            } else {
                format!("Microphone: {:?}.", found.name)
            });
            if permissions.microphone == Permission::NotDetermined {
                notes.push(
                    crate::platform_text::select_linux("macOS asks for microphone access the first time it is used; `isocast \
                     permissions --request` asks now.", "Windows Microphone privacy settings must allow desktop apps. isocastd.exe is the recording engine; `isocast permissions` reports access.",
"Linux records microphones through PipeWire; `isocast permissions` reports access.")
                        .into(),
                );
            }
            if permissions.microphone == Permission::Unknown {
                notes.push(
                    crate::platform_text::select_linux(
                        "Microphone access can't be checked without using it, so it is tried \
                         when recording starts; if it records silence, allow microphone access \
                         in the system's privacy settings.",
                        "Microphone access can't be checked without using it, so it is tried \
                         when recording starts; if it records silence, turn on microphone \
                         access for desktop apps in Settings › Privacy & security › Microphone.",
                        "Linux records microphones through PipeWire, which asks no permission; \
                         if it records silence, check the input in the desktop's sound settings.",
                    )
                    .into(),
                );
            }
            sources.push(source(
                "mic",
                "Mic",
                SourceKind::Microphone(MicrophoneSource::new(device)),
            ));
            audio.push("mic".to_string());
        }
        (Some(_), refused) => notes.push(format!(
            "Microphone access is {}, so there is no microphone: allow it (`isocast \
             permissions` says where), then run `isocast setup init` again.",
            refused.label()
        )),
    }
    if options.system_audio.unwrap_or(true) && screen_allowed {
        sources.push(source(
            "desktop-audio",
            "Desktop Audio",
            SourceKind::SystemAudio(SystemAudioSource::default()),
        ));
        notes.push(format!(
            "Desktop audio: what this {} plays, in the program.",
            crate::platform_text::MACHINE
        ));
    }
    let program_audio: Vec<String> = sources
        .iter()
        .filter(|s| s.kind.has_audio())
        .map(|s| s.id.clone())
        .collect();

    let canvas = Canvas { width, height, fps };
    let scenes = scenes(&canvas, has_camera, options.layout.unwrap_or_default());
    let folder = options
        .folder
        .clone()
        .unwrap_or_else(|| DEFAULT_FOLDER.into());
    notes.push(format!(
        "Records to {folder}: the program ({width}×{height} at {fps} fps, {}){}.",
        crate::platform_text::select_linux(
            "HEVC on the media engine",
            "HEVC on the media engine",
            "H.264"
        ),
        if iso.is_empty() {
            String::new()
        } else {
            format!(
                ", and {} at full resolution{}",
                match iso.len() {
                    1 => format!("the {} in its own ISO file", iso[0]),
                    _ => format!("the {} each in its own ISO file", iso.join(" and ")),
                },
                if audio.is_empty() {
                    ""
                } else {
                    " with the mic"
                }
            )
        }
    ));
    let setup = Setup {
        virtual_camera: Default::default(),
        developer: Default::default(),
        governor: Default::default(),
        version: SETUP_VERSION,
        name: options.name.clone().unwrap_or_else(|| "My setup".into()),
        canvas,
        sources,
        scenes,
        keyframes: Vec::new(),
        // OBS's default transition.
        transition: Transition::fade(300),
        audio: AudioSetup::default(),
        // Streaming is set up when wanted (a service and its key).
        stream: None,
        vertical: None,
        canvases: Default::default(),
        hotkeys: Default::default(),
        recording: Recording {
            folder,
            program: ProgramRecording {
                enabled: true,
                filename: "{date} {time}".into(),
                encoder: Encoder::default(),
                audio: program_audio,
                width: None,
                height: None,
            },
            iso: IsoRecording {
                enabled: !iso.is_empty(),
                sources: iso,
                audio: Some(audio),
                ..IsoRecording::default()
            },
            keep_free_gb: 1.0,
            replay: Default::default(),
            open_editor: serde::de::IgnoredAny,
        },
    };
    let problems = setup.validate();
    Ok(Initialized {
        setup,
        notes,
        problems,
        applied: None,
    })
}

/// A source with default audio settings and no filters.
fn source(id: &str, name: &str, kind: SourceKind) -> Source {
    Source {
        id: id.into(),
        name: Some(name.into()),
        kind,
        audio: AudioSettings::default(),
        filters: Vec::new(),
        audio_filters: Vec::new(),
        encoder: SourceEncoder::default(),
        unknown: UnknownFields::new(),
    }
}

/// What stands in for a device Isocast may not use yet.
fn placeholder() -> SourceKind {
    SourceKind::Color(ColorSource {
        color: Color("#1e1e24".into()),
        width: None,
        height: None,
    })
}

/// The best camera for a first setup: a real one that can deliver a
/// picture, in the order external (USB), Continuity Camera, built-in;
/// then virtual and Desk View cameras; never an iPhone or iPad screen.
fn best_camera(cameras: &[Camera]) -> Option<&Camera> {
    let rank = |camera: &Camera| {
        let kind = match camera.kind {
            CameraKind::External => 0,
            CameraKind::Continuity => 1,
            CameraKind::BuiltIn => 2,
            CameraKind::Other => 3,
            CameraKind::Virtual => 4,
            CameraKind::DeskView => 5,
            CameraKind::DeviceScreen => return None,
        };
        Some(kind + if camera.suspended { 10 } else { 0 })
    };
    cameras
        .iter()
        .filter_map(|camera| Some((rank(camera)?, camera)))
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, camera)| camera)
}

/// A camera's name, or its id if another camera has the same name.
fn unique_name(camera: &Camera, cameras: &[Camera]) -> String {
    match cameras.iter().filter(|c| c.name == camera.name).count() {
        1 => camera.name.clone(),
        _ => camera.id.clone(),
    }
}

/// Find a device by id, exact name, name ignoring case, or the shortest
/// name containing `query` (as capture does).
fn find<'a, T>(
    items: &'a [T],
    query: &str,
    key: impl Fn(&T) -> (&String, &String),
) -> Option<&'a T> {
    let lower = query.to_lowercase();
    items
        .iter()
        .find(|item| {
            let (id, name) = key(item);
            id == query || name == query
        })
        .or_else(|| {
            items
                .iter()
                .find(|item| key(item).1.to_lowercase() == lower)
        })
        .or_else(|| {
            items
                .iter()
                .filter(|item| key(item).1.to_lowercase().contains(&lower))
                .min_by_key(|item| key(item).1.len())
        })
}

/// `"Name" (id X), …` for error messages.
fn list<'a>(items: impl Iterator<Item = (&'a String, &'a String)>) -> String {
    let items: Vec<String> = items
        .map(|(name, id)| format!("{name:?} (id {id})"))
        .collect();
    if items.is_empty() {
        "none".into()
    } else {
        items.join(", ")
    }
}

/// "Screen + Camera", "Camera" and "Screen", or just "Screen" without a
/// camera, sized for `canvas`.
fn scenes(canvas: &Canvas, camera: bool, layout: Layout) -> Vec<Scene> {
    let (w, h) = (canvas.width, canvas.height);
    // Sizes are designed for 1080 pixels on the canvas's shorter side.
    let px = |value: f32| (value * w.min(h) as f32 / 1080.0).round();
    let full = Rect {
        x: 0,
        y: 0,
        width: w,
        height: h,
    };
    let item = |source: &str, rect: Rect, fit: Fit| SceneItem {
        id: None,
        source: Some(source.into()),
        scene: None,
        content: None,
        rect: Some(rect),
        layout: None,
        crop: Crop::default(),
        fit,
        align: Align::Center,
        scale_filter: ScaleFilter::Bilinear,
        blend: Blend::Normal,
        visible: true,
        locked: false,
        show_transition: None,
        hide_transition: None,
        animations: Vec::new(),
        style: Style::default(),
        children: Vec::new(),
    };
    let rounded = Style {
        radius: Radius::All(px(16.0)),
        shadow: Some(Shadow {
            x: 0.0,
            y: px(4.0),
            blur: px(24.0),
            color: Color("#00000080".into()),
        }),
        ..Style::default()
    };
    let scene = |id: &str, name: &str, items| Scene {
        id: id.into(),
        name: Some(name.into()),
        layout: None,
        items,
        transition: None,
        vertical: None,
        canvases: Default::default(),
    };
    let screen = scene("screen", "Screen", vec![item("screen", full, Fit::Contain)]);
    if !camera {
        return vec![screen];
    }
    let margin = px(40.0) as i32;
    let backdrop = SceneItem {
        id: Some("backdrop".into()),
        source: None,
        style: Style {
            background: Some(Color("#141418".into())),
            ..Style::default()
        },
        ..item("", full, Fit::Stretch)
    };
    let both = match layout {
        // A wide screen fills only a band of a tall canvas, so a portrait
        // canvas stacks the screen and the camera, the camera below (or
        // above, for the top layouts) filling the rest.
        _ if h > w => {
            let width = w - 2 * margin as u32;
            let screen_height = (width as f32 * 9.0 / 16.0).round() as u32;
            let camera_height = h - screen_height - 3 * margin as u32;
            let camera_on_top = matches!(layout, Layout::TopLeft | Layout::TopRight);
            let (screen_y, camera_y) = match camera_on_top {
                true => (2 * margin + camera_height as i32, margin),
                false => (margin, 2 * margin + screen_height as i32),
            };
            let rect = |y, height| Rect {
                x: margin,
                y,
                width,
                height,
            };
            vec![
                backdrop,
                SceneItem {
                    style: rounded.clone(),
                    ..item("screen", rect(screen_y, screen_height), Fit::Contain)
                },
                SceneItem {
                    style: rounded,
                    ..item("camera", rect(camera_y, camera_height), Fit::Cover)
                },
            ]
        }
        Layout::SideBySide => {
            let room = w as i32 - 3 * margin;
            let screen_width = (room as f32 * 0.72).round() as u32;
            let picture_height = (screen_width as f32 * 9.0 / 16.0).round() as u32;
            let top = (h as i32 - picture_height as i32) / 2;
            let screen_rect = Rect {
                x: margin,
                y: top,
                width: screen_width,
                height: picture_height,
            };
            let camera_rect = Rect {
                x: 2 * margin + screen_width as i32,
                y: top,
                width: room as u32 - screen_width,
                height: picture_height,
            };
            vec![
                backdrop,
                SceneItem {
                    style: rounded.clone(),
                    ..item("screen", screen_rect, Fit::Contain)
                },
                SceneItem {
                    style: rounded,
                    ..item("camera", camera_rect, Fit::Cover)
                },
            ]
        }
        corner => {
            let width = (w / 4) & !1;
            let height = ((width as f32 * 9.0 / 16.0).round() as u32) & !1;
            let left = matches!(corner, Layout::BottomLeft | Layout::TopLeft);
            let top = matches!(corner, Layout::TopLeft | Layout::TopRight);
            let rect = Rect {
                x: if left {
                    margin
                } else {
                    w as i32 - width as i32 - margin
                },
                y: if top {
                    margin
                } else {
                    h as i32 - height as i32 - margin
                },
                width,
                height,
            };
            vec![
                item("screen", full, Fit::Contain),
                SceneItem {
                    style: rounded,
                    ..item("camera", rect, Fit::Cover)
                },
            ]
        }
    };
    vec![
        scene("screen-and-camera", "Screen + Camera", both),
        scene("camera", "Camera", vec![item("camera", full, Fit::Cover)]),
        screen,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{Display, Microphone, Permissions};

    #[test]
    fn starting_cadence_is_conservative_without_changing_explicit_options() {
        let mut machine = crate::api::Machine {
            chip: "backend-neutral".into(),
            memory_gb: 24.0,
            cores: 8,
            os: String::new(),
            hardware_encoders: Vec::new(),
        };
        assert_eq!(starting_fps(&machine), 30);
        machine.cores = 16;
        machine.memory_gb = 8.0;
        assert_eq!(starting_fps(&machine), 30);
        machine.memory_gb = 32.0;
        assert_eq!(starting_fps(&machine), 60);
        let options = InitOptions {
            fps: Some(60),
            ..Default::default()
        };
        let made = init(&devices(Vec::new(), Permission::Denied), &options).unwrap();
        assert_eq!(made.setup.canvas.fps, 60);
        assert!(!made.setup.governor.allow_iso_thinning);
    }

    fn camera(name: &str, kind: CameraKind, suspended: bool) -> Camera {
        Camera {
            id: format!("id-{name}"),
            name: name.into(),
            model: String::new(),
            manufacturer: String::new(),
            kind,
            formats: Vec::new(),
            suspended,
        }
    }

    fn devices(cameras: Vec<Camera>, permission: Permission) -> Devices {
        Devices {
            displays: vec![Display {
                id: "D1".into(),
                name: "Studio Display".into(),
                main: true,
                built_in: false,
                width: 5120,
                height: 2880,
                refresh_rate: 60.0,
            }],
            windows: Vec::new(),
            cameras,
            microphones: vec![Microphone {
                id: "M1".into(),
                name: "MacBook Pro Microphone".into(),
                model: String::new(),
                manufacturer: String::new(),
                default: true,
                sample_rates: vec![48000.0],
                channels: 1,
                channel_names: Vec::new(),
            }],
            audio_outputs: Vec::new(),
            audio_apps: Vec::new(),
            permissions: Permissions {
                screen_recording: permission,
                camera: permission,
                microphone: permission,
                system_audio: permission,
            },
            notes: Vec::new(),
        }
    }

    #[test]
    fn unknown_access_notes_speak_for_this_os_only() {
        let cameras = vec![camera("Webcam", CameraKind::External, false)];
        let made = init(
            &devices(cameras, Permission::Unknown),
            &InitOptions::default(),
        )
        .unwrap();
        let notes = made.notes.join("\n");
        let (own, others): (&[&str], &[&str]) = if cfg!(target_os = "macos") {
            (
                &["system's privacy settings", "HEVC on the media engine"],
                &["Windows", "Linux", "/dev/video"],
            )
        } else if cfg!(windows) {
            (
                &[
                    "Privacy & security › Camera",
                    "Privacy & security › Microphone",
                    "HEVC on the media engine",
                ],
                &["macOS", "Linux", "/dev/video", "system's privacy settings"],
            )
        } else {
            (
                &[
                    "screen-sharing dialog",
                    "/dev/video*",
                    "PipeWire, which asks no permission",
                    "fps, H.264)",
                ],
                &[
                    "macOS",
                    "Windows",
                    "privacy settings",
                    "Privacy & security",
                    "HEVC",
                ],
            )
        };
        for words in own {
            assert!(notes.contains(words), "{words:?} missing from {notes}");
        }
        for words in others {
            assert!(!notes.contains(words), "{words:?} in {notes}");
        }
    }

    #[test]
    fn the_best_camera_is_real_external_then_continuity_then_built_in() {
        let cameras = vec![
            camera("OBS Virtual Camera", CameraKind::Virtual, false),
            camera("MacBook Pro Camera", CameraKind::BuiltIn, false),
            camera("iPhone Camera", CameraKind::Continuity, false),
            camera("Desk View", CameraKind::DeskView, false),
            camera("iPhone", CameraKind::DeviceScreen, false),
            camera("Insta360 X3", CameraKind::External, false),
        ];
        assert_eq!(best_camera(&cameras).unwrap().name, "Insta360 X3");
        assert_eq!(best_camera(&cameras[..3]).unwrap().name, "iPhone Camera");
        // A closed laptop's camera loses to a virtual one.
        let closed = vec![
            camera("MacBook Pro Camera", CameraKind::BuiltIn, true),
            camera("OBS Virtual Camera", CameraKind::Virtual, false),
        ];
        assert_eq!(best_camera(&closed).unwrap().name, "OBS Virtual Camera");
        assert!(best_camera(&cameras[4..5]).is_none());
    }

    #[test]
    fn a_granted_mac_gets_three_valid_scenes_and_iso_files() {
        let devices = devices(
            vec![camera("Insta360 X3", CameraKind::External, false)],
            Permission::Granted,
        );
        let made = init(&devices, &InitOptions::default()).unwrap();
        assert!(made.problems.is_empty(), "{:?}", made.problems);
        let setup = &made.setup;
        let scenes: Vec<&str> = setup.scenes.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(scenes, ["screen-and-camera", "camera", "screen"]);
        assert_eq!(setup.recording.iso.sources, ["screen", "camera"]);
        assert_eq!(setup.recording.program.audio, ["mic", "desktop-audio"]);
        assert_eq!(setup.recording.iso.audio.as_deref().unwrap(), ["mic"]);
        let pip = setup.scenes[0].items[1].rect.unwrap();
        assert_eq!((pip.x, pip.y, pip.width, pip.height), (1400, 770, 480, 270));
        // It survives being written and read as TOML.
        let text = setup.to_toml().unwrap();
        assert_eq!(&Setup::from_toml(&text).unwrap(), setup);
    }

    #[test]
    fn refused_permissions_leave_placeholders_and_a_valid_setup() {
        let devices = devices(
            vec![camera("Insta360 X3", CameraKind::External, false)],
            Permission::Denied,
        );
        let made = init(&devices, &InitOptions::default()).unwrap();
        assert!(made.problems.is_empty(), "{:?}", made.problems);
        let kinds: Vec<&str> = made.setup.sources.iter().map(|s| s.kind.label()).collect();
        assert_eq!(kinds, ["color", "color"]);
        assert!(!made.setup.recording.iso.enabled);
        assert!(made.setup.recording.program.audio.is_empty());
        assert!(made.notes.iter().any(|n| n.contains("Screen recording")));
    }

    #[test]
    fn no_camera_means_one_scene_and_bad_choices_list_the_valid_ones() {
        let devices = devices(Vec::new(), Permission::Granted);
        let made = init(&devices, &InitOptions::default()).unwrap();
        assert!(made.problems.is_empty(), "{:?}", made.problems);
        assert_eq!(made.setup.scenes.len(), 1);
        let options = InitOptions {
            microphone: Some("Wave".into()),
            ..InitOptions::default()
        };
        let error = init(&devices, &options).unwrap_err();
        assert!(error.contains("\"MacBook Pro Microphone\""), "{error}");
        let side = InitOptions {
            layout: Some(Layout::SideBySide),
            width: Some(1280),
            height: Some(720),
            ..InitOptions::default()
        };
        let devices = super::tests::devices(
            vec![camera("Cam", CameraKind::BuiltIn, false)],
            Permission::Granted,
        );
        let made = init(&devices, &side).unwrap();
        assert!(made.problems.is_empty(), "{:?}", made.problems);
        // A portrait canvas stacks the screen above the camera.
        let tall = InitOptions {
            width: Some(1080),
            height: Some(1920),
            ..InitOptions::default()
        };
        let made = init(&devices, &tall).unwrap();
        assert!(made.problems.is_empty(), "{:?}", made.problems);
        let rects: Vec<[i64; 4]> = made.setup.scenes[0].items[1..]
            .iter()
            .map(|i| {
                let r = i.rect.unwrap();
                [r.x.into(), r.y.into(), r.width.into(), r.height.into()]
            })
            .collect();
        assert_eq!(rects, [[40, 40, 1000, 563], [40, 643, 1000, 1237]]);
    }
}
