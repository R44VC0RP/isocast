//! Importing OBS Studio scene collections.
//!
//! OBS saves each scene collection as JSON (on a Mac in
//! `~/Library/Application Support/obs-studio/basic/scenes/NAME.json`), and
//! the canvas, frame rate and recording settings in a profile's
//! `basic.ini`. [`import`] turns a collection into a setup: its sources
//! (with ids made from their names), scenes and their items (groups become
//! children, scenes inside scenes nested scene items), filters and the
//! transition, wherever Isocast has an equivalent, and lists what it left
//! out. The settings' meanings come from OBS's public documentation and the
//! files themselves.
//!
//! An item's size in OBS is its source's size times its scale, and the
//! collection doesn't say how big a camera, display or image is. The caller
//! supplies what it can find out ([`Options::devices`],
//! [`Options::content_size`]); anything else is assumed and noted.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::devices::Devices;
use crate::stream::{AUDIO_BITRATES, StreamService, StreamSettings};
use crate::*;

/// What [`import`] made of a scene collection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Imported {
    pub setup: Setup,
    /// What wasn't brought over, or only partly, each naming the OBS
    /// source, filter, item or setting and why.
    pub unsupported: Vec<Unsupported>,
    /// Assumptions the import made, like a source size it couldn't find
    /// out; worth checking.
    pub notes: Vec<String>,
    /// Problems `setup check` would report in the result; none normally.
    pub problems: Vec<Issue>,
    pub counts: Counts,
    /// The collection file read, if it came from a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    /// The OBS profile the canvas, frame rate and recording settings came
    /// from, if one was found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// Something from the collection Isocast couldn't bring over as it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Unsupported {
    /// Where, by OBS names: `source "Mic"`, `source "Mic" filter "Gain"`,
    /// `scene "Live" item "Logo"` or `transition "Stinger"`.
    pub obs: String,
    /// OBS's id for the kind of thing, like `noise_suppress_filter`.
    pub kind: String,
    pub reason: String,
}

/// How much of the collection came over.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Counts {
    /// OBS sources (not scenes or groups), and how many became sources.
    pub obs_sources: usize,
    pub sources: usize,
    pub scenes: usize,
    /// Items in OBS scenes and groups, and how many became items. Items of
    /// audio-only sources have no place in Isocast scenes (the mixer has
    /// them), so they aren't counted as items.
    pub obs_items: usize,
    pub items: usize,
    /// Filters on OBS sources, and how many became filters or recording
    /// settings (Source Record).
    pub obs_filters: usize,
    pub filters: usize,
}

/// The parts of an OBS profile (`basic.ini`) an import uses.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Profile {
    /// The profile's name, as OBS shows it.
    pub name: String,
    /// The base (canvas) resolution.
    pub canvas: Option<(u32, u32)>,
    /// The size recordings are written at: the Output (Scaled) Resolution,
    /// or an Advanced recording's own rescale.
    pub output: Option<(u32, u32)>,
    pub fps: Option<f64>,
    /// Where recordings go.
    pub folder: Option<String>,
    pub container: Option<String>,
    pub encoder: Option<String>,
    /// The file name format, like `%CCYY-%MM-%DD %hh-%mm-%ss`.
    pub filename: Option<String>,
    /// The audio tracks recordings get, as OBS's bit mask (track 1 is bit
    /// 0).
    pub tracks: u32,
    /// Where it streams to, from the profile's `service.json` (never its
    /// stream key), and at what bitrates.
    pub stream: Option<ObsStream>,
    /// The stream's bitrates from `basic.ini`, in kb/s.
    stream_video_kbps: Option<u32>,
    stream_audio_kbps: Option<u32>,
    /// Reconnecting after a dropped stream: enabled, seconds between tries,
    /// tries.
    pub reconnect: Option<(bool, u32, u32)>,
    /// The monitoring device's name, unless it is the system default.
    pub monitoring: Option<String>,
    /// Hotkeys with keys bound: the action and its keys, like
    /// `("OBSBasic.StartRecording", "F16")`.
    pub hotkeys: Vec<(String, String)>,
    /// Aitum Multistream's extra outputs for this profile
    /// (`plugin_config/aitum-multistream/config.json`), without keys.
    pub multistream: Vec<PluginOutput>,
    /// Aitum Vertical Canvas's canvases
    /// (`plugin_config/vertical-canvas/config.json`), without keys.
    pub plugin_canvases: Vec<PluginCanvas>,
}

/// A stream output of an OBS plugin (Aitum Multistream, Vertical Canvas),
/// without its key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginOutput {
    pub name: String,
    pub server: String,
    pub enabled: bool,
    pub video_kbps: Option<u32>,
    /// Its own size (Multistream's "scale").
    pub size: Option<(u32, u32)>,
}

/// One of Aitum Vertical Canvas's canvases.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PluginCanvas {
    pub width: u32,
    pub height: u32,
    pub video_kbps: Option<u32>,
    pub audio_kbps: Option<u32>,
    /// Its scenes, by name (the plugin's own; not imported).
    pub scenes: usize,
    pub outputs: Vec<PluginOutput>,
}

/// An OBS profile's stream settings, without the key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ObsStream {
    /// OBS's kind of service: `rtmp_custom`, `rtmp_common`, `whip_custom`…
    pub kind: String,
    /// The service's name for `rtmp_common`, like "Twitch".
    pub service: Option<String>,
    pub server: Option<String>,
    pub video_kbps: Option<u32>,
    pub audio_kbps: Option<u32>,
}

impl Profile {
    /// Add what the profile's folder has besides `basic.ini`: the stream
    /// service (`service.json`) and an advanced stream encoder's bitrate
    /// (`streamEncoder.json`). The stream key in `service.json` is not
    /// kept.
    fn read_stream(&mut self, folder: &Path) {
        self.read_plugins(folder);
        let read = |name: &str| -> Option<Value> {
            serde_json::from_str(&std::fs::read_to_string(folder.join(name)).ok()?).ok()
        };
        let Some(service) = read("service.json") else {
            return;
        };
        let settings = service.get("settings").unwrap_or(&Value::Null);
        let encoder = read("streamEncoder.json");
        let video = encoder
            .as_ref()
            .and_then(|e| int(e, "bitrate"))
            .and_then(|b| u32::try_from(b).ok())
            .or(self.stream_video_kbps);
        self.stream = Some(ObsStream {
            kind: text(&service, "type").unwrap_or("rtmp_common").to_string(),
            service: text(settings, "service").map(str::to_string),
            server: text(settings, "server")
                .filter(|s| !s.is_empty() && *s != "auto")
                .map(str::to_string),
            video_kbps: video,
            audio_kbps: self.stream_audio_kbps,
        });
    }

    /// Aitum Multistream's outputs for this profile and Vertical Canvas's
    /// canvases (#84), from OBS's `plugin_config` (three folders above the
    /// profile's: `basic/profiles/<name>`). Keys are never kept.
    fn read_plugins(&mut self, folder: &Path) {
        let Some(root) = folder
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
        else {
            return;
        };
        let read = |path: &str| -> Option<Value> {
            serde_json::from_str(&std::fs::read_to_string(root.join(path)).ok()?).ok()
        };
        let kbps = |value: &Value, key: &str| {
            int(value, key)
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| *n > 0)
        };
        let output = |item: &Value| -> Option<PluginOutput> {
            let server = text(item, "stream_server")?.trim().to_string();
            if server.is_empty() {
                return None;
            }
            let advanced = item.get("advanced").and_then(Value::as_bool) == Some(true);
            let size = (item.get("scale").and_then(Value::as_bool) == Some(true))
                .then(|| {
                    let w = u32::try_from(int(item, "width")?).ok()?;
                    let h = u32::try_from(int(item, "height")?).ok()?;
                    Some((w, h))
                })
                .flatten();
            Some(PluginOutput {
                name: text(item, "name").unwrap_or_default().to_string(),
                server,
                enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(true),
                video_kbps: advanced
                    .then(|| kbps(item.get("video_encoder_settings")?, "bitrate"))
                    .flatten(),
                size,
            })
        };
        let dir = folder
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if let Some(config) = read("plugin_config/aitum-multistream/config.json") {
            let name = self.name.clone();
            let profile = config
                .get("profiles")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|p| text(p, "name").is_some_and(|n| n == name || n == dir));
            self.multistream = profile
                .and_then(|p| p.get("outputs")?.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(output)
                .collect();
        }
        if let Some(config) = read("plugin_config/vertical-canvas/config.json") {
            for canvas in config
                .get("canvas")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let (Some(width), Some(height)) = (
                    int(canvas, "width").and_then(|n| u32::try_from(n).ok()),
                    int(canvas, "height").and_then(|n| u32::try_from(n).ok()),
                ) else {
                    continue;
                };
                let mut outputs: Vec<PluginOutput> = canvas
                    .get("stream_outputs")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(output)
                    .collect();
                // Before stream_outputs: one server of its own.
                if outputs.is_empty()
                    && let Some(single) = output(canvas)
                {
                    outputs.push(PluginOutput {
                        name: "main".into(),
                        video_kbps: None,
                        size: None,
                        ..single
                    });
                }
                self.plugin_canvases.push(PluginCanvas {
                    width,
                    height,
                    video_kbps: kbps(canvas, "streaming_video_bitrate")
                        .or_else(|| kbps(canvas, "video_bitrate")),
                    audio_kbps: kbps(canvas, "audio_bitrate"),
                    scenes: canvas
                        .get("scenes")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len),
                    outputs,
                });
            }
        }
    }

    /// Read a profile's `basic.ini`.
    pub fn parse(text: &str) -> Profile {
        let mut values: HashMap<(String, String), String> = HashMap::new();
        let mut section = String::new();
        for line in text.lines() {
            let line = line.trim().trim_start_matches('\u{feff}');
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.to_string();
            } else if let Some((key, value)) = line.split_once('=') {
                values.insert((section.clone(), key.trim().into()), value.trim().into());
            }
        }
        let get = |section: &str, key: &str| {
            values
                .get(&(section.to_string(), key.to_string()))
                .filter(|v| !v.is_empty())
                .cloned()
        };
        let number = |section: &str, key: &str| get(section, key)?.parse::<f64>().ok();
        let size = |x: &str, y: &str| {
            number("Video", x)
                .zip(number("Video", y))
                .map(|(w, h)| (w as u32, h as u32))
        };
        let canvas = size("BaseCX", "BaseCY");
        let fps = match get("Video", "FPSType").as_deref() {
            Some("1") => number("Video", "FPSInt"),
            Some("2") => number("Video", "FPSNum")
                .zip(number("Video", "FPSDen"))
                .filter(|(_, den)| *den > 0.0)
                .map(|(num, den)| num / den),
            _ => get("Video", "FPSCommon").and_then(|common| common_fps(&common)),
        };
        let advanced = get("Output", "Mode").as_deref() == Some("Advanced");
        let (folder, container, encoder) = if advanced {
            let ffmpeg = get("AdvOut", "RecType").as_deref() == Some("FFmpeg");
            let folder = if ffmpeg {
                get("AdvOut", "FFFilePath")
            } else {
                get("AdvOut", "RecFilePath")
            };
            let encoder = get("AdvOut", "RecEncoder")
                .filter(|e| e != "none")
                .or_else(|| get("AdvOut", "Encoder"));
            let container = get("AdvOut", "RecFormat2").or_else(|| get("AdvOut", "RecFormat"));
            (folder, container, encoder)
        } else {
            let container =
                get("SimpleOutput", "RecFormat2").or_else(|| get("SimpleOutput", "RecFormat"));
            (
                get("SimpleOutput", "FilePath"),
                container,
                get("SimpleOutput", "RecEncoder"),
            )
        };
        let rescaled = (advanced && get("AdvOut", "RecRescale").as_deref() == Some("true"))
            .then(|| {
                get("AdvOut", "RecRescaleRes")
                    .as_deref()
                    .and_then(parse_size)
            })
            .flatten();
        let output = rescaled.or_else(|| size("OutputCX", "OutputCY"));
        // Simple mode records the stream's single track unless the
        // recording has a quality of its own.
        let tracks = if advanced {
            number("AdvOut", "RecTracks")
        } else {
            get("SimpleOutput", "RecQuality")
                .filter(|q| q != "Stream")
                .and(number("SimpleOutput", "RecTracks"))
        };
        let tracks = tracks.map_or(1, |t| t as u32 & 0x3f);
        let kbps = |section: &str, key: &str| number(section, key).map(|n| n as u32);
        let reconnect = get("Output", "Reconnect").map(|on| {
            let int = |key| number("Output", key).map_or(0, |n| n.max(0.0) as u32);
            (on == "true", int("RetryDelay"), int("MaxRetries"))
        });
        let monitoring = get("Audio", "MonitoringDeviceName")
            .filter(|_| get("Audio", "MonitoringDeviceId").as_deref() != Some("default"))
            .filter(|name| !name.eq_ignore_ascii_case("default"));
        let mut hotkeys: Vec<(String, String)> = values
            .iter()
            .filter(|((section, _), _)| section == "Hotkeys")
            .filter_map(|((_, action), value)| Some((action.clone(), hotkey_keys(value)?)))
            .collect();
        hotkeys.sort();
        let (stream_video_kbps, stream_audio_kbps) = if advanced {
            (None, kbps("AdvOut", "Track1Bitrate"))
        } else {
            (
                kbps("SimpleOutput", "VBitrate"),
                kbps("SimpleOutput", "ABitrate"),
            )
        };
        Profile {
            name: get("General", "Name").unwrap_or_default(),
            canvas,
            output,
            fps,
            folder,
            container,
            encoder,
            filename: get("Output", "FilenameFormatting"),
            tracks,
            stream: None,
            multistream: Vec::new(),
            plugin_canvases: Vec::new(),
            stream_video_kbps,
            stream_audio_kbps,
            reconnect,
            monitoring,
            hotkeys,
        }
    }
}

/// The keys bound to an OBS hotkey (`{"bindings":[{"key":"OBS_KEY_F16"}]}`),
/// like `shift+command+F16`; none if nothing is bound.
fn hotkey_keys(value: &str) -> Option<String> {
    let json: Value = serde_json::from_str(value).ok()?;
    let keys: Vec<String> = json
        .get("bindings")?
        .as_array()?
        .iter()
        .filter_map(|binding| {
            let key = text(binding, "key")?;
            let key = key.strip_prefix("OBS_KEY_").unwrap_or(key);
            let mut parts: Vec<&str> = ["shift", "control", "alt", "command"]
                .into_iter()
                .filter(|modifier| flag(binding, modifier) == Some(true))
                .collect();
            parts.push(key);
            Some(parts.join("+"))
        })
        .collect();
    (!keys.is_empty()).then(|| keys.join(" or "))
}

/// What an import couldn't bring over, and assumptions worth checking.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Findings {
    pub unsupported: Vec<Unsupported>,
    pub notes: Vec<String>,
}

impl Findings {
    fn unsupported(&mut self, obs: impl Into<String>, kind: &str, reason: impl Into<String>) {
        self.unsupported.push(Unsupported {
            obs: obs.into(),
            kind: kind.into(),
            reason: reason.into(),
        });
    }
}

/// `path` written `~/...` if it is inside the home folder.
fn home_relative(path: &str, home: Option<&str>) -> String {
    match home {
        Some(home) if !home.is_empty() => match path.strip_prefix(home) {
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_string(),
        },
        _ => path.to_string(),
    }
}

/// The canvas frame rate from the profile's (Isocast's are whole), or
/// `given`.
fn frame_rate(given: Option<u32>, profile: Option<&Profile>, notes: &mut Vec<String>) -> u32 {
    match (given, profile.and_then(|p| p.fps)) {
        (Some(fps), _) => fps,
        (None, Some(fps)) => {
            let whole = fps.round().clamp(1.0, 240.0) as u32;
            if (fps - f64::from(whole)).abs() > 0.001 {
                notes.push(format!(
                    "OBS ran at {fps:.3} fps; Isocast's canvas runs at whole rates, so {whole}"
                ));
            }
            whole
        }
        (None, None) => {
            notes.push("no OBS profile gave the frame rate; using 30 fps, OBS's default".into());
            30
        }
    }
}

/// The recording settings a profile gives: folder, container, codec and
/// file name.
fn recording_settings(
    profile: Option<&Profile>,
    home: Option<&str>,
    findings: &mut Findings,
) -> (String, Container, Codec, String) {
    let folder = match profile.and_then(|p| p.folder.as_deref()) {
        Some(folder) => home_relative(folder, home),
        None => {
            findings.notes.push(format!(
                "no OBS profile gave the recordings folder; using {FALLBACK_FOLDER}"
            ));
            FALLBACK_FOLDER.into()
        }
    };
    let container = match profile.and_then(|p| p.container.as_deref()) {
        Some(format) if format.contains("mp4") => Container::Mp4,
        Some(format) if format.contains("mov") => Container::Mov,
        Some(format) => {
            findings.unsupported(
                "profile recording format",
                format,
                "Isocast writes .mov or .mp4; .mov chosen",
            );
            Container::Mov
        }
        None => Container::Mov,
    };
    let codec = match profile.and_then(|p| p.encoder.as_deref()) {
        Some(e) if e.contains("hevc") || e.contains("h265") || e.contains("265") => Codec::Hevc,
        Some(e) if e.contains("264") || e.contains("avc") => Codec::H264,
        Some(e) if e.contains("prores") => Codec::Prores422,
        _ => Codec::platform_default(),
    };
    let filename = match profile.and_then(|p| p.filename.as_deref()) {
        Some(format) => {
            let converted = format
                .replace("%CCYY-%MM-%DD", "{date}")
                .replace("%hh-%mm-%ss", "{time}");
            if converted.contains('%') {
                findings.unsupported(
                    "profile file name format",
                    "FilenameFormatting",
                    format!(
                        "{format:?} uses codes Isocast doesn't have; it names files \
                         \"{{date}} {{time}}\" (like OBS's default)"
                    ),
                );
                default_program_filename()
            } else {
                converted
            }
        }
        None => default_program_filename(),
    };
    (folder, container, codec, filename)
}

/// The size OBS scaled its canvas to for recording, as the program file's
/// size, when it differs from `canvas` and fits it.
fn program_size(
    profile: &Profile,
    canvas: (u32, u32),
    findings: &mut Findings,
) -> (Option<u32>, Option<u32>) {
    let Some((w, h)) = profile.output.filter(|&output| output != canvas) else {
        return (None, None);
    };
    let fits = |side: u32, limit: u32| (16..=limit).contains(&side) && side.is_multiple_of(2);
    if fits(w, canvas.0) && fits(h, canvas.1) {
        findings.notes.push(format!(
            "OBS recorded at {w}×{h}, scaled from its {}×{} canvas; so does this profile \
             (recording.program.width/height), which needs a canvas at least that big",
            canvas.0, canvas.1
        ));
        (Some(w), Some(h))
    } else {
        findings.unsupported(
            "profile output resolution",
            "OutputCX",
            format!(
                "OBS recorded at {w}×{h}, scaled from its {}×{} canvas; Isocast's program file \
                 is an even size up to the canvas's, so it records at the canvas size",
                canvas.0, canvas.1
            ),
        );
        (None, None)
    }
}

/// The profile's stream service, as a setup's stream (without the key,
/// which stays in OBS).
fn stream_settings(profile: &Profile, findings: &mut Findings) -> Option<StreamSettings> {
    let obs = profile.stream.clone()?;
    let service = match (obs.kind.as_str(), obs.service.as_deref()) {
        ("rtmp_custom", _) => "custom",
        ("rtmp_common", Some(name)) => match name.to_lowercase() {
            n if n.starts_with("twitch") => "twitch",
            n if n.starts_with("youtube") => "youtube",
            n if n.starts_with("kick") => "kick",
            n if n.starts_with("facebook") => "facebook",
            _ => {
                findings.unsupported(
                    "profile stream",
                    "rtmp_common",
                    format!("{name:?} isn't a service Isocast knows; set a custom server"),
                );
                return None;
            }
        },
        (kind, _) => {
            let kind = kind.to_string();
            findings.unsupported(
                "profile stream",
                &kind,
                "only RTMP and RTMPS streaming is available",
            );
            return None;
        }
    };
    let mut settings = serde_json::json!({ "service": service });
    if let Some(server) = &obs.server {
        settings["server"] = server.as_str().into();
    }
    if let Some(kbps) = obs.video_kbps.filter(|k| (100..=100_000).contains(k)) {
        settings["video_bitrate_kbps"] = kbps.into();
    }
    if let Some(kbps) = obs.audio_kbps.filter(|k| AUDIO_BITRATES.contains(k)) {
        settings["audio_bitrate_kbps"] = kbps.into();
    }
    let stream: StreamSettings = serde_json::from_value(settings).ok()?;
    findings.notes.push(format!(
        "the profile streams to {}; its stream key stays in OBS: set it with `isocast stream key \
         set` (MCP: set_stream_key)",
        obs.server.as_deref().unwrap_or(service)
    ));
    Some(stream)
}

/// A plugin output's service and server: a preset when its server is one
/// of the services' (Twitch, YouTube, Kick, Facebook), else custom. None
/// for what isn't RTMP or RTMPS (WHIP, SRT).
fn plugin_service(server: &str) -> Option<(StreamService, Option<String>)> {
    let lower = server.to_ascii_lowercase();
    if !(lower.starts_with("rtmp://") || lower.starts_with("rtmps://")) {
        return None;
    }
    Some(
        if lower.contains("twitch.tv") || lower.contains("live-video.net") {
            (StreamService::Twitch, None)
        } else if lower.contains("youtube.com") {
            (StreamService::Youtube, None)
        } else if lower.contains("facebook.com") {
            (StreamService::Facebook, None)
        } else if lower.contains("kick") {
            (StreamService::Kick, Some(server.to_string()))
        } else {
            (StreamService::Custom, Some(server.to_string()))
        },
    )
}

/// Plugin outputs as destinations of `stream` (made, with the first as
/// its main destination, if there is none): Aitum Multistream's onto
/// the main canvas's `[stream]`, Vertical Canvas's onto a canvas's.
/// `what` names them in findings. Keys stay in OBS.
fn plugin_destinations(
    outputs: &[PluginOutput],
    stream: &mut Option<StreamSettings>,
    defaults: (Option<u32>, Option<u32>),
    what: &str,
    findings: &mut Findings,
) {
    use crate::stream::{MAX_DESTINATIONS, StreamDestination};
    let mut imported = 0;
    for output in outputs {
        let Some((service, server)) = plugin_service(&output.server) else {
            findings.unsupported(
                format!("{what} output {:?}", output.name),
                "stream_server",
                "only RTMP and RTMPS outputs are available (not WHIP or SRT)",
            );
            continue;
        };
        let size = output.size.filter(|(w, h)| {
            (16..=4096).contains(w)
                && (16..=4096).contains(h)
                && w.is_multiple_of(2)
                && h.is_multiple_of(2)
        });
        let kbps = output
            .video_kbps
            .or(defaults.0)
            .filter(|k| (200..=50_000).contains(k));
        match stream {
            None => {
                let mut settings = serde_json::json!({ "service": service.id() });
                if let Some(server) = &server {
                    settings["server"] = server.as_str().into();
                }
                if let Some(kbps) = kbps {
                    settings["video_bitrate_kbps"] = kbps.into();
                }
                if let Some(kbps) = defaults.1.filter(|k| AUDIO_BITRATES.contains(k)) {
                    settings["audio_bitrate_kbps"] = kbps.into();
                }
                if let Some((w, h)) = size {
                    settings["width"] = w.into();
                    settings["height"] = h.into();
                }
                let Ok(mut made) = serde_json::from_value::<StreamSettings>(settings) else {
                    continue;
                };
                made.enabled = output.enabled;
                *stream = Some(made);
                imported += 1;
            }
            Some(stream) => {
                if stream.destinations.len() + 1 >= MAX_DESTINATIONS {
                    findings.unsupported(
                        format!("{what} output {:?}", output.name),
                        "outputs",
                        format!("at most {MAX_DESTINATIONS} destinations per canvas"),
                    );
                    continue;
                }
                let taken = stream.destination_ids();
                let base = crate::library::slug(&output.name, service.id());
                let base = if base == crate::stream::MAIN_DESTINATION {
                    format!("{base}-2")
                } else {
                    base
                };
                let id = (1..)
                    .map(|n| {
                        if n == 1 {
                            base.clone()
                        } else {
                            format!("{base}-{n}")
                        }
                    })
                    .find(|id| !taken.contains(id))
                    .expect("a free id");
                stream.destinations.push(StreamDestination {
                    id,
                    service,
                    server,
                    enabled: output.enabled,
                    video_bitrate_kbps: output.video_kbps.filter(|k| (200..=50_000).contains(k)),
                    width: size.map(|s| s.0),
                    height: size.map(|s| s.1),
                    rate_control: None,
                    keyframe_interval_seconds: None,
                    audio_bitrate_kbps: None,
                });
                imported += 1;
            }
        }
    }
    if imported > 0 {
        findings.notes.push(format!(
            "{what}'s {imported} output(s) became stream destinations; their keys stay in OBS: \
             set each with `isocast stream key set --destination ID` (MCP: set_stream_key)"
        ));
    }
}

/// Aitum Vertical Canvas's canvases as Isocast canvases: the first as
/// `[vertical]`, more as `[[canvases]]` (`vertical-2`…), each following
/// the main scene (letterboxed until scenes get arrangements for it) with
/// its outputs as its stream destinations.
fn plugin_canvases(
    profile: &Profile,
    findings: &mut Findings,
) -> (Option<crate::VerticalCanvas>, Vec<crate::VerticalCanvas>) {
    let mut vertical = None;
    let mut canvases = Vec::new();
    for (i, plugin) in profile.plugin_canvases.iter().enumerate() {
        let even = |n: u32| (16..=8192).contains(&n) && n.is_multiple_of(2);
        if !even(plugin.width) || !even(plugin.height) {
            findings.unsupported(
                format!("Vertical Canvas {}", i + 1),
                "width",
                format!(
                    "{}×{} isn't an even size from 16 to 8192",
                    plugin.width, plugin.height
                ),
            );
            continue;
        }
        let mut canvas = if i == 0 {
            crate::VerticalCanvas::default()
        } else {
            crate::VerticalCanvas::with_id(&format!("vertical-{}", i + 1))
        };
        canvas.width = plugin.width;
        canvas.height = plugin.height;
        let what = format!("Vertical Canvas {}", i + 1);
        plugin_destinations(
            &plugin.outputs,
            &mut canvas.stream,
            (plugin.video_kbps, plugin.audio_kbps),
            &what,
            findings,
        );
        if plugin.scenes > 0 {
            findings.unsupported(
                format!("{what} scenes"),
                "scenes",
                format!(
                    "its {} scene(s) of its own aren't imported: the canvas follows the main \
                     scenes, letterboxed, until each gets an arrangement for it",
                    plugin.scenes
                ),
            );
        }
        if i == 0 {
            vertical = Some(canvas);
        } else {
            canvases.push(canvas);
        }
    }
    (vertical, canvases)
}

/// A profile's reconnect settings (onto `stream`, if it streams) and
/// monitoring device.
fn profile_extras(
    profile: &Profile,
    stream: &mut Option<StreamSettings>,
    audio: &mut AudioSetup,
    findings: &mut Findings,
) {
    if let (Some(stream), Some((enabled, delay, retries))) = (stream, profile.reconnect) {
        stream.reconnect.enabled = enabled;
        if (1..=600).contains(&delay) {
            stream.reconnect.delay_seconds = delay;
        }
        if (1..=1000).contains(&retries) {
            stream.reconnect.max_retries = retries;
        }
    }
    if let Some(device) = &profile.monitoring {
        audio.monitoring_device = device.clone();
        findings.notes.push(format!(
            "monitored sources play through {device:?}, as in OBS (`isocast devices` lists the \
             outputs)"
        ));
    }
}

/// An OBS profile made into an Isocast profile.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedProfile {
    pub profile: crate::library::Profile,
    pub unsupported: Vec<Unsupported>,
    pub notes: Vec<String>,
    /// The basic.ini read.
    pub path: PathBuf,
}

/// Read an OBS profile (`profile`: its name, folder or basic.ini path)
/// into an Isocast profile: frame rate, recording, the program file's size
/// (OBS's Output (Scaled) Resolution), stream (without its key), reconnect
/// and monitoring; hotkeys are listed as not imported.
pub fn import_profile(profile: &str, home: Option<&str>) -> Result<ImportedProfile, String> {
    let home_path = home.map(PathBuf::from);
    let obs = home_path.as_deref().map(obs_folder);
    let (path, read) = read_profile(profile, home_path.as_deref(), obs.as_deref())?;
    let (profile, findings) = profile_from(&read, &path, home);
    Ok(ImportedProfile {
        profile,
        unsupported: findings.unsupported,
        notes: findings.notes,
        path,
    })
}

/// An Isocast profile from a parsed OBS profile read from `path`.
fn profile_from(
    obs: &Profile,
    path: &Path,
    home: Option<&str>,
) -> (crate::library::Profile, Findings) {
    use crate::library::{ProfileCanvas, ProfileIso, ProfileProgram, ProfileRecording};
    let mut findings = Findings::default();
    let fps = frame_rate(None, Some(obs), &mut findings.notes);
    let (folder, container, codec, filename) = recording_settings(Some(obs), home, &mut findings);
    let (width, height) = match obs.canvas {
        Some(canvas) => program_size(obs, canvas, &mut findings),
        None => (None, None),
    };
    let mut stream = stream_settings(obs, &mut findings);
    let mut audio = AudioSetup::default();
    profile_extras(obs, &mut stream, &mut audio, &mut findings);
    plugin_destinations(
        &obs.multistream,
        &mut stream,
        (None, None),
        "Aitum Multistream",
        &mut findings,
    );
    // Vertical Canvas: the profile keeps the outputs; the scene collection
    // decides whether the canvases are there.
    let (vertical, canvases) = plugin_canvases(obs, &mut findings);
    let split = |canvas: crate::VerticalCanvas| crate::library::ProfileVertical {
        id: canvas.id,
        fps: canvas.fps,
        recording: canvas.recording,
        stream: canvas.stream,
    };
    if vertical.is_some() {
        findings.notes.push(
            "Vertical Canvas's outputs are the profile's; a scene collection without its \
             canvases gets them with `isocast canvas add vertical --size WxH` (MCP: canvas_add)"
                .into(),
        );
    }
    let profile_vertical = vertical.map(split);
    let profile_canvases: Vec<crate::library::ProfileVertical> =
        canvases.into_iter().map(split).collect();
    let tracks = obs.tracks.max(1);
    if tracks.count_ones() > 1 {
        let numbers: Vec<String> = (0..6)
            .filter(|bit| tracks & (1 << bit) != 0)
            .map(|bit| (bit + 1).to_string())
            .collect();
        findings.unsupported(
            "profile recording audio tracks",
            "RecTracks",
            format!(
                "OBS recorded audio tracks {}; Isocast records one program mix, the scene \
                 collection's recording.program.audio",
                numbers.join(", ")
            ),
        );
    }
    for (action, keys) in &obs.hotkeys {
        findings.unsupported(
            format!("profile hotkey {action}"),
            "hotkey",
            format!(
                "bound to {keys}; Isocast's keys are fixed for now (Settings → Hotkeys lists \
                 them)"
            ),
        );
    }
    let name = if obs.name.is_empty() {
        dir_name(path).to_string()
    } else {
        obs.name.clone()
    };
    let profile = crate::library::Profile {
        hotkeys: Default::default(),
        developer: Default::default(),
        governor: Default::default(),
        version: SETUP_VERSION,
        name,
        canvas: ProfileCanvas { fps },
        audio,
        recording: ProfileRecording {
            folder,
            program: ProfileProgram {
                enabled: true,
                filename,
                encoder: Encoder {
                    codec,
                    container,
                    ..Encoder::default()
                },
                width,
                height,
            },
            // The scene collection's ISO sources, if it has any.
            iso: ProfileIso {
                enabled: true,
                encoder: IsoEncoder {
                    container,
                    ..IsoEncoder::default()
                },
                ..ProfileIso::default()
            },
            keep_free_gb: default_keep_free_gb(),
            replay: Default::default(),
            open_editor: serde::de::IgnoredAny,
        },
        stream,
        vertical: profile_vertical,
        canvases: profile_canvases,
    };
    (profile, findings)
}

/// OBS's common frame rates, as its profiles write them.
fn common_fps(text: &str) -> Option<f64> {
    match text {
        "24 NTSC" => Some(24_000.0 / 1001.0),
        "29.97" => Some(30_000.0 / 1001.0),
        "59.94" => Some(60_000.0 / 1001.0),
        other => other.split_whitespace().next()?.parse().ok(),
    }
}

/// What an import can find out about this machine.
pub struct Options<'a> {
    /// Import onto a canvas of this size, scaling positions and sizes about
    /// the centre by the change in height, as OBS does when its canvas
    /// changes; the collection's own size otherwise.
    pub canvas: Option<(u32, u32)>,
    /// The canvas frame rate; the profile's otherwise.
    pub fps: Option<u32>,
    pub profile: Option<&'a Profile>,
    /// The displays, cameras and microphones here, to match OBS's device
    /// ids with and to know their sizes.
    pub devices: Option<&'a Devices>,
    /// The size of a source's picture when it comes from a file or its text
    /// (images, slide shows, text, media), if it can be read.
    pub content_size: &'a dyn Fn(&SourceKind) -> Option<(u32, u32)>,
    /// The home folder, so paths inside it are written `~/...`.
    pub home: Option<&'a str>,
}

/// Look for OBS's settings in this folder instead of the home folder's
/// (isolated engines and tests, so they never read the user's OBS).
pub const FOLDER_ENV: &str = "ISOCAST_OBS_FOLDER";

/// Recordings folder for an import whose OBS profile names none. On Windows
/// `~/Videos` resolves to the Videos known folder, even when redirected.
const FALLBACK_FOLDER: &str = crate::platform_text::select("~/Movies", "~/Videos");

/// Where OBS keeps its settings on this computer: `$ISOCAST_OBS_FOLDER` if
/// set.
pub fn obs_folder(home: &Path) -> PathBuf {
    match std::env::var_os(FOLDER_ENV) {
        Some(folder) if !folder.is_empty() => PathBuf::from(folder),
        // %APPDATA%, else the RoamingAppData known folder (Windows itself
        // may not set the variable for an app launched without a shell).
        _ if cfg!(windows) => {
            crate::api::windows_folder("APPDATA", "AppData/Roaming").join("obs-studio")
        }
        // $XDG_CONFIG_HOME when absolute (the XDG spec ignores relative and
        // empty values), else ~/.config.
        _ if cfg!(target_os = "linux") => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .unwrap_or_else(|| home.join(".config"))
            .join("obs-studio"),
        _ => home.join("Library/Application Support/obs-studio"),
    }
}

/// What [`discover`] found in OBS's settings folder.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Discovery {
    /// The folder looked in.
    pub folder: String,
    /// Whether the folder is there (OBS has run on this Mac).
    pub found: bool,
    /// The scene collections, OBS's current one first, then by name.
    pub collections: Vec<FoundCollection>,
    /// The profiles, OBS's current one first, then by name.
    pub profiles: Vec<FoundProfile>,
    /// Files that couldn't be read, each with why.
    pub errors: Vec<FoundError>,
}

/// An OBS scene collection, for offering it before importing it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FoundCollection {
    /// OBS's name for it, like "Recording".
    pub name: String,
    /// Its .json file (give it, or the name, to `import_obs` or
    /// `library_import_obs`).
    pub file: String,
    pub scenes: usize,
    /// Sources other than scenes and groups.
    pub sources: usize,
    /// OBS's scene collection in use.
    pub current: bool,
    /// The profile an import takes its settings from (a [`FoundProfile`]'s
    /// `path`): named like the collection, else OBS's current one, else one
    /// with the collection's canvas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// An OBS profile's video, recording and stream settings (never its stream
/// key).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FoundProfile {
    pub name: String,
    /// Its basic.ini.
    pub path: String,
    /// OBS's profile in use.
    pub current: bool,
    /// The base (canvas) resolution, width and height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas: Option<[u32; 2]>,
    /// The size recordings are written at, if not the canvas's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<[u32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<f64>,
    /// OBS's recording encoder id, like `apple_h264` or `obs_x264`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoder: Option<String>,
    /// The recording format, like `mkv` or `fragmented_mp4`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_folder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<FoundStream>,
    /// The settings in a line, like "1920×1080 · 60 fps · H.264 · MKV ·
    /// Twitch 6000 kb/s".
    pub summary: String,
}

/// Where an OBS profile streams to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct FoundStream {
    /// The Isocast service it becomes; absent if Isocast has none for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<crate::stream::StreamService>,
    /// OBS's name for the service, like "Twitch" (or its kind, like
    /// `rtmp_custom`).
    pub obs_service: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_kbps: Option<u32>,
    /// OBS keeps a stream key in plain text in the profile's service.json:
    /// `library_import_obs` with `stream_key` moves it to the Keychain.
    pub key_in_file: bool,
}

/// A file in OBS's folder that couldn't be read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FoundError {
    pub file: String,
    pub message: String,
}

/// Look through OBS's settings folder (`obs`, from [`obs_folder`]) for
/// scene collections and profiles, without importing anything.
pub fn discover(obs: &Path) -> Discovery {
    let mut found = Discovery {
        folder: obs.display().to_string(),
        found: obs.is_dir(),
        ..Discovery::default()
    };
    if !found.found {
        return found;
    }
    let global = ["user.ini", "global.ini"]
        .iter()
        .filter_map(|file| std::fs::read_to_string(obs.join(file)).ok())
        .collect::<Vec<_>>();
    let setting = |key: &str| {
        global.iter().find_map(|text| {
            text.lines()
                .find_map(|line| line.trim().strip_prefix(key))
                .map(|value| value.trim().to_string())
        })
    };
    let current_collection = setting("SceneCollectionFile=");
    let current_profile = setting("ProfileDir=");

    let mut entries: Vec<PathBuf> = std::fs::read_dir(obs.join("basic/profiles"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    entries.sort();
    for folder in entries {
        let path = folder.join("basic.ini");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                found.errors.push(FoundError {
                    file: path.display().to_string(),
                    message: format!("couldn't read the profile: {err}"),
                });
                continue;
            }
        };
        let mut profile = Profile::parse(&text);
        profile.read_stream(&folder);
        let dir = dir_name(&path).to_string();
        let name = if profile.name.is_empty() {
            dir.clone()
        } else {
            profile.name.clone()
        };
        let stream = profile.stream.as_ref().map(|s| FoundStream {
            service: stream_settings(&profile, &mut Findings::default()).map(|s| s.service),
            obs_service: s.service.clone().unwrap_or_else(|| s.kind.clone()),
            server: s.server.clone(),
            video_kbps: s.video_kbps,
            key_in_file: plain_stream_key(&path).is_some(),
        });
        found.profiles.push(FoundProfile {
            summary: profile_summary(&profile, stream.as_ref()),
            name,
            path: path.display().to_string(),
            current: current_profile.as_deref() == Some(dir.as_str()),
            canvas: profile.canvas.map(|(w, h)| [w, h]),
            output: profile
                .output
                .filter(|o| Some(*o) != profile.canvas)
                .map(|(w, h)| [w, h]),
            fps: profile.fps,
            encoder: profile.encoder.clone(),
            container: profile.container.clone(),
            recording_folder: profile.folder.clone(),
            stream,
        });
    }
    found
        .profiles
        .sort_by(|a, b| b.current.cmp(&a.current).then(a.name.cmp(&b.name)));

    let mut files: Vec<PathBuf> = std::fs::read_dir(obs.join("basic/scenes"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    files.sort();
    for file in files {
        let read = std::fs::read_to_string(&file)
            .map_err(|err| format!("couldn't read the scene collection: {err}"))
            .and_then(|text| {
                serde_json::from_str::<Value>(&text)
                    .map_err(|err| format!("not a scene collection (not JSON: {err})"))
            })
            .and_then(|json| match json.get("sources").and_then(Value::as_array) {
                Some(_) => Ok(json),
                None => Err("not a scene collection (no `sources` list)".to_string()),
            });
        let json = match read {
            Ok(json) => json,
            Err(message) => {
                found.errors.push(FoundError {
                    file: file.display().to_string(),
                    message,
                });
                continue;
            }
        };
        let stem = file
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let name = text(&json, "name")
            .filter(|n| !n.is_empty())
            .unwrap_or(&stem)
            .to_string();
        let sources = json["sources"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let kind = |s: &Value| text(s, "id").unwrap_or_default().to_string();
        let scenes = sources.iter().filter(|s| kind(s) == "scene").count();
        let others = sources
            .iter()
            .filter(|s| !matches!(kind(s).as_str(), "scene" | "group"))
            .count();
        let profile =
            find_profile(obs, &name, resolution(&json)).map(|(path, _)| path.display().to_string());
        found.collections.push(FoundCollection {
            current: current_collection.as_deref() == Some(stem.as_str()),
            name,
            file: file.display().to_string(),
            scenes,
            sources: others,
            profile,
        });
    }
    found
        .collections
        .sort_by(|a, b| b.current.cmp(&a.current).then(a.name.cmp(&b.name)));
    found
}

/// The stream key OBS keeps in plain text in a profile's service.json
/// (`profile`: its basic.ini), if it does. Never log or show it.
pub fn plain_stream_key(profile: &Path) -> Option<String> {
    let folder = profile.parent()?;
    let file = std::fs::read_to_string(folder.join("service.json")).ok()?;
    let json: Value = serde_json::from_str(&file).ok()?;
    let key = text(json.get("settings")?, "key")?.trim();
    (!key.is_empty()).then(|| key.to_string())
}

/// An OBS profile's settings in a line.
fn profile_summary(profile: &Profile, stream: Option<&FoundStream>) -> String {
    let mut parts = Vec::new();
    match (profile.canvas, profile.output) {
        (Some((w, h)), Some((ow, oh))) if (ow, oh) != (w, h) => {
            parts.push(format!("{w}×{h} → {ow}×{oh}"))
        }
        (Some((w, h)), _) => parts.push(format!("{w}×{h}")),
        _ => {}
    }
    if let Some(fps) = profile.fps {
        if (fps - fps.round()).abs() < 0.001 {
            parts.push(format!("{} fps", fps.round()));
        } else {
            parts.push(format!("{fps:.2} fps"));
        }
    }
    if let Some(encoder) = &profile.encoder {
        parts.push(encoder_label(encoder));
    }
    if let Some(container) = &profile.container {
        parts.push(match container.as_str() {
            "fragmented_mp4" => "MP4 (fragmented)".to_string(),
            "fragmented_mov" => "MOV (fragmented)".to_string(),
            "hybrid_mp4" => "MP4 (hybrid)".to_string(),
            other => other.to_uppercase(),
        });
    }
    if let Some(stream) = stream {
        let service = stream.obs_service.as_str();
        let service = match service {
            "rtmp_custom" => "Custom server",
            other => other,
        };
        match stream.video_kbps {
            Some(kbps) => parts.push(format!("{service} {kbps} kb/s")),
            None => parts.push(service.to_string()),
        }
    }
    parts.join(" · ")
}

/// OBS's encoder ids in words.
fn encoder_label(id: &str) -> String {
    let lower = id.to_lowercase();
    if lower.contains("prores") {
        "ProRes".into()
    } else if lower.contains("hevc") || lower.contains("265") {
        "HEVC".into()
    } else if lower.contains("av1") {
        "AV1".into()
    } else if lower.contains("x264") {
        "x264".into()
    } else if lower.contains("264") || lower.contains("avc") {
        "H.264".into()
    } else {
        id.to_string()
    }
}

/// Read a scene collection file, or one named in OBS's scenes folder, and
/// the profile with its video settings (`profile`: a name or a `basic.ini`
/// path; else the one named like the collection, or OBS's current one, or
/// one whose canvas matches), then [`import`] it.
pub fn import_file(
    collection: &str,
    profile: Option<&str>,
    options: Options<'_>,
) -> Result<Imported, String> {
    let (mut imported, findings) = import_collection_file(collection, profile, options)?;
    imported.unsupported.extend(findings.unsupported);
    imported.notes.extend(findings.notes);
    Ok(imported)
}

/// [`import_file`], keeping apart what is found about the profile's
/// settings (recording, stream, frame rate), for importing the collection
/// on its own.
pub fn import_collection_file(
    collection: &str,
    profile: Option<&str>,
    options: Options<'_>,
) -> Result<(Imported, Findings), String> {
    let home = options.home.map(PathBuf::from);
    let obs = home.as_deref().map(obs_folder);
    let path = collection_path(collection, home.as_deref(), obs.as_deref())?;
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
    let json: Value = serde_json::from_str(&text)
        .map_err(|err| format!("{} is not JSON: {err}", path.display()))?;
    let name = json
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let resolution = resolution(&json);
    let found = match profile {
        Some(profile) => Some(read_profile(profile, home.as_deref(), obs.as_deref())?),
        None => obs
            .as_deref()
            .and_then(|obs| find_profile(obs, &name, resolution)),
    };
    let (mut imported, findings) = import_parts(
        &json,
        &Options {
            profile: found.as_ref().map(|(_, p)| p),
            ..options
        },
    )?;
    imported.collection = Some(path.display().to_string());
    imported.profile = found.map(|(path, _)| path.display().to_string());
    Ok((imported, findings))
}

fn collection_path(
    collection: &str,
    home: Option<&Path>,
    obs: Option<&Path>,
) -> Result<PathBuf, String> {
    let path = expand(collection, home);
    // A path counts only when absolute (or `~/...`): a bare name is OBS's,
    // never a file in isocastd's working folder.
    if path.is_absolute() && path.is_file() {
        return Ok(path);
    }
    let named = obs.map(|obs| obs.join("basic/scenes").join(format!("{collection}.json")));
    match named {
        Some(named) if !collection.contains('/') && named.is_file() => Ok(named),
        _ => {
            let available = obs
                .map(|obs| names_in(&obs.join("basic/scenes"), "json"))
                .unwrap_or_default();
            let problem = if path.is_dir() {
                format!(
                    "{collection:?} is a folder; give a scene collection's .json file or a \
                     collection's name"
                )
            } else if path.exists() {
                format!("{collection:?} is not a scene collection file")
            } else if path.is_relative() && collection.contains('/') {
                format!(
                    "there is no file at {collection:?} in isocastd's folder; give the \
                     collection file's absolute path"
                )
            } else if collection.contains('/') {
                format!("there is no file at {collection:?}")
            } else {
                format!("{collection:?} is neither a file nor the name of an OBS scene collection")
            };
            Err(format!(
                "{problem}{}",
                if available.is_empty() {
                    String::new()
                } else {
                    format!("; OBS has: {}", available.join(", "))
                }
            ))
        }
    }
}

/// The stems of the files with `extension` in `folder`, sorted.
fn names_in(folder: &Path, extension: &str) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some(extension))
        .filter_map(|path| Some(path.file_stem()?.to_str()?.to_string()))
        .collect();
    names.sort();
    names
}

fn expand(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix("~/"), home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

fn read_profile(
    profile: &str,
    home: Option<&Path>,
    obs: Option<&Path>,
) -> Result<(PathBuf, Profile), String> {
    let path = expand(profile, home);
    // As for collections: only an absolute path is a file.
    let path = if path.is_absolute() && path.is_file() {
        path
    } else if path.is_absolute() && path.join("basic.ini").is_file() {
        path.join("basic.ini")
    } else {
        let all = obs.map(profiles).unwrap_or_default();
        return all
            .into_iter()
            .find(|(path, p)| p.name == profile || dir_name(path) == profile)
            .ok_or_else(|| {
                let names: Vec<String> = obs
                    .map(profiles)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(_, p)| p.name)
                    .collect();
                format!(
                    "no OBS profile {profile:?}; give a basic.ini path or one of: {}",
                    names.join(", ")
                )
            });
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("could not read {}: {err}", path.display()))?;
    let mut profile = Profile::parse(&text);
    if let Some(folder) = path.parent() {
        profile.read_stream(folder);
    }
    Ok((path, profile))
}

/// The name of the folder holding a profile's `basic.ini`.
fn dir_name(path: &Path) -> &str {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .unwrap_or_default()
}

/// Every profile in OBS's folder, with its `basic.ini` path.
fn profiles(obs: &Path) -> Vec<(PathBuf, Profile)> {
    let mut found: Vec<(PathBuf, Profile)> = std::fs::read_dir(obs.join("basic/profiles"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("basic.ini"))
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            let mut profile = Profile::parse(&text);
            profile.read_stream(path.parent()?);
            Some((path, profile))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// The profile named like the collection, else OBS's current one, else
/// one whose canvas is the collection's.
fn find_profile(
    obs: &Path,
    collection: &str,
    resolution: Option<(u32, u32)>,
) -> Option<(PathBuf, Profile)> {
    let all = profiles(obs);
    let current = ["user.ini", "global.ini"].iter().find_map(|file| {
        let text = std::fs::read_to_string(obs.join(file)).ok()?;
        text.lines()
            .find_map(|line| line.trim().strip_prefix("ProfileDir="))
            .map(str::to_string)
    });
    let fits = |p: &Profile| resolution.is_none() || p.canvas == resolution;
    let pick = |test: &dyn Fn(&Path, &Profile) -> bool| {
        all.iter()
            .find(|(path, p)| test(path, p) && fits(p))
            .cloned()
    };
    // A profile named like the collection is meant for it, whatever its
    // canvas (Rebase.json is 720×1280, its profile's canvas 1768×3840).
    let named = all
        .iter()
        .find(|(path, p)| p.name == collection || dir_name(path) == collection)
        .cloned();
    named
        .or_else(|| pick(&|path, _| Some(dir_name(path)) == current.as_deref()))
        .or_else(|| pick(&|_, _| true))
}

/// The canvas size the collection was saved with (OBS 31 and later write
/// it).
fn resolution(collection: &Value) -> Option<(u32, u32)> {
    let r = collection.get("resolution")?;
    let x = r.get("x")?.as_f64()?;
    let y = r.get("y")?.as_f64()?;
    (x >= 1.0 && y >= 1.0).then_some((x as u32, y as u32))
}

/// Turn a scene collection into a setup, listing what it couldn't bring
/// over.
pub fn import(collection: &Value, options: &Options<'_>) -> Result<Imported, String> {
    let (mut imported, findings) = import_parts(collection, options)?;
    imported.unsupported.extend(findings.unsupported);
    imported.notes.extend(findings.notes);
    Ok(imported)
}

/// [`import`], keeping apart what is found about the profile's settings.
fn import_parts(collection: &Value, options: &Options<'_>) -> Result<(Imported, Findings), String> {
    let Some(sources) = collection.get("sources").and_then(Value::as_array) else {
        return Err(
            "not an OBS scene collection: expected a JSON object with a \"sources\" list".into(),
        );
    };
    let mut importer = Importer::new(collection, sources, options);
    importer.run();
    Ok(importer.finish())
}

/// The top-level keys of OBS's global audio devices (Settings → Audio).
const GLOBAL_AUDIO: &[&str] = &[
    "DesktopAudioDevice1",
    "DesktopAudioDevice2",
    "AuxAudioDevice1",
    "AuxAudioDevice2",
    "AuxAudioDevice3",
    "AuxAudioDevice4",
];

/// One OBS source, scene or group, as saved.
#[derive(Clone, Copy)]
struct Obs<'v> {
    json: &'v Value,
    name: &'v str,
    /// The kind with its version, like `color_source_v3`.
    kind: &'v str,
    settings: &'v Value,
}

impl<'v> Obs<'v> {
    fn new(json: &'v Value) -> Self {
        let kind = text(json, "versioned_id")
            .filter(|k| !k.is_empty())
            .or_else(|| text(json, "id"))
            .unwrap_or_default();
        Self {
            json,
            name: text(json, "name").unwrap_or_default(),
            kind,
            settings: json.get("settings").unwrap_or(&Value::Null),
        }
    }

    fn uuid(&self) -> Option<&'v str> {
        text(self.json, "uuid").filter(|u| !u.is_empty())
    }

    /// Its kind without the version, like `color_source`.
    fn base_kind(&self) -> &'v str {
        unversioned(self.kind)
    }

    fn label(&self) -> String {
        format!("source {:?}", self.name)
    }
}

fn unversioned(kind: &str) -> &str {
    match kind.rsplit_once("_v") {
        Some((base, version)) if version.chars().all(|c| c.is_ascii_digit()) => base,
        _ => kind,
    }
}

/// What an OBS source became.
#[derive(Clone)]
enum Target {
    Source {
        id: String,
        video: bool,
        audio: bool,
    },
    Scene(String),
    Group,
    Skipped,
}

/// Maps OBS coordinates in a parent (the canvas, or a group) to Isocast
/// coordinates in the same parent.
#[derive(Clone, Copy)]
struct Space {
    kx: f64,
    ky: f64,
    ox: f64,
    oy: f64,
}

/// Where an item lands.
struct Placed {
    rect: Rect,
    crop: Crop,
    rotate: f32,
    /// Mirrored on one axis, which Isocast can't show.
    mirrored: bool,
    /// The content's size in Isocast pixels before rounding.
    size: (f64, f64),
}

struct Importer<'a, 'v> {
    options: &'a Options<'a>,
    collection: &'v Value,
    /// The canvas the collection's coordinates are in, and the one imported
    /// onto.
    from: (u32, u32),
    to: (u32, u32),
    fps: u32,
    objects: Vec<Obs<'v>>,
    by_uuid: HashMap<&'v str, usize>,
    by_name: HashMap<&'v str, usize>,
    targets: Vec<Target>,
    sizes: HashMap<usize, Option<(f64, f64)>>,
    sources: Vec<Source>,
    scenes: Vec<Scene>,
    iso: Vec<String>,
    /// Source Record's "different audio" source, by recorded source.
    iso_audio: BTreeMap<String, Vec<String>>,
    iso_encoder: Option<IsoEncoder>,
    unsupported: Vec<Unsupported>,
    notes: Vec<String>,
    /// What is found about the profile's settings, kept apart.
    profile: Findings,
    counts: Counts,
    audio_items: usize,
}

impl<'a, 'v> Importer<'a, 'v> {
    fn new(collection: &'v Value, sources: &'v [Value], options: &'a Options<'a>) -> Self {
        let mut objects: Vec<Obs<'v>> = sources.iter().map(Obs::new).collect();
        if let Some(groups) = collection.get("groups").and_then(Value::as_array) {
            objects.extend(groups.iter().map(Obs::new));
        }
        for key in GLOBAL_AUDIO {
            if let Some(device) = collection.get(*key).filter(|d| d.is_object()) {
                let device = Obs::new(device);
                let listed = |uuid| objects.iter().any(|o| o.uuid() == Some(uuid));
                if !device.uuid().is_some_and(listed) {
                    objects.push(device);
                }
            }
        }
        let mut by_uuid = HashMap::new();
        let mut by_name = HashMap::new();
        for (i, obs) in objects.iter().enumerate() {
            if let Some(uuid) = obs.uuid() {
                by_uuid.entry(uuid).or_insert(i);
            }
            by_name.entry(obs.name).or_insert(i);
        }
        let mut notes = Vec::new();
        let saved = resolution(collection);
        let profile = options.profile;
        let from = saved
            .or(profile.and_then(|p| p.canvas))
            .or(options.canvas)
            .unwrap_or_else(|| {
                notes.push(
                    "the collection and profile don't say the canvas size; assumed 1920×1080"
                        .into(),
                );
                (1920, 1080)
            });
        if let (Some(saved), Some(canvas), None) =
            (saved, profile.and_then(|p| p.canvas), options.canvas)
            && saved != canvas
        {
            notes.push(format!(
                "the collection was last saved on a {}×{} canvas and the profile's is {}×{}; \
                 imported at {}×{} (give a canvas size to scale it)",
                saved.0, saved.1, canvas.0, canvas.1, saved.0, saved.1
            ));
        }
        let to = options.canvas.unwrap_or(from);
        if to != from {
            notes.push(format!(
                "scaled from the collection's {}×{} canvas to {}×{}, about the centre by the \
                 change in height, as OBS does",
                from.0, from.1, to.0, to.1
            ));
            if from.0 * to.1 != to.0 * from.1 {
                notes.push(
                    "the new canvas has another shape: items keep their proportions, so check \
                     the edges, and nested scenes stretch to their items"
                        .into(),
                );
            }
        }
        let mut profile_findings = Findings::default();
        let fps = frame_rate(options.fps, profile, &mut profile_findings.notes);
        Self {
            options,
            collection,
            from,
            to,
            fps,
            targets: vec![Target::Skipped; objects.len()],
            objects,
            by_uuid,
            by_name,
            sizes: HashMap::new(),
            sources: Vec::new(),
            scenes: Vec::new(),
            iso: Vec::new(),
            iso_audio: BTreeMap::new(),
            iso_encoder: None,
            unsupported: Vec::new(),
            notes,
            profile: profile_findings,
            counts: Counts::default(),
            audio_items: 0,
        }
    }

    fn unsupported(&mut self, obs: impl Into<String>, kind: &str, reason: impl Into<String>) {
        self.unsupported.push(Unsupported {
            obs: obs.into(),
            kind: kind.into(),
            reason: reason.into(),
        });
    }

    fn run(&mut self) {
        self.name_scenes();
        for i in 0..self.objects.len() {
            let obs = self.objects[i];
            match obs.base_kind() {
                "scene" => {}
                "group" => self.targets[i] = Target::Group,
                _ => self.add_source(i),
            }
        }
        // Source Record filters name their audio sources by OBS name, so
        // they are read once every source has its id.
        for i in 0..self.objects.len() {
            if matches!(self.targets[i], Target::Source { .. }) {
                self.add_filters(i);
            }
        }
        self.add_scenes();
    }

    /// Give every scene an id, in OBS's scene order.
    fn name_scenes(&mut self) {
        let order: Vec<&str> = self
            .collection
            .get("scene_order")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|s| text(s, "name"))
            .collect();
        let mut scenes: Vec<usize> = (0..self.objects.len())
            .filter(|&i| self.objects[i].base_kind() == "scene")
            .collect();
        scenes.sort_by_key(|&i| {
            order
                .iter()
                .position(|name| *name == self.objects[i].name)
                .unwrap_or(usize::MAX)
        });
        let mut ids = HashSet::new();
        for i in scenes {
            let id = unique_id(self.objects[i].name, "scene", &mut ids);
            self.targets[i] = Target::Scene(id.clone());
            self.scenes.push(Scene {
                id,
                name: Some(self.objects[i].name.to_string()),
                layout: None,
                items: Vec::new(),
                transition: None,
                vertical: None,
                canvases: Default::default(),
            });
        }
    }

    fn add_source(&mut self, i: usize) {
        let obs = self.objects[i];
        self.counts.obs_sources += 1;
        let kind = match self.source_kind(obs) {
            Ok(kind) => kind,
            Err(reason) => {
                self.unsupported(obs.label(), obs.kind, reason);
                return;
            }
        };
        let mut taken: HashSet<String> = self.sources.iter().map(|s| s.id.clone()).collect();
        taken.extend(crate::RESERVED_SOURCE_IDS.iter().map(|id| id.to_string()));
        let id = unique_id(obs.name, "source", &mut taken);
        let audio = if kind.has_audio() {
            self.audio_settings(obs)
        } else {
            AudioSettings::default()
        };
        if obs.json.get("enabled") == Some(&Value::Bool(false)) {
            self.unsupported(
                obs.label(),
                obs.kind,
                "it is disabled in OBS; imported as an ordinary source",
            );
        }
        if int(obs.json, "deinterlace_mode").unwrap_or(0) != 0 {
            self.unsupported(
                obs.label(),
                obs.kind,
                "deinterlacing isn't available; the picture comes through as captured",
            );
        }
        self.targets[i] = Target::Source {
            id: id.clone(),
            video: kind.has_video(),
            audio: kind.has_audio(),
        };
        self.sources.push(Source {
            id,
            name: Some(obs.name.to_string()),
            kind,
            audio,
            filters: Vec::new(),
            audio_filters: Vec::new(),
            encoder: SourceEncoder::default(),
            unknown: UnknownFields::new(),
        });
        self.counts.sources += 1;
    }

    /// Volume, mute and sync offset, and what can't come over yet.
    fn audio_settings(&mut self, obs: Obs<'v>) -> AudioSettings {
        let volume = number(obs.json, "volume").unwrap_or(1.0);
        let mut gain_db = if volume > 0.0 {
            20.0 * volume.log10()
        } else {
            -60.0
        };
        if !(-60.0..=24.0).contains(&gain_db) {
            self.notes.push(format!(
                "{}: volume {:.1} dB is outside −60 to 24 dB; clamped",
                obs.label(),
                gain_db
            ));
            gain_db = gain_db.clamp(-60.0, 24.0);
        }
        let sync_ms = number(obs.json, "sync").unwrap_or(0.0) / 1_000_000.0;
        let offset_ms = sync_ms.round().clamp(-2000.0, 2000.0) as i32;
        if (sync_ms - f64::from(offset_ms)).abs() >= 1.0 {
            self.notes.push(format!(
                "{}: sync offset {sync_ms:.0} ms is outside ±2000 ms; clamped",
                obs.label()
            ));
        }
        // OBS's balance runs 0 (left) to 1 (right) with the centre at 0.5.
        let balance = number(obs.json, "balance").unwrap_or(0.5).clamp(0.0, 1.0);
        let balance = ((2.0 * balance - 1.0) * 1000.0).round() / 1000.0;
        let monitor = match int(obs.json, "monitoring_type").unwrap_or(0) {
            1 => Monitoring::MonitorOnly,
            2 => Monitoring::MonitorAndOutput,
            _ => Monitoring::Off,
        };
        // Hearing system audio through the speakers would feed back.
        let monitor = match (monitor, obs.base_kind()) {
            (Monitoring::Off, _) => Monitoring::Off,
            (_, "coreaudio_output_capture" | "sck_audio_capture") => {
                self.notes.push(format!(
                    "{}: monitoring desktop audio would feed back; left off",
                    obs.label()
                ));
                Monitoring::Off
            }
            (monitor, _) => monitor,
        };
        AudioSettings {
            // OBS_SOURCE_FLAG_FORCE_MONO (Advanced Audio Properties' Mono).
            mono: int(obs.json, "flags").unwrap_or(0) & 2 != 0,
            iso: None,
            gain_db: (gain_db * 10.0).round() / 10.0,
            muted: obs.json.get("muted") == Some(&Value::Bool(true)),
            offset_ms,
            balance,
            monitor,
        }
    }

    /// The Isocast kind of an OBS source, or why there is none.
    fn source_kind(&mut self, obs: Obs<'v>) -> Result<SourceKind, String> {
        let s = obs.settings;
        let label = obs.label();
        Ok(match obs.base_kind() {
            "screen_capture" => {
                let cursor = flag(s, "show_cursor").unwrap_or(true);
                match int(s, "type").unwrap_or(0) {
                    1 => self.window_by_id(s, cursor)?,
                    2 => match text(s, "application").filter(|a| !a.is_empty()) {
                        Some(app) => SourceKind::Application(ApplicationSource {
                            app: app.into(),
                            display: self.display(&label, s),
                            cursor,
                        }),
                        None => return Err("application capture with no application chosen".into()),
                    },
                    _ => SourceKind::Display(DisplaySource {
                        display: self.display(&label, s),
                        cursor,
                    }),
                }
            }
            "display_capture" => {
                let index = int(s, "display").unwrap_or(0);
                if index != 0 {
                    self.notes.push(format!(
                        "{label}: OBS chose display number {index}; imported as the main display"
                    ));
                }
                SourceKind::Display(DisplaySource {
                    display: "main".into(),
                    cursor: flag(s, "show_cursor").unwrap_or(true),
                })
            }
            "window_capture" => self.window_capture(&label, s)?,
            "monitor_capture"
            | "xshm_input"
            | "pipewire-screen-capture-source"
            | "pipewire-desktop-capture-source" => {
                self.notes.push(format!(
                    "{label}: a screen capture from another system; imported as the main display"
                ));
                SourceKind::Display(DisplaySource {
                    display: "main".into(),
                    cursor: flag(s, "capture_cursor")
                        .or(flag(s, "show_cursor"))
                        .unwrap_or(true),
                })
            }
            "macos-avcapture" | "macos-avcapture-fast" | "av_capture_input" => {
                if flag(s, "enable_audio") == Some(true) {
                    self.unsupported(
                        label.clone(),
                        obs.kind,
                        "a camera's own audio isn't captured; add its microphone as a source",
                    );
                }
                let (width, height, fps) = match camera_format(s) {
                    Some((w, h)) => (Some(w), Some(h), camera_fps(s)),
                    None => (None, None, None),
                };
                SourceKind::Camera(CameraSource {
                    device: self.camera(&label, text(s, "device"), text(s, "device_name"))?,
                    width,
                    height,
                    fps,
                })
            }
            "dshow_input" | "v4l2_input" => {
                let id = text(s, "video_device_id")
                    .or(text(s, "device_id"))
                    .unwrap_or_default();
                // Windows writes "Name:\\?\usb#…".
                let name = id.split(":\\").next().unwrap_or(id);
                self.notes.push(format!(
                    "{label}: a camera on another system; matched by its name {name:?}"
                ));
                SourceKind::Camera(CameraSource {
                    device: self.camera(&label, None, Some(name))?,
                    width: None,
                    height: None,
                    fps: None,
                })
            }
            "coreaudio_input_capture" => SourceKind::Microphone(MicrophoneSource::new(
                self.microphone(&label, text(s, "device_id")),
            )),
            "wasapi_input_capture" | "pulse_input_capture" | "alsa_input_capture" => {
                self.notes.push(format!(
                    "{label}: a microphone on another system; imported as the default input"
                ));
                SourceKind::Microphone(MicrophoneSource::new("default"))
            }
            "coreaudio_output_capture" | "wasapi_output_capture" | "pulse_output_capture" => {
                self.notes.push(format!(
                    "{label}: OBS captured an output device; Isocast records {}",
                    crate::platform_text::select_linux(
                        "what the Mac plays",
                        "what this PC plays",
                        "what this computer plays",
                    )
                ));
                SourceKind::SystemAudio(SystemAudioSource::default())
            }
            "sck_audio_capture" => match int(s, "type").unwrap_or(0) {
                1 => match text(s, "application").filter(|a| !a.is_empty()) {
                    Some(app) => {
                        SourceKind::ApplicationAudio(ApplicationAudioSource { app: app.into() })
                    }
                    None => return Err("application audio with no application chosen".into()),
                },
                _ => SourceKind::SystemAudio(SystemAudioSource::default()),
            },
            "color_source" => SourceKind::Color(ColorSource {
                color: obs_color(int(s, "color").unwrap_or(0xFFFF_FFFF), true),
                width: int(s, "width").filter(|w| *w > 0).map(|w| w as u32),
                height: int(s, "height").filter(|h| *h > 0).map(|h| h as u32),
            }),
            "image_source" => match text(s, "file").filter(|f| !f.is_empty()) {
                Some(file) => SourceKind::Image(ImageSource {
                    path: self.path(file),
                    width: None,
                    height: None,
                }),
                None => return Err("no image file chosen".into()),
            },
            "slideshow" => self.slideshow(&label, s)?,
            "text_ft2_source" => self.text_ft2(&label, s),
            "text_gdiplus" => self.text_gdiplus(&label, s),
            "ffmpeg_source" => {
                if flag(s, "is_local_file") == Some(false) {
                    return Err(format!(
                        "it plays a network stream ({}), which Isocast can't open yet",
                        text(s, "input").unwrap_or_default()
                    ));
                }
                let Some(file) = text(s, "local_file").filter(|f| !f.is_empty()) else {
                    return Err("no media file chosen".into());
                };
                let percent = number(s, "speed_percent").unwrap_or(100.0);
                SourceKind::Media(MediaSource {
                    files: vec![self.path(file)],
                    repeat: flag(s, "looping").unwrap_or(false),
                    speed: self.speed(&label, percent),
                    restart_on_activate: flag(s, "restart_on_activate").unwrap_or(true),
                })
            }
            "vlc_source" => {
                let files: Vec<String> = s
                    .get("playlist")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|f| f.get("hidden") != Some(&Value::Bool(true)))
                    .filter_map(|f| text(f, "value"))
                    .map(|f| self.path(f))
                    .collect();
                if files.is_empty() {
                    return Err("its playlist is empty".into());
                }
                if flag(s, "shuffle") == Some(true) {
                    self.unsupported(
                        label.clone(),
                        obs.kind,
                        "shuffled playlists aren't available; plays in order",
                    );
                }
                SourceKind::Media(MediaSource {
                    files,
                    repeat: flag(s, "loop").unwrap_or(true),
                    speed: 1.0,
                    restart_on_activate: text(s, "playback_behavior")
                        .is_none_or(|b| b == "stop_restart"),
                })
            }
            "browser_source" => self.browser(&label, s)?,
            "syphon-input" => return Err("Syphon sources aren't available yet".into()),
            "ndi_source" => return Err("NDI sources aren't available yet".into()),
            "game_capture" => {
                return Err(crate::platform_text::select_linux(
                    "Game Capture is Windows-only",
                    "Game Capture isn't available yet; add a display or window capture instead",
                    "Game Capture is Windows-only",
                )
                .into());
            }
            _ => {
                return Err(format!(
                    "Isocast has no {:?} source (a plugin or another system's kind)",
                    obs.kind
                ));
            }
        })
    }

    /// The display a screen capture names: its id if this Mac has it (or
    /// it isn't known), "main" if none is chosen.
    fn display(&mut self, label: &str, s: &Value) -> String {
        let Some(uuid) = text(s, "display_uuid").filter(|u| !u.is_empty()) else {
            return "main".into();
        };
        if let Some(devices) = self.options.devices {
            match devices
                .displays
                .iter()
                .find(|d| d.id.eq_ignore_ascii_case(uuid))
            {
                Some(display) => return display.id.clone(),
                None => self.notes.push(format!(
                    "{label}: display {uuid} isn't connected; kept its id (use \"main\" or a \
                     name from `isocast devices`)"
                )),
            }
        }
        uuid.into()
    }

    /// A window capture by window number, found among this Mac's windows.
    fn window_by_id(&mut self, s: &Value, cursor: bool) -> Result<SourceKind, String> {
        let id = int(s, "window").unwrap_or(0);
        let window = self
            .options
            .devices
            .and_then(|d| d.windows.iter().find(|w| i64::from(w.id) == id));
        match window {
            Some(window) => Ok(SourceKind::Window(WindowSource {
                app: window.app.clone(),
                title: window.title.clone().filter(|t| !t.is_empty()),
                cursor,
            })),
            None => Err(format!(
                "a window capture saves only the window's number ({id}), which changes when \
                 its app restarts; add a window source naming the app"
            )),
        }
    }

    /// The older window capture, which saves the app and window names
    /// (macOS), or "title:class:app.exe" (Windows).
    fn window_capture(&mut self, label: &str, s: &Value) -> Result<SourceKind, String> {
        let (app, title) = match text(s, "owner_name").filter(|o| !o.is_empty()) {
            Some(owner) => (
                owner.to_string(),
                text(s, "window_name").map(str::to_string),
            ),
            None => {
                let window = text(s, "window").unwrap_or_default();
                let mut parts = window.split(':');
                let title = parts.next().unwrap_or_default().to_string();
                let exe = parts.next_back().unwrap_or_default();
                let app = exe.strip_suffix(".exe").unwrap_or(exe).to_string();
                self.notes.push(format!(
                    "{label}: a window capture from another system; matched by the app {app:?}"
                ));
                (app, Some(title))
            }
        };
        if app.is_empty() {
            return Err("no window chosen".into());
        }
        Ok(SourceKind::Window(WindowSource {
            app,
            title: title.filter(|t| !t.is_empty()),
            cursor: flag(s, "show_cursor").unwrap_or(true),
        }))
    }

    /// A camera's name as Isocast matches it: this Mac's name for OBS's
    /// device id, else OBS's name; OBS's id for a camera that isn't
    /// connected, since its name could match part of another camera's
    /// ("Ryan iPhone", an iPhone's screen, is in "Ryan iPhone Camera").
    fn camera(
        &mut self,
        label: &str,
        id: Option<&str>,
        name: Option<&str>,
    ) -> Result<String, String> {
        let (id, name) = (id.filter(|i| !i.is_empty()), name.filter(|n| !n.is_empty()));
        if let Some(devices) = self.options.devices {
            if let Some(camera) = devices.cameras.iter().find(|c| Some(c.id.as_str()) == id) {
                return Ok(camera.name.clone());
            }
            if !devices
                .cameras
                .iter()
                .any(|c| Some(c.name.as_str()) == name)
            {
                let missing = name.or(id).unwrap_or_default();
                if let Some(id) = id {
                    self.notes.push(format!(
                        "{label}: camera {missing:?} isn't connected now; kept its id {id}, so \
                         no other camera stands in for it"
                    ));
                    return Ok(id.into());
                }
                self.notes
                    .push(format!("{label}: camera {missing:?} isn't connected now"));
            }
        }
        name.or(id)
            .map(str::to_string)
            .ok_or_else(|| "no device chosen".into())
    }

    /// A microphone as Isocast matches it: its name if this Mac has OBS's
    /// device id, else the id.
    fn microphone(&mut self, label: &str, id: Option<&str>) -> String {
        let Some(id) = id.filter(|i| !i.is_empty() && *i != "default") else {
            return "default".into();
        };
        if let Some(devices) = self.options.devices {
            let mics = &devices.microphones;
            match mics.iter().find(|m| m.id == id) {
                Some(mic) if mics.iter().filter(|m| m.name == mic.name).count() == 1 => {
                    return mic.name.clone();
                }
                Some(_) => {}
                None => self.notes.push(format!(
                    "{label}: microphone {id} isn't connected now; kept its id"
                )),
            }
        }
        id.into()
    }

    fn slideshow(&mut self, label: &str, s: &Value) -> Result<SourceKind, String> {
        let files: Vec<String> = s
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|f| f.get("hidden") != Some(&Value::Bool(true)))
            .filter_map(|f| text(f, "value"))
            .map(|f| self.path(f))
            .collect();
        if files.is_empty() {
            return Err("it has no images".into());
        }
        if let Some(folder) = files.iter().find(|f| !looks_like_image(f)) {
            self.notes.push(format!(
                "{label}: {folder} looks like a folder; Isocast slide shows list image files"
            ));
        }
        let slide_ms = int(s, "slide_time").unwrap_or(8000).clamp(100, 3_600_000) as u32;
        let speed = int(s, "transition_speed").unwrap_or(700).max(0) as u32;
        let speed = speed.min(slide_ms.saturating_sub(1));
        let transition = match text(s, "transition").unwrap_or("fade") {
            "cut" => Transition::default(),
            "fade" => Transition::fade(speed),
            other => {
                self.unsupported(
                    label,
                    "slideshow",
                    format!("the {other:?} slide transition isn't available; fades instead"),
                );
                Transition::fade(speed)
            }
        };
        if text(s, "slide_mode") == Some("mode_manual") {
            self.unsupported(
                label,
                "slideshow",
                "manual slide changes (hotkeys) aren't available; it advances on its own",
            );
        }
        let size = text(s, "custom_size").and_then(parse_size);
        Ok(SourceKind::Slideshow(SlideshowSource {
            files,
            slide_ms,
            transition,
            repeat: flag(s, "loop").unwrap_or(true),
            shuffle: flag(s, "randomize").unwrap_or(false),
            width: size.map(|(w, _)| w),
            height: size.map(|(_, h)| h),
        }))
    }

    /// OBS's FreeType 2 text, the text source on Macs.
    fn text_ft2(&mut self, label: &str, s: &Value) -> SourceKind {
        let font = s.get("font").unwrap_or(&Value::Null);
        let flags = int(font, "flags").unwrap_or(0);
        let top = int(s, "color1").unwrap_or(0xFFFF_FFFF);
        if int(s, "color2").unwrap_or(0xFFFF_FFFF) != top {
            self.unsupported(
                label,
                "text_ft2_source",
                "gradient text isn't available; drawn in its top colour",
            );
        }
        if flag(s, "log_mode") == Some(true) {
            self.unsupported(
                label,
                "text_ft2_source",
                "chat log mode isn't available; shows the whole file",
            );
        }
        let (text, file) = text_or_file(self, s, "from_file", "text_file");
        let wrap = flag(s, "word_wrap") == Some(true);
        SourceKind::Text(TextSource {
            text,
            file,
            data: None,
            font: font_face(font),
            size: number(font, "size").unwrap_or(256.0).clamp(1.0, 1000.0) as f32,
            weight: if flags & 1 != 0 { 700 } else { 400 },
            italic: flags & 2 != 0,
            color: obs_color(top, true),
            outline: (flag(s, "outline") == Some(true)).then(|| TextOutline {
                width: 2.0,
                color: Color("#000000".into()),
            }),
            shadow: (flag(s, "drop_shadow") == Some(true)).then(|| TextShadow {
                x: 4.0,
                y: 4.0,
                color: Color("#000000".into()),
            }),
            background: None,
            align: TextAlign::Left,
            wrap_width: int(s, "custom_width")
                .filter(|w| wrap && *w > 0)
                .map(|w| w as u32),
            padding: 0,
            line_height: 1.2,
            width: None,
            height: None,
        })
    }

    /// OBS's text source on Windows.
    fn text_gdiplus(&mut self, label: &str, s: &Value) -> SourceKind {
        let font = s.get("font").unwrap_or(&Value::Null);
        let flags = int(font, "flags").unwrap_or(0);
        if flag(s, "gradient") == Some(true) {
            self.unsupported(
                label,
                "text_gdiplus",
                "gradient text isn't available; drawn in its main colour",
            );
        }
        if flag(s, "vertical") == Some(true) {
            self.unsupported(label, "text_gdiplus", "vertical text isn't available");
        }
        let with_opacity = |color: i64, opacity: i64| {
            let rgb = color & 0x00FF_FFFF;
            let alpha = (opacity.clamp(0, 100) * 255 + 50) / 100;
            obs_color(rgb | (alpha << 24), true)
        };
        let opacity = int(s, "opacity").unwrap_or(100);
        let (content, file) = text_or_file(self, s, "read_from_file", "file");
        let extents = flag(s, "extents") == Some(true);
        let extent = |key: &str| int(s, key).filter(|v| extents && *v > 0).map(|v| v as u32);
        let (width, height) = (extent("extents_cx"), extent("extents_cy"));
        SourceKind::Text(TextSource {
            text: content,
            file,
            data: None,
            font: font_face(font),
            size: number(font, "size").unwrap_or(256.0).clamp(1.0, 1000.0) as f32,
            weight: if flags & 1 != 0 { 700 } else { 400 },
            italic: flags & 2 != 0,
            color: with_opacity(int(s, "color").unwrap_or(0xFF_FFFF), opacity),
            outline: (flag(s, "outline") == Some(true)).then(|| TextOutline {
                width: int(s, "outline_size").unwrap_or(2).clamp(0, 100) as f32,
                color: with_opacity(
                    int(s, "outline_color").unwrap_or(0),
                    int(s, "outline_opacity").unwrap_or(100),
                ),
            }),
            shadow: None,
            background: int(s, "bk_opacity")
                .filter(|o| *o > 0)
                .map(|o| with_opacity(int(s, "bk_color").unwrap_or(0), o)),
            align: match text(s, "align") {
                Some("center") => TextAlign::Center,
                Some("right") => TextAlign::Right,
                _ => TextAlign::Left,
            },
            wrap_width: width.filter(|_| flag(s, "extents_wrap").unwrap_or(true)),
            padding: 0,
            line_height: 1.2,
            width: width.zip(height).map(|(w, _)| w),
            height: width.zip(height).map(|(_, h)| h),
        })
    }

    fn browser(&mut self, label: &str, s: &Value) -> Result<SourceKind, String> {
        let local = flag(s, "is_local_file") == Some(true);
        let (url, file) = match (local, text(s, "local_file"), text(s, "url")) {
            (true, Some(file), _) if !file.is_empty() => (None, Some(self.path(file))),
            (true, _, _) => return Err("a local file page with no file chosen".into()),
            (false, _, Some(url)) if !url.is_empty() => (Some(url.to_string()), None),
            (false, _, _) => (Some("https://obsproject.com/browser-source".into()), None),
        };
        if let Some(url) = &url
            && !["http://", "https://", "file://"]
                .iter()
                .any(|p| url.starts_with(p))
        {
            return Err(format!(
                "its URL {url:?} isn't http, https or file, which Isocast can load"
            ));
        }
        let fps = if flag(s, "fps_custom") == Some(true) {
            int(s, "fps").unwrap_or(30)
        } else {
            i64::from(self.fps)
        };
        if fps > 60 {
            self.notes.push(format!(
                "{label}: pages render at up to 60 fps in Isocast, not {fps}"
            ));
        }
        let size = |key: &str, default: i64| int(s, key).unwrap_or(default).clamp(1, 8192) as u32;
        Ok(SourceKind::Browser(BrowserSource {
            url,
            file,
            width: size("width", 800),
            height: size("height", 600),
            fps: fps.clamp(1, 60) as u32,
            css: text(s, "css")
                .map(str::to_string)
                .unwrap_or_else(crate::source::browser_css),
            control_audio: flag(s, "reroute_audio").unwrap_or(false),
            shutdown_when_hidden: flag(s, "shutdown").unwrap_or(false),
            refresh_when_shown: flag(s, "restart_when_active").unwrap_or(false),
        }))
    }

    /// A playback speed from OBS's percentage, within Isocast's range.
    fn speed(&mut self, label: &str, percent: f64) -> f32 {
        let speed = (percent / 100.0).clamp(0.25, 4.0);
        if (speed - percent / 100.0).abs() > 0.001 {
            self.notes.push(format!(
                "{label}: speed {percent}% is outside 25–400%; set to {:.0}%",
                speed * 100.0
            ));
        }
        speed as f32
    }

    /// A file path, written `~/...` when it is in the home folder.
    fn path(&self, path: &str) -> String {
        home_relative(path, self.options.home)
    }

    fn add_filters(&mut self, i: usize) {
        let obs = self.objects[i];
        let Target::Source { id, video, .. } = self.targets[i].clone() else {
            return;
        };
        let mut filters = Vec::new();
        let mut audio_filters = Vec::new();
        let mut keys = HashSet::new();
        let mut audio_keys = HashSet::new();
        for json in obs
            .json
            .get("filters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let filter = Obs::new(json);
            self.counts.obs_filters += 1;
            let where_ = format!("{} filter {:?}", obs.label(), filter.name);
            let enabled = json.get("enabled") != Some(&Value::Bool(false));
            if filter.base_kind() == "source_record_filter" {
                self.source_record(&where_, &id, video, enabled, filter);
                continue;
            }
            if audio_filter(filter.base_kind()).is_some() {
                match self.audio_filter_kind(&where_, filter) {
                    Ok(kind) => {
                        let key = unique_id(filter.name, kind.label(), &mut audio_keys);
                        audio_filters.push(AudioFilter {
                            id: (key != kind.label()).then_some(key),
                            enabled,
                            kind,
                            unknown: UnknownFields::new(),
                        });
                        self.counts.filters += 1;
                    }
                    Err(reason) => self.unsupported(where_, filter.kind, reason),
                }
                continue;
            }
            if !video {
                self.unsupported(
                    where_,
                    filter.kind,
                    "Isocast has no such filter for a source without a picture",
                );
                continue;
            }
            match self.filter_kind(&where_, filter) {
                Ok(kind) => {
                    let key = unique_id(filter.name, kind.label(), &mut keys);
                    filters.push(Filter {
                        id: (key != kind.label()).then_some(key),
                        enabled,
                        kind,
                        unknown: UnknownFields::new(),
                    });
                    self.counts.filters += 1;
                }
                Err(reason) => self.unsupported(where_, filter.kind, reason),
            }
        }
        if let Some(source) = self.sources.iter_mut().find(|s| s.id == id) {
            source.filters = filters;
            // Isocast only runs audio filters on sources with sound.
            if source.kind.has_audio() {
                source.audio_filters = audio_filters;
            } else if !audio_filters.is_empty() {
                self.notes.push(format!(
                    "{}: its audio filters were left out, since it has no sound in Isocast",
                    obs.label()
                ));
            }
        }
    }

    /// The Isocast kind of an OBS audio filter, or why there is none.
    fn audio_filter_kind(
        &mut self,
        where_: &str,
        filter: Obs<'v>,
    ) -> Result<AudioFilterKind, String> {
        let s = filter.settings;
        let num = |key: &str| number(s, key);
        let mut spec = serde_json::Map::new();
        let mut set = |name: &str, value: Option<f64>| {
            if let Some(value) = value {
                spec.insert(name.into(), Value::from(value));
            }
        };
        let kind = match filter.base_kind() {
            "gain_filter" => {
                set("gain_db", num("db"));
                "gain"
            }
            "invert_polarity_filter" => "invert_polarity",
            "noise_gate_filter" => {
                set("open_threshold_db", num("open_threshold"));
                set("close_threshold_db", num("close_threshold"));
                set("attack_ms", num("attack_time"));
                set("hold_ms", num("hold_time"));
                set("release_ms", num("release_time"));
                "noise_gate"
            }
            "noise_suppress_filter" => {
                if let Some(method) = text(s, "method").filter(|m| *m != "rnnoise") {
                    self.notes.push(format!(
                        "{where_}: uses RNNoise, Isocast's noise suppression, instead of {method:?}"
                    ));
                }
                "noise_suppression"
            }
            kind @ ("compressor_filter" | "expander_filter" | "upward_compressor_filter") => {
                set("ratio", num("ratio"));
                set("threshold_db", num("threshold"));
                set("attack_ms", num("attack_time"));
                set("release_ms", num("release_time"));
                set("output_gain_db", num("output_gain"));
                if kind != "compressor_filter" {
                    let detection = match text(s, "detector").unwrap_or("RMS") {
                        "peak" | "Peak" => "peak",
                        _ => "rms",
                    };
                    spec.insert("detection".into(), Value::from(detection));
                }
                if kind == "compressor_filter"
                    && let Some(name) =
                        text(s, "sidechain_source").filter(|n| !n.is_empty() && *n != "none")
                {
                    match self.by_name.get(name).map(|&i| self.targets[i].clone()) {
                        Some(Target::Source { id, .. }) => {
                            spec.insert("sidechain_source".into(), Value::from(id));
                        }
                        _ => self.notes.push(format!(
                            "{where_}: its sidechain {name:?} isn't an imported source; left out"
                        )),
                    }
                }
                match kind {
                    "compressor_filter" => "compressor",
                    "expander_filter" => "expander",
                    _ => "upward_compressor",
                }
            }
            "limiter_filter" => {
                set("threshold_db", num("threshold"));
                set("release_ms", num("release_time"));
                "limiter"
            }
            "basic_eq_filter" => {
                set("low_db", num("low"));
                set("mid_db", num("mid"));
                set("high_db", num("high"));
                "equalizer"
            }
            "vst_filter" => {
                return Err("VST plugins aren't supported (Audio Units come later)".into());
            }
            _ => return Err("there is no matching audio filter".into()),
        };
        spec.insert("kind".into(), Value::from(kind));
        serde_json::from_value::<AudioFilterKind>(Value::Object(spec))
            .map_err(|err| format!("its settings didn't convert ({err})"))
    }

    /// The Isocast kind of an OBS video filter, or why there is none.
    fn filter_kind(&mut self, where_: &str, filter: Obs<'v>) -> Result<FilterKind, String> {
        let s = filter.settings;
        // The first versions kept opacity as a percentage.
        let opacity = || {
            match filter.kind.ends_with("_v2") {
                true => number(s, "opacity").unwrap_or(1.0),
                false => number(s, "opacity").unwrap_or(100.0) / 100.0,
            }
            .clamp(0.0, 1.0) as f32
        };
        let float = |key: &str, default: f64| number(s, key).unwrap_or(default) as f32;
        let key_type = |s: &Value, default: &str| {
            let custom = obs_color(int(s, "key_color").unwrap_or(0x00FF00), false);
            match text(s, "key_color_type").unwrap_or(default) {
                "green" => (KeyColorType::Green, custom),
                "blue" => (KeyColorType::Blue, custom),
                "magenta" => (KeyColorType::Magenta, custom),
                "red" => (KeyColorType::Custom, Color("#ff0000".into())),
                _ => (KeyColorType::Custom, custom),
            }
        };
        let adjust = KeyAdjust {
            contrast: float("contrast", 0.0),
            brightness: float("brightness", 0.0),
            gamma: float("gamma", 0.0),
        };
        let whole = |key: &str, default: i64| int(s, key).unwrap_or(default).clamp(1, 1000) as u32;
        Ok(match filter.base_kind() {
            "color_filter" => {
                let multiply = if filter.kind.ends_with("_v2") {
                    int(s, "color_multiply").unwrap_or(0xFFFFFF)
                } else {
                    int(s, "color").unwrap_or(0xFFFFFF)
                };
                FilterKind::ColorCorrection(ColorCorrection {
                    gamma: float("gamma", 0.0),
                    contrast: float("contrast", 0.0),
                    brightness: float("brightness", 0.0),
                    saturation: float("saturation", 0.0),
                    hue_shift: float("hue_shift", 0.0),
                    opacity: opacity(),
                    color_multiply: obs_color(multiply, false),
                    color_add: obs_color(int(s, "color_add").unwrap_or(0), false),
                })
            }
            "chroma_key_filter" => {
                let (key_color_type, key_color) = key_type(s, "green");
                FilterKind::ChromaKey(ChromaKey {
                    key_color_type,
                    key_color,
                    similarity: whole("similarity", 400),
                    smoothness: whole("smoothness", 80),
                    spill: whole("spill", 100),
                    opacity: opacity(),
                    adjust,
                })
            }
            "color_key_filter" => {
                let (key_color_type, key_color) = key_type(s, "green");
                FilterKind::ColorKey(ColorKey {
                    key_color_type,
                    key_color,
                    similarity: whole("similarity", 80),
                    smoothness: whole("smoothness", 50),
                    opacity: opacity(),
                    adjust,
                })
            }
            "luma_key_filter" => FilterKind::LumaKey(LumaKey {
                luma_max: float("luma_max", 1.0),
                luma_max_smooth: float("luma_max_smooth", 0.0),
                luma_min: float("luma_min", 0.0),
                luma_min_smooth: float("luma_min_smooth", 0.0),
            }),
            "crop_filter" => {
                let edge = |key: &str| int(s, key).unwrap_or(0) as i32;
                if flag(s, "relative").unwrap_or(true) {
                    FilterKind::CropPad(CropPad {
                        left: edge("left"),
                        top: edge("top"),
                        right: edge("right"),
                        bottom: edge("bottom"),
                        area: None,
                    })
                } else {
                    FilterKind::CropPad(CropPad {
                        area: Some(CropArea {
                            x: edge("left"),
                            y: edge("top"),
                            width: int(s, "cx").unwrap_or(0).max(0) as u32,
                            height: int(s, "cy").unwrap_or(0).max(0) as u32,
                        }),
                        ..CropPad::default()
                    })
                }
            }
            "scale_filter" => {
                let resolution = text(s, "resolution").unwrap_or_default();
                let (width, height, aspect) = if let Some((w, h)) = parse_size(resolution) {
                    (Some(w), Some(h), None)
                } else if resolution.contains(':') {
                    (None, None, Some(resolution.trim().to_string()))
                } else if resolution.to_ascii_lowercase().contains("base")
                    || resolution.to_ascii_lowercase().contains("canvas")
                {
                    (Some(self.to.0), Some(self.to.1), None)
                } else {
                    return Err(format!(
                        "it changes nothing (resolution {resolution:?}), so it was left out"
                    ));
                };
                FilterKind::Scaling(Scaling {
                    width,
                    height,
                    aspect,
                    scale_filter: match text(s, "sampling").unwrap_or("bicubic") {
                        "point" => ScaleFilter::Point,
                        "bilinear" => ScaleFilter::Bilinear,
                        "lanczos" => ScaleFilter::Lanczos,
                        "area" => ScaleFilter::Area,
                        _ => ScaleFilter::Bicubic,
                    },
                    undistort: flag(s, "undistort").unwrap_or(false),
                })
            }
            "scroll_filter" => {
                let limit = |on: &str, size: &str| {
                    int(s, size)
                        .filter(|v| flag(s, on) == Some(true) && *v > 0)
                        .map(|v| v as u32)
                };
                FilterKind::Scroll(Scroll {
                    horizontal_speed: float("speed_x", 0.0),
                    vertical_speed: float("speed_y", 0.0),
                    width: limit("limit_cx", "cx"),
                    height: limit("limit_cy", "cy"),
                    repeat: flag(s, "loop").unwrap_or(true),
                })
            }
            "sharpness_filter" => FilterKind::Sharpen(Sharpen {
                sharpness: float("sharpness", 0.08),
            }),
            "mask_filter" => {
                let Some(path) = text(s, "image_path").filter(|p| !p.is_empty()) else {
                    return Err("no image chosen".into());
                };
                let kind = text(s, "type").unwrap_or("mask_color_filter.effect");
                let mode = if kind.contains("mask_alpha") {
                    MaskMode::AlphaMaskAlpha
                } else if kind.contains("mult") {
                    MaskMode::BlendMultiply
                } else if kind.contains("add") {
                    MaskMode::BlendAdd
                } else if kind.contains("sub") {
                    MaskMode::BlendSubtract
                } else {
                    MaskMode::AlphaMaskColor
                };
                FilterKind::ImageMask(ImageMask {
                    mode,
                    path: self.path(path),
                    color: obs_color(int(s, "color").unwrap_or(0xFFFFFF), false),
                    opacity: opacity(),
                    stretch: flag(s, "stretch").unwrap_or(false),
                })
            }
            "clut_filter" => {
                let Some(path) = text(s, "image_path").filter(|p| !p.is_empty()) else {
                    return Err("no lookup table chosen".into());
                };
                let lower = path.to_ascii_lowercase();
                if !lower.ends_with(".cube") && !lower.ends_with(".png") {
                    return Err(format!(
                        "{path} isn't a .cube or .png table, which Isocast reads"
                    ));
                }
                FilterKind::ApplyLut(ApplyLut {
                    path: self.path(path),
                    amount: float("clut_amount", 1.0).clamp(0.0, 1.0),
                })
            }
            "gpu_delay" | "async_delay_filter" => {
                let delay = int(s, "delay_ms").unwrap_or(0).max(0);
                if delay > 500 {
                    self.notes.push(format!(
                        "{where_}: delay {delay} ms is more than Isocast's 500 ms; set to 500"
                    ));
                }
                if filter.base_kind() == "async_delay_filter" {
                    self.notes.push(format!(
                        "{where_}: Video Delay (Async) became a Render Delay, which delays the \
                         picture only"
                    ));
                }
                FilterKind::RenderDelay(RenderDelay {
                    delay_ms: delay.min(500) as u32,
                })
            }
            "advanced_masks_filter" => return self.advanced_mask(where_, s),
            _ => {
                return Err(format!(
                    "Isocast has no {:?} filter (a plugin or another system's kind)",
                    filter.kind
                ));
            }
        })
    }

    /// The Advanced Masks plugin's filter (#115) as a mask filter. Its
    /// settings are read as its scene collections store them (only those
    /// changed from its defaults); the numbering of its menus follows the
    /// order its interface lists them in, which the collections we have
    /// seen agree with. What doesn't map is said in a note.
    fn advanced_mask(&mut self, where_: &str, s: &Value) -> Result<FilterKind, String> {
        let float = |key: &str| number(s, key).map(|v| v as f32);
        if int(s, "mask_effect").unwrap_or(1) == 2 {
            return Err(
                "it is an adjustment mask (colour changes through a mask), which Isocast \
                        doesn't have; only alpha masks are imported"
                    .into(),
            );
        }
        // Its positions are in the masked source's pixels, whose size it
        // keeps with them; else the canvas's.
        let (w, h) = match (float("mask_source_width"), float("mask_source_height")) {
            (Some(w), Some(h)) if w > 0.0 && h > 0.0 => (w, h),
            _ => {
                self.notes.push(format!(
                    "{where_}: the masked picture's size isn't saved with the mask; its \
                     position and size are read against the canvas"
                ));
                (self.to.0 as f32, self.to.1 as f32)
            }
        };
        if flag(s, "shape_relative") == Some(true) {
            self.notes.push(format!(
                "{where_}: its Scene View Transformation (positions in the scene) isn't \
                 imported; the mask is placed on the source's own picture"
            ));
        }
        match int(s, "mask_type").unwrap_or(1) {
            1 => {}
            2 => {
                let channel = match int(s, "mask_source_mask_properties_list").unwrap_or(1) {
                    1 => MaskChannel::Alpha,
                    2 | 3 => MaskChannel::Luminance,
                    _ => {
                        self.notes.push(format!(
                            "{where_}: its custom channel mix became the mask's luminance"
                        ));
                        MaskChannel::Luminance
                    }
                };
                if int(s, "mask_source_compression_list").unwrap_or(1) != 1 {
                    self.notes.push(format!(
                        "{where_}: its threshold or range compression isn't imported; set the \
                         mask's threshold and softness"
                    ));
                }
                let Some(name) = text(s, "mask_source").filter(|n| !n.is_empty()) else {
                    return Err("no mask source chosen".into());
                };
                let (mask_source, mask_scene) =
                    match self.by_name.get(name).map(|&i| self.targets[i].clone()) {
                        Some(Target::Source {
                            id, video: true, ..
                        }) => (Some(id), None),
                        Some(Target::Scene(id)) => (None, Some(id)),
                        _ => {
                            return Err(format!(
                                "its mask source {name:?} isn't an imported source or scene with \
                                 a picture"
                            ));
                        }
                    };
                return Ok(FilterKind::SourceMask(SourceMask {
                    mask_source,
                    mask_scene,
                    channel,
                    ..SourceMask::default()
                }));
            }
            4 => {
                let position = float("mask_gradient_position").unwrap_or(w / 2.0);
                let width = float("mask_gradient_width").unwrap_or(w / 2.0).max(1.0);
                return Ok(FilterKind::GradientMask(GradientMask {
                    x: position / w,
                    angle: float("mask_gradient_rotation").unwrap_or(0.0),
                    length: width / w,
                    // From hidden to shown along it.
                    stops: vec![
                        GradientStop {
                            at: 0.0,
                            opacity: 0.0,
                        },
                        GradientStop {
                            at: 1.0,
                            opacity: 1.0,
                        },
                    ],
                    invert: flag(s, "mask_gradient_invert").unwrap_or(false),
                    ..GradientMask::default()
                }));
            }
            3 => {
                return Err(
                    "its image masks map to Isocast's image_mask filter; add one with \
                            the image file"
                        .into(),
                );
            }
            other => {
                return Err(format!(
                    "its mask type {other} (BSM alpha wipe or a newer kind) has no Isocast \
                     equivalent"
                ));
            }
        }
        let shape = match int(s, "shape_type").unwrap_or(1) {
            1 => MaskShape::Rectangle,
            2 => MaskShape::Circle,
            3 => MaskShape::Ellipse,
            4 => MaskShape::Polygon,
            5 => MaskShape::Star,
            6 => MaskShape::Heart,
            _ => {
                return Err(
                    "its squircle, superellipse or superformula shape has no Isocast equivalent"
                        .into(),
                );
            }
        };
        let (bw, bh) = match shape {
            MaskShape::Rectangle => (
                float("rectangle_width").unwrap_or(w * 0.8),
                float("rectangle_height").unwrap_or(h * 0.8),
            ),
            MaskShape::Circle => {
                let r = float("circle_radius").unwrap_or(w.min(h) * 0.4);
                (2.0 * r, 2.0 * r)
            }
            MaskShape::Ellipse => (
                float("shape_ellipse_a").unwrap_or(w * 0.8),
                float("shape_ellipse_b").unwrap_or(h * 0.8),
            ),
            MaskShape::Heart => {
                let size = float("heart_size").unwrap_or(w.min(h) * 0.8);
                (size, size)
            }
            MaskShape::Polygon | MaskShape::Star => {
                self.notes.push(format!(
                    "{where_}: its polygon or star size, sides and points aren't read; Isocast's \
                     defaults are used"
                ));
                (w * 0.8, h * 0.8)
            }
        };
        let radius = match (shape, int(s, "rectangle_corner_type").unwrap_or(1)) {
            (MaskShape::Rectangle, 2) => Radius::Each([
                float("rectangle_corner_radius_tl").unwrap_or(0.0),
                float("rectangle_corner_radius_tr").unwrap_or(0.0),
                float("rectangle_corner_radius_br").unwrap_or(0.0),
                float("rectangle_corner_radius_bl").unwrap_or(0.0),
            ]),
            (MaskShape::Rectangle, _) => {
                Radius::All(float("rectangle_corner_radius").unwrap_or(0.0))
            }
            (MaskShape::Star, _) => Radius::All(float("star_corner_radius").unwrap_or(0.0)),
            _ => Radius::All(0.0),
        };
        let feather = match int(s, "shape_feather_type").unwrap_or(1) {
            1 => 0.0,
            kind => {
                if kind != 3 {
                    self.notes.push(format!(
                        "{where_}: its inner or outer feather is centred on the edge in Isocast"
                    ));
                }
                float("shape_feather_amount")
                    .unwrap_or(0.0)
                    .clamp(0.0, 1000.0)
            }
        };
        Ok(FilterKind::ShapeMask(ShapeMask {
            shape,
            x: float("shape_center_x").unwrap_or(w / 2.0) / w,
            y: float("shape_center_y").unwrap_or(h / 2.0) / h,
            width: (bw / w).clamp(0.0, 10.0),
            height: (bh / h).clamp(0.0, 10.0),
            rotation: float("shape_rotation").unwrap_or(0.0),
            radius,
            feather,
            invert: flag(s, "shape_invert").unwrap_or(false),
            ..ShapeMask::default()
        }))
    }

    /// The Source Record plugin's filter: the source is recorded to its own
    /// file, which Isocast's ISO recording does for every source it lists.
    fn source_record(&mut self, where_: &str, id: &str, video: bool, enabled: bool, f: Obs<'v>) {
        let s = f.settings;
        if !enabled || int(s, "record_mode").unwrap_or(3) == 0 {
            self.counts.filters += 1;
            return;
        }
        if !video {
            self.unsupported(
                where_,
                f.kind,
                "ISO files record sources with a picture; this one has none",
            );
            return;
        }
        self.counts.filters += 1;
        if !self.iso.iter().any(|i| i == id) {
            self.iso.push(id.into());
        }
        if flag(s, "different_audio") == Some(true)
            && let Some(name) = text(s, "audio_source")
            && let Some(&j) = self.by_name.get(name)
            && let Target::Source {
                id: audio,
                audio: true,
                ..
            } = &self.targets[j]
        {
            self.iso_audio.insert(id.into(), vec![audio.clone()]);
        }
        let codec = match int(s, "codec_type").map(|c| (c as u32).to_be_bytes()) {
            Some(code) => match &code {
                b"hvc1" => Some(Codec::Hevc),
                b"avc1" => Some(Codec::H264),
                b"apco" => Some(Codec::Prores422Proxy),
                b"apcs" => Some(Codec::Prores422Lt),
                b"apcn" => Some(Codec::Prores422),
                b"apch" => Some(Codec::Prores422Hq),
                b"ap4h" => Some(Codec::Prores4444),
                _ => None,
            },
            None => None,
        };
        let encoder = IsoEncoder {
            codec: codec.unwrap_or(Codec::platform_default()),
            quality: (int(s, "quality").unwrap_or(70).clamp(1, 100) as u8).into(),
            ..IsoEncoder::default()
        };
        match &self.iso_encoder {
            None => self.iso_encoder = Some(encoder),
            Some(first) if *first != encoder => self.notes.push(format!(
                "{where_}: Source Record filters use different encoders; ISO files all use the \
                 first one's"
            )),
            Some(_) => {}
        }
        if flag(s, "scale") == Some(true) {
            self.notes.push(format!(
                "{where_}: ISO files are recorded at the source's own size, not scaled"
            ));
        }
        // Source Record names files by a pattern per filter ("FACECAM-%CCYY…");
        // Isocast names every ISO file from its source's id.
        if let Some(pattern) = text(s, "filename_formatting") {
            let prefix = pattern.split('%').next().unwrap_or_default();
            let ours = format!("{}-", id.to_uppercase());
            let wanted = prefix.trim_end_matches(['-', '_', ' ']);
            if !prefix.is_empty() && prefix != ours && !wanted.is_empty() {
                self.notes.push(format!(
                    "{where_}: Source Record named its files {prefix}…; Isocast names ISO files \
                     {ours}… from the source's id (recording.iso.filename is \
                     \"{{SOURCE}}-{{date}} {{time}}\"), so change the id to {:?} to keep the old \
                     names",
                    slug(wanted, "source")
                ));
            }
        }
    }

    fn add_scenes(&mut self) {
        let (from, to) = (self.from, self.to);
        let k = f64::from(to.1) / f64::from(from.1);
        let canvas = Space {
            kx: k,
            ky: k,
            ox: f64::from(to.0) / 2.0 - k * f64::from(from.0) / 2.0,
            oy: f64::from(to.1) / 2.0 - k * f64::from(from.1) / 2.0,
        };
        let mut audio_in_scenes = false;
        for i in 0..self.objects.len() {
            let Target::Scene(id) = self.targets[i].clone() else {
                continue;
            };
            let obs = self.objects[i];
            let label = format!("scene {:?}", obs.name);
            if flag(obs.settings, "custom_size") == Some(true) {
                self.unsupported(
                    label.clone(),
                    "scene",
                    "a custom scene size isn't available; it is drawn at the canvas size",
                );
            }
            let filters = obs.json.get("filters").and_then(Value::as_array);
            for filter in filters.into_iter().flatten().map(Obs::new) {
                self.counts.obs_filters += 1;
                self.unsupported(
                    format!("{label} filter {:?}", filter.name),
                    filter.kind,
                    "filters on scenes aren't available; Isocast filters sources",
                );
            }
            let private = obs.json.get("private_settings").unwrap_or(&Value::Null);
            if let Some(transition) = text(private, "transition").filter(|t| !t.is_empty()) {
                self.unsupported(
                    label.clone(),
                    "scene",
                    format!("its own transition ({transition:?}) isn't available yet"),
                );
            }
            let mut keys = HashSet::new();
            let before = self.audio_items;
            let items = self.items(obs.name, obs.settings, canvas, &mut keys, 0);
            audio_in_scenes |= self.audio_items > before;
            if let Some(scene) = self.scenes.iter_mut().find(|s| s.id == id) {
                scene.items = items;
            }
        }
        if audio_in_scenes {
            // These items aren't in the item count, so say where they went.
            self.notes.push(format!(
                "{} audio item(s) in OBS scenes became program audio rather than items: in \
                 Isocast audio sources are in the program mix all the time (OBS mixes them while \
                 their scene is on air)",
                self.audio_items
            ));
        }
    }

    /// The items of a scene or group, in `space`.
    fn items(
        &mut self,
        scene: &str,
        settings: &'v Value,
        space: Space,
        keys: &mut HashSet<String>,
        depth: u32,
    ) -> Vec<SceneItem> {
        let mut out = Vec::new();
        let items = settings.get("items").and_then(Value::as_array);
        for item in items.into_iter().flatten() {
            // Copies of a group's items kept for old versions of OBS.
            if flag(item, "group_item_backup") == Some(true) {
                continue;
            }
            self.counts.obs_items += 1;
            let name = text(item, "name").unwrap_or_default();
            let label = format!("scene {scene:?} item {name:?}");
            let index = text(item, "source_uuid")
                .and_then(|u| self.by_uuid.get(u))
                .or_else(|| self.by_name.get(name))
                .copied();
            let Some(index) = index else {
                self.unsupported(label, "scene_item", "its source isn't in the collection");
                continue;
            };
            let (source, nested, size) = match self.targets[index].clone() {
                Target::Source { video: false, .. } => {
                    self.audio_items += 1;
                    continue;
                }
                Target::Skipped => continue,
                Target::Source { id, .. } => {
                    let size = self.source_size(index);
                    if size.is_none() {
                        let (w, h) = self.from;
                        let source = self.objects[index].name;
                        self.notes
                            .push(match int(item, "bounds_type").unwrap_or(0) {
                                0 => format!(
                                    "{label}: the size of {source:?} isn't known here, so its rect \
                                 assumes {w}×{h}; check it"
                                ),
                                _ => format!(
                                    "{label}: the size of {source:?} isn't known here, so it fills \
                                 its bounding box; check it"
                                ),
                            });
                    }
                    (Some(id), None, size)
                }
                Target::Scene(id) => {
                    let size = (f64::from(self.from.0), f64::from(self.from.1));
                    (None, Some(id), Some(size))
                }
                Target::Group => {
                    let s = self.objects[index].settings;
                    let size = (
                        number(s, "cx").unwrap_or(f64::from(self.from.0)).max(1.0),
                        number(s, "cy").unwrap_or(f64::from(self.from.1)).max(1.0),
                    );
                    (None, None, Some(size))
                }
            };
            let placed = self.place(&label, item, size, space);
            if placed.mirrored {
                self.unsupported(
                    label.clone(),
                    "scene_item",
                    "it is flipped, which isn't available; shown unflipped",
                );
            }
            self.item_extras(&label, item);
            let natural = source
                .clone()
                .or(nested.clone())
                .unwrap_or_else(|| slug(name, "group"));
            let mut key = natural.clone();
            let mut n = 2;
            while keys.contains(&key) {
                key = format!("{natural}-{n}");
                n += 1;
            }
            keys.insert(key.clone());
            let group = matches!(self.targets[index], Target::Group);
            let children = if let (true, Some(size)) = (group && depth < MAX_NESTING, size) {
                let inner = Space {
                    kx: placed.size.0 / size.0,
                    ky: placed.size.1 / size.1,
                    ox: 0.0,
                    oy: 0.0,
                };
                let settings = self.objects[index].settings;
                self.items(scene, settings, inner, keys, depth + 1)
            } else {
                Vec::new()
            };
            self.counts.items += 1;
            out.push(SceneItem {
                id: (group || key != natural).then_some(key),
                source,
                scene: nested,
                content: None,
                rect: Some(placed.rect),
                layout: None,
                crop: if group { Crop::default() } else { placed.crop },
                visible: flag(item, "visible").unwrap_or(true),
                show_transition: self.item_transition(&label, item, "show_transition"),
                hide_transition: self.item_transition(&label, item, "hide_transition"),
                animations: Vec::new(),
                locked: flag(item, "locked").unwrap_or(false),
                // Bounds are already worked into the rect and crop.
                fit: Fit::Stretch,
                align: Align::Center,
                scale_filter: item_scale_filter(item),
                blend: item_blend(item),
                style: Style {
                    transform: Transform {
                        rotate: placed.rotate,
                        ..Transform::default()
                    },
                    ..Style::default()
                },
                children,
            });
        }
        out
    }

    /// Item settings Isocast doesn't have yet.
    fn item_extras(&mut self, label: &str, item: &Value) {
        if let Some(method) = text(item, "blend_method").filter(|b| *b != "default") {
            self.unsupported(
                label,
                "scene_item",
                format!("blending method {method:?} isn't available"),
            );
        }
    }

    /// An item's show or hide transition (`key`): OBS saves one as an
    /// object with the transition's id, settings and duration (just the
    /// duration when there is none).
    fn item_transition(&mut self, label: &str, item: &Value, key: &str) -> Option<ItemTransition> {
        let saved = item.get(key)?;
        let kind = text(saved, "id").filter(|k| !k.is_empty())?;
        let settings = saved.get("settings").unwrap_or(&Value::Null);
        let duration_ms = int(saved, "duration")
            .filter(|ms| *ms > 0)
            .unwrap_or(300)
            .clamp(1, i64::from(MAX_TRANSITION_MS)) as u32;
        let direction = match text(settings, "direction").unwrap_or("left") {
            "right" => Direction::Right,
            "up" => Direction::Up,
            "down" => Direction::Down,
            _ => Direction::Left,
        };
        let which = key.trim_end_matches("_transition");
        let kind = match kind.trim_end_matches("_v2") {
            "cut_transition" => return None,
            "fade_transition" => ItemTransitionKind::Fade,
            "slide_transition" | "swipe_transition" => ItemTransitionKind::Slide,
            other => {
                self.unsupported(
                    label,
                    other,
                    format!("Isocast's {which} transitions fade or slide; this one fades"),
                );
                ItemTransitionKind::Fade
            }
        };
        Some(ItemTransition {
            kind,
            duration_ms,
            direction,
            easing: Easing::default(),
        })
    }

    /// The size OBS saw a source at, if it can be known here.
    fn source_size(&mut self, index: usize) -> Option<(f64, f64)> {
        if !self.sizes.contains_key(&index) {
            let size = self.find_size(index);
            self.sizes.insert(index, size);
        }
        self.sizes[&index]
    }

    fn find_size(&self, index: usize) -> Option<(f64, f64)> {
        let pair = |(w, h): (u32, u32)| Some((f64::from(w), f64::from(h)));
        let Target::Source { id, .. } = &self.targets[index] else {
            return None;
        };
        let kind = &self.sources.iter().find(|x| &x.id == id)?.kind;
        let devices = self.options.devices;
        match kind {
            SourceKind::Browser(browser) => pair((browser.width, browser.height)),
            SourceKind::Color(color) => pair((
                color.width.unwrap_or(self.from.0),
                color.height.unwrap_or(self.from.1),
            )),
            SourceKind::Camera(camera) => match camera.width.zip(camera.height) {
                Some(size) => pair(size),
                None => {
                    let found = devices?
                        .cameras
                        .iter()
                        .find(|c| c.name == camera.device || c.id == camera.device)?;
                    // The format Isocast picks: the highest frame rate up to
                    // the canvas's, then the largest size that fits on the
                    // canvas either way round (else the smallest larger one).
                    let fps = f64::from(self.fps) + 0.01;
                    let (cw, ch) = self.to;
                    let key = |f: &crate::devices::VideoFormat| {
                        let rate = f
                            .frame_rates
                            .iter()
                            .copied()
                            .filter(|r| *r <= fps)
                            .fold(0.0, f64::max);
                        let area = f64::from(f.width) * f64::from(f.height);
                        let fits = f.width.max(f.height) <= cw.max(ch)
                            && f.width.min(f.height) <= cw.min(ch);
                        let turned = (f.width >= f.height) == (cw >= ch);
                        (rate, fits, if fits { area } else { -area }, turned)
                    };
                    let best = found.formats.iter().max_by(|a, b| {
                        key(a)
                            .partial_cmp(&key(b))
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })?;
                    pair((best.width, best.height))
                }
            },
            SourceKind::Display(DisplaySource { display, .. })
            | SourceKind::Application(ApplicationSource { display, .. }) => {
                let found = devices?.displays.iter().find(|d| {
                    d.id.eq_ignore_ascii_case(display) || (display == "main" && d.main)
                })?;
                pair((found.width, found.height))
            }
            SourceKind::Text(TextSource {
                width: Some(w),
                height: Some(h),
                ..
            }) => pair((*w, *h)),
            _ => (self.options.content_size)(kind).and_then(pair),
        }
    }

    /// Where an OBS item lands in Isocast: OBS places the item's box (its
    /// source's size, cropped, times its scale, or its bounding box) with
    /// its alignment point at `pos`, rotated about that point; Isocast
    /// places a rect and rotates it about its centre. Without the source's
    /// size, the item fills its bounding box, or is canvas-sized.
    fn place(
        &mut self,
        label: &str,
        item: &Value,
        size: Option<(f64, f64)>,
        space: Space,
    ) -> Placed {
        let known = size.is_some();
        let size = size.unwrap_or((f64::from(self.from.0), f64::from(self.from.1)));
        let pos = vec2(item, "pos", 0.0);
        let rot = number(item, "rot").unwrap_or(0.0);
        let (sx, sy) = vec2(item, "scale", 1.0);
        let align = int(item, "align").unwrap_or(5);
        let crop_of = |key: &str| int(item, key).unwrap_or(0).max(0) as f64;
        let crop = [
            crop_of("crop_left"),
            crop_of("crop_top"),
            crop_of("crop_right"),
            crop_of("crop_bottom"),
        ];
        // The part of the source shown.
        let vw = (size.0 - crop[0] - crop[2]).max(1.0);
        let vh = (size.1 - crop[1] - crop[3]).max(1.0);
        let mut extra = [0.0; 4];
        let (box_w, box_h, content) = match int(item, "bounds_type").unwrap_or(0) {
            0 => {
                let (w, h) = (vw * sx.abs(), vh * sy.abs());
                (w, h, (0.0, 0.0, w, h))
            }
            kind => {
                let (bw, bh) = vec2(item, "bounds", 0.0);
                let (bw, bh) = (bw.abs().max(1.0), bh.abs().max(1.0));
                let fit = (bw / vw).min(bh / vh);
                let scale = match kind {
                    _ if !known => None,
                    2 => Some(fit),
                    3 => Some((bw / vw).max(bh / vh)),
                    4 => Some(bw / vw),
                    5 => Some(bh / vh),
                    6 => Some(fit.min(1.0)),
                    _ => None,
                };
                let (w, h) = scale.map_or((bw, bh), |s| (vw * s, vh * s));
                let bounds_align = int(item, "bounds_align").unwrap_or(0);
                let mut content = (
                    offset(bounds_align, 1, 2, bw - w),
                    offset(bounds_align, 4, 8, bh - h),
                    w,
                    h,
                );
                if flag(item, "bounds_crop") == Some(true) && kind != 1 {
                    // Cut what overflows the box, in source pixels.
                    let (per_x, per_y) = (vw / w, vh / h);
                    let (x0, y0) = (content.0.max(0.0), content.1.max(0.0));
                    let (x1, y1) = ((content.0 + w).min(bw), (content.1 + h).min(bh));
                    extra = [
                        (x0 - content.0) * per_x,
                        (y0 - content.1) * per_y,
                        (content.0 + w - x1) * per_x,
                        (content.1 + h - y1) * per_y,
                    ];
                    content = (x0, y0, (x1 - x0).max(1.0), (y1 - y0).max(1.0));
                }
                (bw, bh, content)
            }
        };
        let anchor = (offset(align, 1, 2, box_w), offset(align, 4, 8, box_h));
        let (flip_x, flip_y) = (sx < 0.0, sy < 0.0);
        let mut d = (
            content.0 + content.2 / 2.0 - anchor.0,
            content.1 + content.3 / 2.0 - anchor.1,
        );
        if flip_x {
            d.0 = -d.0;
        }
        if flip_y {
            d.1 = -d.1;
        }
        let (sin, cos) = rot.to_radians().sin_cos();
        let centre = (pos.0 + d.0 * cos - d.1 * sin, pos.1 + d.0 * sin + d.1 * cos);
        let centre = (
            space.ox + space.kx * centre.0,
            space.oy + space.ky * centre.1,
        );
        let (w, h) = (content.2 * space.kx, content.3 * space.ky);
        if rot % 90.0 != 0.0 && (space.kx - space.ky).abs() > 1e-6 {
            self.notes.push(format!(
                "{label}: rotated inside a group scaled unevenly, which Isocast approximates"
            ));
        }
        let mut rotate = rot + if flip_x && flip_y { 180.0 } else { 0.0 };
        rotate = (rotate + 180.0).rem_euclid(360.0) - 180.0;
        let round = |v: f64| v.round().max(0.0) as u32;
        Placed {
            rect: Rect {
                x: (centre.0 - w / 2.0).round() as i32,
                y: (centre.1 - h / 2.0).round() as i32,
                width: round(w).max(1),
                height: round(h).max(1),
            },
            crop: Crop {
                left: round(crop[0] + extra[0]),
                top: round(crop[1] + extra[1]),
                right: round(crop[2] + extra[2]),
                bottom: round(crop[3] + extra[3]),
            },
            rotate: if rotate == -180.0 { 180.0 } else { rotate } as f32,
            mirrored: flip_x != flip_y,
            size: (w, h),
        }
    }

    /// The setup's transition: OBS's current one, where Isocast has it.
    fn transition(&mut self) -> Transition {
        let configured: Vec<Obs<'v>> = self
            .collection
            .get("transitions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(Obs::new)
            .collect();
        let current = text(self.collection, "current_transition").unwrap_or("Fade");
        let duration = int(self.collection, "transition_duration").unwrap_or(300);
        let mut ms = duration.clamp(0, 10_000) as u32;
        if duration > 10_000 {
            self.notes.push(format!(
                "the transition lasts {duration} ms in OBS; Isocast's longest is 10 s"
            ));
        }
        for t in &configured {
            if t.name != current {
                self.unsupported(
                    format!("transition {:?}", t.name),
                    t.kind,
                    "only OBS's current transition is imported; switches can still name any \
                     transition kind",
                );
            }
        }
        let found = configured.iter().find(|t| t.name == current).copied();
        let kind = match found {
            Some(t) => t.base_kind(),
            None if current == "Cut" => "cut_transition",
            None => "fade_transition",
        };
        let settings = found.map_or(&Value::Null, |t| t.settings);
        match kind {
            "cut_transition" => Transition::default(),
            "fade_transition" => Transition::fade(ms),
            other => match self.transition_kind(other, settings) {
                Ok(spec) => {
                    let mut spec = spec;
                    spec["duration_ms"] = Value::from(ms.max(1));
                    match serde_json::from_value::<Transition>(spec) {
                        Ok(transition) => transition,
                        Err(err) => {
                            self.unsupported(
                                format!("transition {current:?}"),
                                other,
                                format!("its settings didn't convert ({err}); a fade instead"),
                            );
                            Transition::fade(ms.max(1))
                        }
                    }
                }
                Err(why) => {
                    ms = ms.max(1);
                    self.unsupported(
                        format!("transition {current:?}"),
                        other,
                        format!("{why}; a {ms} ms fade instead"),
                    );
                    Transition::fade(ms)
                }
            },
        }
    }

    /// An Isocast transition (as setup data, without its duration) for an
    /// OBS transition of `kind` with `settings`, or why there is none.
    fn transition_kind(&mut self, kind: &str, settings: &Value) -> Result<Value, String> {
        let direction = || match text(settings, "direction").unwrap_or("left") {
            "right" => "right",
            "up" => "up",
            "down" => "down",
            _ => "left",
        };
        Ok(match kind {
            "swipe_transition" => serde_json::json!({
                "kind": "swipe",
                "direction": direction(),
                "swipe_in": flag(settings, "swipe_in").unwrap_or(false),
            }),
            "slide_transition" => serde_json::json!({
                "kind": "slide",
                "direction": direction(),
            }),
            "fade_to_color_transition" => {
                let color = obs_color(int(settings, "color").unwrap_or(0xFF00_0000), false);
                serde_json::json!({
                    "kind": "fade_to_color",
                    "color": color.0,
                    "switch_point": number(settings, "switch_point").unwrap_or(50.0),
                })
            }
            "wipe_transition" => {
                // OBS names its built-in wipes after their image files.
                let image = text(settings, "luma_image").unwrap_or("linear-h.png");
                let name = image
                    .rsplit(|c| c == '/' || (cfg!(windows) && c == '\\'))
                    .next()
                    .unwrap_or(image)
                    .to_ascii_lowercase();
                let pattern = match name.trim_end_matches(".png") {
                    n if n.starts_with("linear-v") => "vertical",
                    n if n.starts_with("linear-t") || n.starts_with("linear-b") => "diagonal",
                    n if n.starts_with("linear") => "horizontal",
                    n if n.starts_with("barndoor-v") => "barn_door_vertical",
                    n if n.starts_with("barndoor") => "barn_door_horizontal",
                    n if n.starts_with("blinds-v") => "blinds_vertical",
                    n if n.starts_with("blinds") => "blinds_horizontal",
                    n if n.starts_with("box") => "box",
                    n if n.starts_with("circle") || n.starts_with("spiral") => "circle",
                    n if n.starts_with("diamond") => "diamond",
                    n if n.starts_with("clock") || n.starts_with("radial") => "clock",
                    n if n.starts_with("checker") => "checkerboard",
                    _ => {
                        self.notes.push(format!(
                            "luma wipe image {image:?} has no built-in match; a horizontal wipe"
                        ));
                        "horizontal"
                    }
                };
                serde_json::json!({
                    "kind": "luma_wipe",
                    "pattern": pattern,
                    "softness": number(settings, "luma_softness").unwrap_or(0.03),
                    "invert": flag(settings, "luma_invert").unwrap_or(false),
                })
            }
            "obs_stinger_transition" => {
                let path = text(settings, "path")
                    .filter(|p| !p.is_empty())
                    .ok_or("a stinger with no video file")?;
                let mut spec = serde_json::json!({ "kind": "stinger", "file": self.path(path) });
                // OBS's transition point is in milliseconds unless it counts
                // frames (tp_type 1).
                let point = number(settings, "transition_point").unwrap_or(0.0);
                match int(settings, "tp_type").unwrap_or(0) {
                    0 if point > 0.0 => spec["cut_point_ms"] = Value::from(point.round() as u32),
                    0 => {}
                    _ => self.notes.push(format!(
                        "stinger {path:?}: its cut point counts frames; it cuts at the video's \
                         middle instead"
                    )),
                }
                if flag(settings, "track_matte_enabled").unwrap_or(false) {
                    let layout = match int(settings, "track_matte_layout").unwrap_or(0) {
                        0 => "side_by_side",
                        1 => "stacked",
                        _ => return Err("a track matte in a separate file isn't supported".into()),
                    };
                    spec["matte"] = serde_json::json!({ "layout": layout });
                }
                spec
            }
            "move_transition" => serde_json::json!({ "kind": "move" }),
            _ => return Err("there is no matching transition".into()),
        })
    }

    fn recording(&mut self) -> Recording {
        let profile = self.options.profile;
        let (folder, container, codec, filename) =
            recording_settings(profile, self.options.home, &mut self.profile);
        // OBS's Output (Scaled) Resolution, unless the import moves the
        // collection onto another canvas.
        let (width, height) = match profile {
            Some(profile)
                if self.options.canvas.is_none()
                    && profile.output.is_some()
                    && profile.canvas != profile.output =>
            {
                program_size(profile, self.to, &mut self.profile)
            }
            _ => (None, None),
        };
        // The program is one mix: that of the first track OBS recorded.
        let tracks = profile.map_or(1, |p| p.tracks).max(1);
        let track = tracks.isolate_lowest_one();
        if tracks != track {
            let numbers: Vec<String> = (0..6)
                .filter(|bit| tracks & (1 << bit) != 0)
                .map(|bit| (bit + 1).to_string())
                .collect();
            self.unsupported(
                "profile recording audio tracks",
                "RecTracks",
                format!(
                    "OBS recorded audio tracks {}; Isocast records one program mix, so its \
                     audio is track {}'s sources",
                    numbers.join(", "),
                    track.trailing_zeros() + 1
                ),
            );
        }
        let program_audio = (0..self.objects.len())
            .filter_map(|i| match &self.targets[i] {
                Target::Source {
                    id, audio: true, ..
                } if int(self.objects[i].json, "mixers").unwrap_or(255) & i64::from(track) != 0 => {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        Recording {
            folder,
            program: ProgramRecording {
                enabled: !self.scenes.is_empty(),
                filename,
                encoder: Encoder {
                    codec,
                    container,
                    ..Encoder::default()
                },
                audio: program_audio,
                width,
                height,
            },
            iso: IsoRecording {
                enabled: !self.iso.is_empty(),
                sources: std::mem::take(&mut self.iso),
                encoder: self.iso_encoder.clone().unwrap_or(IsoEncoder {
                    container,
                    ..IsoEncoder::default()
                }),
                // Without "different audio", Source Record records the
                // source's own sound, as Isocast's ISO files do by default.
                source_audio: std::mem::take(&mut self.iso_audio),
                ..IsoRecording::default()
            },
            keep_free_gb: default_keep_free_gb(),
            replay: Default::default(),
            open_editor: serde::de::IgnoredAny,
        }
    }

    /// The profile's stream service (without the key, which stays in OBS),
    /// and its other settings an import keeps: reconnecting and the
    /// monitoring device.
    fn stream_and_audio(&mut self) -> (Option<StreamSettings>, AudioSetup) {
        let mut audio = AudioSetup::default();
        let Some(profile) = self.options.profile else {
            return (None, audio);
        };
        let mut stream = stream_settings(profile, &mut self.profile);
        profile_extras(profile, &mut stream, &mut audio, &mut self.profile);
        plugin_destinations(
            &profile.multistream,
            &mut stream,
            (None, None),
            "Aitum Multistream",
            &mut self.profile,
        );
        (stream, audio)
    }

    fn finish(mut self) -> (Imported, Findings) {
        let transition = self.transition();
        let recording = self.recording();
        let (stream, audio) = self.stream_and_audio();
        let (vertical, canvases) = match self.options.profile {
            Some(profile) => plugin_canvases(profile, &mut self.profile),
            None => (None, Vec::new()),
        };
        if self
            .collection
            .get("canvases")
            .and_then(Value::as_array)
            .is_some_and(|c| !c.is_empty())
        {
            self.unsupported(
                "canvases",
                "canvas",
                "OBS's own extra canvases aren't imported; Aitum Vertical Canvas's are (with \
                 the profile), or add one with `isocast canvas add ID --size WxH`",
            );
        }
        // A setup starts on its first scene; OBS kept the one on air.
        let on_air = text(self.collection, "current_program_scene")
            .or_else(|| text(self.collection, "current_scene"))
            .and_then(|name| Some((name, self.by_name.get(name)?)))
            .and_then(|(name, i)| match &self.targets[*i] {
                Target::Scene(id) => Some((name, id.clone())),
                _ => None,
            });
        let first = self.scenes.first().map(|s| s.id.clone());
        if let (Some((name, id)), Some(first)) = (on_air, first)
            && first != id
        {
            self.notes.push(format!(
                "OBS had scene {name:?} on air; Isocast starts on the first scene, {first:?}: \
                 put {id:?} on air with `isocast scene {id}` (MCP: switch_scene)"
            ));
        }
        let setup = Setup {
            virtual_camera: Default::default(),
            developer: Default::default(),
            governor: Default::default(),
            version: SETUP_VERSION,
            name: text(self.collection, "name")
                .filter(|n| !n.is_empty())
                .unwrap_or("Imported from OBS")
                .to_string(),
            canvas: Canvas {
                width: self.to.0,
                height: self.to.1,
                fps: self.fps,
            },
            sources: self.sources,
            scenes: self.scenes,
            keyframes: Vec::new(),
            transition,
            audio,
            recording,
            stream,
            vertical,
            canvases,
            hotkeys: Default::default(),
        };
        self.counts.scenes = setup.scenes.len();
        let imported = Imported {
            problems: setup.validate(),
            setup,
            unsupported: self.unsupported,
            notes: self.notes,
            counts: self.counts,
            collection: None,
            profile: None,
        };
        (imported, self.profile)
    }
}

/// Where a point sits along a box of `length` for OBS alignment flags: at
/// the start with `start`, the end with `end`, else the middle.
fn offset(align: i64, start: i64, end: i64, length: f64) -> f64 {
    if align & start != 0 {
        0.0
    } else if align & end != 0 {
        length
    } else {
        length / 2.0
    }
}

/// What an OBS audio filter is, if it is one.
fn audio_filter(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "gain_filter" => "Gain",
        "noise_gate_filter" => "Noise Gate",
        "noise_suppress_filter" => "Noise Suppression",
        "compressor_filter" => "Compressor",
        "limiter_filter" => "Limiter",
        "expander_filter" => "Expander",
        "upward_compressor_filter" => "Upward Compressor",
        "basic_eq_filter" => "3-Band Equalizer",
        "invert_polarity_filter" => "Invert Polarity",
        "vst_filter" => "VST plugin",
        _ => return None,
    })
}

/// A stable id from an OBS name: lowercase letters, digits and dashes,
/// made unique among `taken` with a number.
fn unique_id(name: &str, fallback: &str, taken: &mut HashSet<String>) -> String {
    let base = slug(name, fallback);
    let mut id = base.clone();
    let mut n = 2;
    while taken.contains(&id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    taken.insert(id.clone());
    id
}

/// `name` in lowercase letters, digits and dashes; `fallback` if nothing is
/// left.
fn slug(name: &str, fallback: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9') => out.push(c),
            _ if !out.is_empty() && !out.ends_with('-') => out.push('-'),
            _ => {}
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        fallback.to_string()
    } else {
        out.to_string()
    }
}

/// An OBS colour: a 32-bit number holding red in the lowest byte, then
/// green, blue and alpha. Without `alpha` (or when it is opaque) the
/// colour is written `#rrggbb`.
fn obs_color(value: i64, alpha: bool) -> Color {
    let [r, g, b, a] = (value as u32).to_le_bytes();
    if alpha && a != 255 {
        Color(format!("#{r:02x}{g:02x}{b:02x}{a:02x}"))
    } else {
        Color(format!("#{r:02x}{g:02x}{b:02x}"))
    }
}

/// The size a camera was set to: a session preset like
/// `AVCaptureSessionPreset1920x1080`, or the fast source's base64 format
/// ("1920x1080 30.000-60.000 …").
fn camera_format(s: &Value) -> Option<(u32, u32)> {
    if let Some(format) = text(s, "supported_format")
        && let Some(decoded) = base64(format)
        && let Some(size) = decoded.split_whitespace().next().and_then(parse_size)
    {
        return Some(size);
    }
    if flag(s, "use_preset") == Some(false) {
        let resolution: Value = serde_json::from_str(text(s, "resolution")?).ok()?;
        let w = int(&resolution, "width")?;
        let h = int(&resolution, "height")?;
        return (w > 0 && h > 0).then_some((w as u32, h as u32));
    }
    text(s, "preset")?
        .strip_prefix("AVCaptureSessionPreset")
        .and_then(|p| parse_size(p.trim_start_matches("iFrame")))
}

/// A camera's chosen frame rate, rounded to a whole rate.
fn camera_fps(s: &Value) -> Option<u32> {
    let rate = s.get("frame_rate")?;
    let num = number(rate, "numerator")?;
    let den = number(rate, "denominator")?;
    (num > 0.0 && den > 0.0).then(|| (num / den).round().clamp(1.0, 240.0) as u32)
}

/// "1920x1080" as numbers.
fn parse_size(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.trim().split_once('x')?;
    let (w, h): (u32, u32) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (w > 0 && h > 0 && w <= 16384 && h <= 16384).then_some((w, h))
}

/// Standard base64, as text.
fn base64(text: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0);
    for c in text.bytes().filter(|c| *c != b'=') {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    String::from_utf8(bytes).ok()
}

fn looks_like_image(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".tga", ".svg",
    ]
    .iter()
    .any(|e| lower.ends_with(e))
}

/// A text source's text, or the file it reads.
fn text_or_file(
    importer: &Importer<'_, '_>,
    s: &Value,
    from_file: &str,
    file: &str,
) -> (Option<String>, Option<String>) {
    match (flag(s, from_file), text(s, file)) {
        (Some(true), Some(path)) if !path.is_empty() => (None, Some(importer.path(path))),
        _ => (Some(text(s, "text").unwrap_or_default().to_string()), None),
    }
}

fn font_face(font: &Value) -> String {
    text(font, "face")
        .filter(|f| !f.is_empty())
        .unwrap_or("Helvetica")
        .to_string()
}

fn text<'v>(value: &'v Value, key: &str) -> Option<&'v str> {
    value.get(key)?.as_str()
}

fn number(value: &Value, key: &str) -> Option<f64> {
    value.get(key)?.as_f64()
}

fn int(value: &Value, key: &str) -> Option<i64> {
    let value = value.get(key)?;
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|v| v.round() as i64))
}

fn flag(value: &Value, key: &str) -> Option<bool> {
    value.get(key)?.as_bool()
}

fn vec2(value: &Value, key: &str, default: f64) -> (f64, f64) {
    let v = value.get(key).unwrap_or(&Value::Null);
    (
        number(v, "x").unwrap_or(default),
        number(v, "y").unwrap_or(default),
    )
}

/// The scale filter of an OBS scene item (OBS's "disable" is bilinear).
fn item_scale_filter(item: &Value) -> ScaleFilter {
    match text(item, "scale_filter").unwrap_or("disable") {
        "point" => ScaleFilter::Point,
        "bicubic" => ScaleFilter::Bicubic,
        "lanczos" => ScaleFilter::Lanczos,
        "area" => ScaleFilter::Area,
        _ => ScaleFilter::Bilinear,
    }
}

/// The blending mode of an OBS scene item.
fn item_blend(item: &Value) -> Blend {
    match text(item, "blend_type").unwrap_or("normal") {
        "additive" => Blend::Additive,
        "subtract" => Blend::Subtract,
        "screen" => Blend::Screen,
        "multiply" => Blend::Multiply,
        "lighten" => Blend::Lighten,
        "darken" => Blend::Darken,
        _ => Blend::Normal,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::devices::{Camera, CameraKind, Display, Microphone, VideoFormat};

    /// Import `collection` on a 1920×1080, 30 fps canvas, where images
    /// under /img/ are 200×100.
    fn run_with(
        collection: Value,
        canvas: Option<(u32, u32)>,
        devices: Option<&Devices>,
    ) -> Imported {
        let sizes = |kind: &SourceKind| match kind {
            SourceKind::Image(image) if image.path.starts_with("/img/") => Some((200, 100)),
            _ => None,
        };
        let options = Options {
            canvas,
            fps: Some(30),
            profile: None,
            devices,
            content_size: &sizes,
            home: Some("/Users/me"),
        };
        import(&collection, &options).unwrap()
    }

    fn run(collection: Value) -> Imported {
        run_with(collection, None, None)
    }

    fn image(name: &str, uuid: &str) -> Value {
        json!({ "name": name, "uuid": uuid, "id": "image_source",
                "settings": { "file": "/img/a.png" } })
    }

    fn scene(name: &str, items: Value) -> Value {
        json!({ "name": name, "id": "scene", "settings": { "items": items } })
    }

    fn collection(sources: Value) -> Value {
        json!({ "name": "Test", "resolution": { "x": 1920, "y": 1080 }, "sources": sources })
    }

    fn rect(item: &SceneItem) -> [i64; 4] {
        let r = item.rect.unwrap();
        [r.x.into(), r.y.into(), r.width.into(), r.height.into()]
    }

    #[test]
    fn ids_are_made_from_names_and_kept_unique() {
        let color = |name: &str| json!({ "name": name, "id": "color_source_v3", "settings": {} });
        let imported = run(collection(json!([
            color("Main Camera"),
            color("Main Camera"),
            color("program"),
            color("日本"),
            color("  Hello--World!! "),
        ])));
        let ids: Vec<&str> = imported
            .setup
            .sources
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "main-camera",
                "main-camera-2",
                "program-2",
                "source",
                "hello-world"
            ]
        );
        assert!(imported.problems.is_empty(), "{:?}", imported.problems);
    }

    #[test]
    fn items_are_placed_as_obs_places_them() {
        // OBS anchors an item's box at `pos` by its alignment (5 is top
        // left, 0 the centre, 10 bottom right) and rotates it about there.
        let items = json!([
            { "name": "A", "source_uuid": "a", "pos": { "x": 100, "y": 50 },
              "scale": { "x": 2, "y": 2 }, "align": 5 },
            { "name": "A", "source_uuid": "a", "pos": { "x": 960, "y": 540 }, "rot": 90,
              "scale": { "x": 1, "y": 1 }, "align": 0 },
            { "name": "A", "source_uuid": "a", "pos": { "x": 1920, "y": 1080 },
              "scale": { "x": 1, "y": 1 }, "align": 10, "crop_left": 50 },
            // Scale to inner bounds: 200×100 fits 400×400 as 400×200, centred.
            { "name": "A", "source_uuid": "a", "pos": { "x": 0, "y": 0 }, "align": 5,
              "bounds_type": 2, "bounds": { "x": 400, "y": 400 }, "bounds_align": 0 },
            // Outer bounds, cropped: 800×400 centred in 400×400 loses 200 px
            // on each side, 50 source pixels.
            { "name": "A", "source_uuid": "a", "pos": { "x": 0, "y": 0 }, "align": 5,
              "bounds_type": 3, "bounds": { "x": 400, "y": 400 }, "bounds_crop": true },
            { "name": "A", "source_uuid": "a", "pos": { "x": 10, "y": 20 }, "align": 5,
              "bounds_type": 1, "bounds": { "x": 300, "y": 300 } },
            // Flipped both ways about its top left: a half turn.
            { "name": "A", "source_uuid": "a", "pos": { "x": 300, "y": 300 },
              "scale": { "x": -1, "y": -1 }, "align": 5 },
            // To width, aligned right and bottom in the box.
            { "name": "A", "source_uuid": "a", "pos": { "x": 0, "y": 0 }, "align": 5,
              "bounds_type": 4, "bounds": { "x": 100, "y": 400 }, "bounds_align": 10 },
        ]);
        let imported = run(collection(json!([image("A", "a"), scene("S", items)])));
        let items = &imported.setup.scenes[0].items;
        let rects: Vec<[i64; 4]> = items.iter().map(rect).collect();
        assert_eq!(
            rects,
            [
                [100, 50, 400, 200],
                [860, 490, 200, 100],
                [1770, 980, 150, 100],
                [0, 100, 400, 200],
                [0, 0, 400, 400],
                [10, 20, 300, 300],
                [100, 200, 200, 100],
                [0, 350, 100, 50],
            ]
        );
        assert_eq!(items[1].style.transform.rotate, 90.0);
        assert_eq!(items[2].crop.left, 50);
        assert_eq!((items[4].crop.left, items[4].crop.right), (50, 50));
        assert_eq!(items[6].style.transform.rotate, 180.0);
        // Two items of one source need their own ids.
        assert_eq!(items[0].id, None);
        assert_eq!(items[1].id.as_deref(), Some("a-2"));
        assert!(imported.problems.is_empty(), "{:?}", imported.problems);
    }

    #[test]
    fn a_flip_on_one_axis_is_listed() {
        let items = json!([{ "name": "A", "source_uuid": "a", "align": 5,
                             "scale": { "x": -1, "y": 1 }, "pos": { "x": 200, "y": 0 } }]);
        let imported = run(collection(json!([image("A", "a"), scene("S", items)])));
        assert_eq!(rect(&imported.setup.scenes[0].items[0]), [0, 0, 200, 100]);
        assert!(imported.unsupported[0].reason.contains("flipped"));
    }

    #[test]
    fn an_unknown_size_fills_the_bounding_box_or_the_canvas() {
        let items = json!([
            { "name": "W", "source_uuid": "w", "align": 5, "pos": { "x": 10, "y": 10 },
              "bounds_type": 2, "bounds": { "x": 300, "y": 200 } },
            { "name": "W", "source_uuid": "w", "align": 5, "pos": { "x": 0, "y": 0 },
              "scale": { "x": 0.5, "y": 0.5 } },
        ]);
        let mic = json!({ "name": "Mic/Aux", "uuid": "m", "id": "coreaudio_input_capture",
                          "settings": {} });
        let mut json = collection(json!([
            { "name": "W", "uuid": "w", "id": "window_capture",
              "settings": { "owner_name": "Finder" } },
            scene("S", items),
            mic.clone(),
        ]));
        // A global audio device listed among the sources too comes in once.
        json["AuxAudioDevice1"] = mic;
        let imported = run(json);
        let items = &imported.setup.scenes[0].items;
        assert_eq!(rect(&items[0]), [10, 10, 300, 200]);
        assert_eq!(rect(&items[1]), [0, 0, 960, 540]);
        let notes: Vec<&String> = imported
            .notes
            .iter()
            .filter(|n| n.contains("isn't known"))
            .collect();
        assert!(notes[0].contains("fills its bounding box"), "{notes:?}");
        assert!(notes[1].contains("assumes 1920×1080"), "{notes:?}");
        assert_eq!(imported.setup.sources.len(), 2);
    }

    #[test]
    fn groups_become_items_with_children() {
        let group = json!({ "name": "Cards", "uuid": "g", "id": "group",
            "settings": { "cx": 400, "cy": 200, "items": [
                { "name": "A", "source_uuid": "a", "pos": { "x": 200, "y": 100 }, "align": 5 },
            ] } });
        let items = json!([
            { "name": "Cards", "source_uuid": "g", "pos": { "x": 100, "y": 100 },
              "scale": { "x": 0.5, "y": 0.5 }, "align": 5 },
            // OBS's backup copy of the group's item, for old versions.
            { "name": "A", "source_uuid": "a", "group_item_backup": true },
        ]);
        let mut json = collection(json!([image("A", "a"), scene("S", items)]));
        json["groups"] = json!([group]);
        let imported = run(json);
        let items = &imported.setup.scenes[0].items;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.as_deref(), Some("cards"));
        assert_eq!(items[0].source, None);
        assert_eq!(rect(&items[0]), [100, 100, 200, 100]);
        assert_eq!(rect(&items[0].children[0]), [100, 50, 100, 50]);
        assert_eq!(items[0].children[0].source.as_deref(), Some("a"));
        assert_eq!((imported.counts.obs_items, imported.counts.items), (2, 2));
        assert!(imported.problems.is_empty(), "{:?}", imported.problems);
    }

    #[test]
    fn nested_scenes_and_a_new_canvas_size_scale_together() {
        let inner = scene(
            "Inner",
            json!([{ "name": "A", "source_uuid": "a", "pos": { "x": 100, "y": 50 }, "align": 5 }]),
        );
        let outer = scene(
            "Outer",
            json!([{ "name": "Inner", "source_uuid": "in", "align": 5,
                     "scale": { "x": 0.5, "y": 0.5 }, "pos": { "x": 0, "y": 0 } }]),
        );
        let mut inner = inner;
        inner["uuid"] = json!("in");
        let mut json = collection(json!([image("A", "a"), inner, outer]));
        json["scene_order"] = json!([{ "name": "Outer" }, { "name": "Inner" }]);
        let imported = run_with(json, Some((3840, 2160)), None);
        let scenes = &imported.setup.scenes;
        assert_eq!(scenes[0].id, "outer");
        assert_eq!(scenes[0].items[0].scene.as_deref(), Some("inner"));
        assert_eq!(rect(&scenes[0].items[0]), [0, 0, 1920, 1080]);
        assert_eq!(rect(&scenes[1].items[0]), [200, 100, 400, 200]);
        assert_eq!(imported.setup.canvas.width, 3840);
        assert!(imported.problems.is_empty(), "{:?}", imported.problems);
    }

    #[test]
    fn video_filters_keep_their_settings() {
        let filters = json!([
            { "name": "Grade", "id": "color_filter", "versioned_id": "color_filter_v2",
              "settings": { "gamma": 0.5, "saturation": -1.0, "hue_shift": 30.0, "opacity": 0.5,
                            "color_multiply": 0xFF0000FFu32, "color_add": 0x0000FF00 } },
            { "name": "Key", "id": "chroma_key_filter", "versioned_id": "chroma_key_filter_v2",
              "enabled": false,
              "settings": { "key_color_type": "custom", "key_color": 0x00FF00FF,
                            "similarity": 300, "opacity": 0.75 } },
            { "name": "Old Key", "id": "color_key_filter", "settings": { "key_color_type": "red",
              "opacity": 50 } },
            { "name": "Cut", "id": "crop_filter", "settings": { "relative": false,
              "left": 10, "top": 20, "cx": 100, "cy": 50 } },
            { "name": "Size", "id": "scale_filter", "settings": { "resolution": "1280x720",
              "sampling": "lanczos" } },
            { "name": "Shape", "id": "scale_filter", "settings": { "resolution": "4:3" } },
            { "name": "Late", "id": "gpu_delay", "settings": { "delay_ms": 700 } },
            { "name": "Mask", "id": "mask_filter", "settings": { "image_path": "/Users/me/m.png",
              "type": "blend_mult_filter.effect", "opacity": 50 } },
            { "name": "Grade 2", "id": "clut_filter", "settings": { "image_path": "/l.3dl" } },
            { "name": "Shader", "id": "shader_filter", "settings": {} },
        ]);
        let mic_filters = json!([{ "name": "Gain", "id": "gain_filter", "settings": { "db": 6 } },
                                 { "name": "Grade", "id": "color_filter", "settings": {} }]);
        let imported = run(collection(json!([
            { "name": "Cam", "id": "image_source", "settings": { "file": "/img/a.png" },
              "filters": filters },
            { "name": "Mic", "id": "coreaudio_input_capture", "settings": {},
              "filters": mic_filters },
        ])));
        let filters = &imported.setup.sources[0].filters;
        let FilterKind::ColorCorrection(grade) = &filters[0].kind else {
            panic!("{filters:?}")
        };
        assert_eq!(
            (grade.gamma, grade.saturation, grade.hue_shift),
            (0.5, -1.0, 30.0)
        );
        assert_eq!(grade.opacity, 0.5);
        assert_eq!(grade.color_multiply.0, "#ff0000");
        assert_eq!(grade.color_add.0, "#00ff00");
        assert_eq!(filters[0].id.as_deref(), Some("grade"));
        let FilterKind::ChromaKey(key) = &filters[1].kind else {
            panic!()
        };
        assert!(!filters[1].enabled);
        assert_eq!(key.key_color_type, KeyColorType::Custom);
        assert_eq!(key.key_color.0, "#ff00ff");
        assert_eq!(
            (key.similarity, key.smoothness, key.opacity),
            (300, 80, 0.75)
        );
        let FilterKind::ColorKey(old) = &filters[2].kind else {
            panic!()
        };
        assert_eq!((old.key_color.0.as_str(), old.opacity), ("#ff0000", 0.5));
        let FilterKind::CropPad(crop) = &filters[3].kind else {
            panic!()
        };
        assert_eq!(
            crop.area,
            Some(CropArea {
                x: 10,
                y: 20,
                width: 100,
                height: 50
            })
        );
        let FilterKind::Scaling(size) = &filters[4].kind else {
            panic!()
        };
        assert_eq!((size.width, size.height), (Some(1280), Some(720)));
        assert_eq!(size.scale_filter, ScaleFilter::Lanczos);
        let FilterKind::Scaling(shape) = &filters[5].kind else {
            panic!()
        };
        assert_eq!(shape.aspect.as_deref(), Some("4:3"));
        assert_eq!(
            filters[6].kind,
            FilterKind::RenderDelay(RenderDelay { delay_ms: 500 })
        );
        let FilterKind::ImageMask(mask) = &filters[7].kind else {
            panic!()
        };
        assert_eq!(
            (mask.mode, mask.path.as_str(), mask.opacity),
            (MaskMode::BlendMultiply, "~/m.png", 0.5)
        );
        assert_eq!(filters.len(), 8);
        let left_out: Vec<&str> = imported
            .unsupported
            .iter()
            .map(|u| u.kind.as_str())
            .collect();
        assert_eq!(left_out, ["clut_filter", "shader_filter", "color_filter"]);
        // The microphone's gain filter comes over as an audio filter.
        let mic = imported
            .setup
            .sources
            .iter()
            .find(|s| s.kind.has_audio() && !s.kind.has_video())
            .expect("a microphone");
        assert_eq!(
            mic.audio_filters[0].kind,
            AudioFilterKind::Gain(Gain { gain_db: 6.0 })
        );
        assert_eq!(
            (imported.counts.obs_filters, imported.counts.filters),
            (12, 9)
        );
        assert!(imported.problems.is_empty(), "{:?}", imported.problems);
    }

    #[test]
    fn advanced_masks_filters_become_mask_filters() {
        // As the plugin's collections store them (defaults left out).
        let filters = json!([
            { "name": "Round", "id": "advanced_masks_filter",
              "versioned_id": "advanced_masks_filter_v2",
              "settings": { "mask_source_width": 1920, "mask_source_height": 1080,
                            "shape_center_x": 960.0, "shape_center_y": 540.0,
                            "rectangle_width": 1152.0, "rectangle_height": 540.0,
                            "rectangle_corner_radius": 20.0, "rectangle_corner_type": 1,
                            "shape_feather_type": 1 } },
            { "name": "Heart", "id": "advanced_masks_filter",
              "settings": { "mask_type": 1, "mask_source_width": 1920,
                            "mask_source_height": 1080, "shape_type": 6, "heart_size": 540,
                            "shape_center_x": 480.0, "shape_center_y": 270.0,
                            "shape_rotation": 24.0, "shape_invert": true } },
            { "name": "Cutout", "id": "advanced_masks_filter",
              "settings": { "mask_type": 2, "mask_source": "Logo",
                            "mask_source_mask_properties_list": 1 } },
            { "name": "Formula", "id": "advanced_masks_filter",
              "settings": { "shape_type": 7 } },
            { "name": "Grade", "id": "advanced_masks_filter",
              "settings": { "mask_effect": 2 } },
        ]);
        let imported = run(collection(json!([
            { "name": "Cam", "id": "image_source", "settings": { "file": "/img/a.png" },
              "filters": filters },
            { "name": "Logo", "id": "image_source", "settings": { "file": "/img/logo.png" } },
        ])));
        let filters = &imported.setup.sources[0].filters;
        let FilterKind::ShapeMask(round) = &filters[0].kind else {
            panic!("{filters:?}")
        };
        assert_eq!(round.shape, MaskShape::Rectangle);
        assert_eq!(
            (round.x, round.y, round.width, round.height),
            (0.5, 0.5, 0.6, 0.5)
        );
        assert_eq!(round.radius, Radius::All(20.0));
        let FilterKind::ShapeMask(heart) = &filters[1].kind else {
            panic!("{filters:?}")
        };
        assert_eq!(heart.shape, MaskShape::Heart);
        assert_eq!((heart.x, heart.y, heart.rotation), (0.25, 0.25, 24.0));
        assert!(heart.invert);
        let FilterKind::SourceMask(cutout) = &filters[2].kind else {
            panic!("{filters:?}")
        };
        let logo = &imported.setup.sources[1].id;
        assert_eq!(cutout.mask_source.as_ref(), Some(logo));
        assert_eq!(cutout.channel, MaskChannel::Alpha);
        assert_eq!(filters.len(), 3);
        assert_eq!(imported.unsupported.len(), 2, "{:?}", imported.unsupported);
        assert!(imported.problems.is_empty(), "{:?}", imported.problems);
    }

    #[test]
    fn sources_keep_their_settings() {
        let imported = run(collection(json!([
            { "name": "Page", "id": "browser_source", "settings": { "is_local_file": true,
              "local_file": "/Users/me/page.html", "width": 1080, "height": 1920,
              "fps_custom": true, "fps": 24, "reroute_audio": true, "restart_when_active": true } },
            { "name": "Clip", "id": "ffmpeg_source", "settings": { "local_file": "/m/clip.mov",
              "looping": true, "speed_percent": 50, "restart_on_activate": false } },
            { "name": "Stream", "id": "ffmpeg_source", "settings": { "is_local_file": false,
              "input": "rtmp://example/live" } },
            { "name": "List", "id": "vlc_source", "settings": { "playlist": [
              { "value": "/m/a.mp4" }, { "value": "/m/b.mp4", "hidden": true } ] } },
            { "name": "Title", "id": "text_ft2_source", "versioned_id": "text_ft2_source_v2",
              "settings": { "text": "Hi", "font": { "face": "Menlo", "size": 48, "flags": 3 },
                            "color1": 0xFF0000FFu32, "color2": 0xFF0000FFu32, "outline": true } },
            { "name": "Tint", "id": "color_source", "versioned_id": "color_source_v3",
              "settings": { "color": 0x80FF0000u32, "width": 640, "height": 360 } },
            { "name": "Slides", "id": "slideshow", "settings": { "slide_time": 2000,
              "transition": "cut", "randomize": true,
              "files": [{ "value": "/s/1.png" }, { "value": "/s/2.jpg" }] } },
            { "name": "App", "id": "screen_capture", "settings": { "type": 2,
              "application": "com.apple.Safari", "show_cursor": false } },
            { "name": "App Sound", "id": "sck_audio_capture", "settings": { "type": 1,
              "application": "com.apple.Music" } },
            { "name": "Win", "id": "screen_capture", "settings": { "type": 1, "window": 42 } },
            { "name": "Cam", "id": "macos-avcapture-fast", "settings": {
              "device": "UID-1", "device_name": "Cam Link 4K",
              "supported_format": "MTkyMHgxMDgwIDMwLjAwMC02MC4wMDAgMCAxMTExOTcwMzY5",
              "frame_rate": { "numerator": 60000, "denominator": 1001 } } },
            { "name": "Mic", "id": "coreaudio_input_capture", "settings": { "device_id": "default" },
              "volume": 0.5, "muted": true, "sync": 120_000_000, "balance": 0.25,
              "monitoring_type": 2 },
        ])));
        let kinds: Vec<SourceKind> = imported
            .setup
            .sources
            .iter()
            .map(|s| s.kind.clone())
            .collect();
        let SourceKind::Browser(page) = &kinds[0] else {
            panic!()
        };
        assert_eq!(page.file.as_deref(), Some("~/page.html"));
        assert_eq!((page.width, page.height, page.fps), (1080, 1920, 24));
        assert!(page.control_audio && page.refresh_when_shown && !page.shutdown_when_hidden);
        let SourceKind::Media(clip) = &kinds[1] else {
            panic!()
        };
        assert_eq!(
            (clip.repeat, clip.speed, clip.restart_on_activate),
            (true, 0.5, false)
        );
        let SourceKind::Media(list) = &kinds[2] else {
            panic!()
        };
        assert_eq!(list.files, ["/m/a.mp4"]);
        let SourceKind::Text(title) = &kinds[3] else {
            panic!()
        };
        assert_eq!(
            (title.text.as_deref(), title.font.as_str(), title.size),
            (Some("Hi"), "Menlo", 48.0)
        );
        assert_eq!((title.weight, title.italic), (700, true));
        assert_eq!(title.color.0, "#ff0000");
        assert!(title.outline.is_some());
        let SourceKind::Color(tint) = &kinds[4] else {
            panic!()
        };
        assert_eq!(tint.color.0, "#0000ff80");
        assert_eq!((tint.width, tint.height), (Some(640), Some(360)));
        let SourceKind::Slideshow(slides) = &kinds[5] else {
            panic!()
        };
        assert_eq!((slides.slide_ms, slides.shuffle), (2000, true));
        assert!(slides.transition.is_cut());
        assert_eq!(
            kinds[6],
            SourceKind::Application(ApplicationSource {
                app: "com.apple.Safari".into(),
                display: "main".into(),
                cursor: false
            })
        );
        assert_eq!(
            kinds[7],
            SourceKind::ApplicationAudio(ApplicationAudioSource {
                app: "com.apple.Music".into()
            })
        );
        let SourceKind::Camera(cam) = &kinds[8] else {
            panic!()
        };
        assert_eq!(cam.device, "Cam Link 4K");
        assert_eq!(
            (cam.width, cam.height, cam.fps),
            (Some(1920), Some(1080), Some(60))
        );
        let mic = &imported.setup.sources[9];
        assert_eq!(
            mic.kind,
            SourceKind::Microphone(MicrophoneSource::new("default"))
        );
        assert_eq!(
            (mic.audio.gain_db, mic.audio.muted, mic.audio.offset_ms),
            (-6.0, true, 120)
        );
        // OBS's balance 0.25 (0 left, 1 right) and "monitor and output".
        assert_eq!(
            (mic.audio.balance, mic.audio.monitor),
            (-0.5, Monitoring::MonitorAndOutput)
        );
        let left_out: Vec<(&str, &str)> = imported
            .unsupported
            .iter()
            .map(|u| (u.obs.as_str(), u.kind.as_str()))
            .collect();
        assert_eq!(
            left_out,
            [
                ("source \"Stream\"", "ffmpeg_source"),
                ("source \"Win\"", "screen_capture"),
            ]
        );
        // Audio sources on track 1 make the program mix.
        assert_eq!(
            imported.setup.recording.program.audio,
            ["page", "clip", "list", "app-sound", "mic"]
        );
        assert!(imported.problems.is_empty(), "{:?}", imported.problems);
    }

    #[test]
    fn devices_are_matched_against_this_mac() {
        let devices = Devices {
            displays: vec![Display {
                id: "D-1".into(),
                name: "Studio Display".into(),
                main: true,
                built_in: false,
                width: 5120,
                height: 2880,
                refresh_rate: 60.0,
            }],
            cameras: vec![Camera {
                id: "UID-1".into(),
                name: "Facecam".into(),
                model: String::new(),
                manufacturer: String::new(),
                kind: CameraKind::External,
                formats: vec![VideoFormat {
                    width: 1280,
                    height: 720,
                    pixel_format: "420v".into(),
                    frame_rates: vec![30.0],
                    frame_rate_ranges: Vec::new(),
                }],
                suspended: false,
            }],
            microphones: vec![Microphone {
                id: "AppleUSB:1".into(),
                name: "Wave:3".into(),
                model: String::new(),
                manufacturer: String::new(),
                default: false,
                sample_rates: vec![48000.0],
                channels: 1,
                channel_names: Vec::new(),
            }],
            ..Devices::default()
        };
        let items = json!([
            { "name": "Screen", "source_uuid": "s", "align": 5, "pos": { "x": 0, "y": 0 },
              "scale": { "x": 0.375, "y": 0.375 } },
            { "name": "Cam", "source_uuid": "c", "align": 5, "pos": { "x": 0, "y": 0 } },
        ]);
        let imported = run_with(
            collection(json!([
                { "name": "Screen", "uuid": "s", "id": "screen_capture",
                  "settings": { "display_uuid": "d-1" } },
                { "name": "Other", "id": "screen_capture",
                  "settings": { "display_uuid": "D-9" } },
                { "name": "Cam", "uuid": "c", "id": "macos-avcapture",
                  "settings": { "device": "UID-1", "device_name": "Old Name",
                                "preset": "AVCaptureSessionPresetHigh" } },
                { "name": "Mic", "id": "coreaudio_input_capture",
                  "settings": { "device_id": "AppleUSB:1" } },
                { "name": "Phone", "id": "macos-avcapture",
                  "settings": { "device": "UID-9", "device_name": "Face" } },
                scene("S", items),
            ])),
            None,
            Some(&devices),
        );
        let setup = &imported.setup;
        let device = |i: usize| match &setup.sources[i].kind {
            SourceKind::Display(d) => d.display.clone(),
            SourceKind::Camera(c) => c.device.clone(),
            SourceKind::Microphone(m) => m.device.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(device(0), "D-1");
        assert_eq!(device(2), "Facecam");
        assert_eq!(device(3), "Wave:3");
        // A missing camera keeps its id: its name is part of "Facecam".
        assert_eq!(device(4), "UID-9");
        assert!(
            imported
                .notes
                .iter()
                .any(|n| n.contains("D-9 isn't connected"))
        );
        // OBS sees the display at its mode's size, and the camera at the
        // format Isocast would pick.
        let items = &setup.scenes[0].items;
        assert_eq!(rect(&items[0]), [0, 0, 1920, 1080]);
        assert_eq!(rect(&items[1]), [0, 0, 1280, 720]);
    }

    #[test]
    fn the_current_transition_comes_over() {
        let with = |current: &str, duration: u32| {
            let mut json = collection(json!([
                { "name": "S", "id": "scene", "settings": { "items": [] },
                  "private_settings": { "transition": "Swipe" } },
            ]));
            json["current_transition"] = json!(current);
            json["transition_duration"] = json!(duration);
            json["transitions"] = json!([
                { "name": "Stinger", "id": "obs_stinger_transition", "settings": {} },
            ]);
            run(json)
        };
        let fade = with("Fade", 300);
        assert_eq!(fade.setup.transition, Transition::fade(300));
        let kinds: Vec<&str> = fade.unsupported.iter().map(|u| u.kind.as_str()).collect();
        assert_eq!(kinds, ["scene", "obs_stinger_transition"]);
        assert!(with("Cut", 300).setup.transition.is_cut());
        let stinger = with("Stinger", 900);
        assert_eq!(stinger.setup.transition, Transition::fade(900));
        assert!(stinger.unsupported[1].reason.contains("900 ms fade"));
        let mut json = collection(json!([]));
        json["current_transition"] = json!("Wipe");
        json["transitions"] = json!([{"name":"Wipe", "id":"wipe_transition",
            "settings":{"luma_image": if cfg!(windows) {
                r"C:\Program Files\obs-studio\linear-v.png"
            } else { "/Applications/OBS.app/linear-v.png" }}}]);
        let wipe = run(json);
        assert_eq!(
            serde_json::to_value(wipe.setup.transition).unwrap()["pattern"],
            "vertical"
        );
    }

    #[test]
    fn source_record_filters_become_iso_files() {
        let record = |enabled: bool| {
            json!({ "name": "Source Record", "id": "source_record_filter", "enabled": enabled,
                    "settings": { "record_mode": 3, "different_audio": true,
                                  "audio_source": "Mic", "quality": 80,
                                  "codec_type": u32::from_be_bytes(*b"apcn") } })
        };
        let imported = run(collection(json!([
            { "name": "Cam", "id": "image_source", "settings": { "file": "/img/a.png" },
              "filters": [record(true)] },
            { "name": "Screen", "id": "image_source", "settings": { "file": "/img/a.png" },
              "filters": [record(false)] },
            { "name": "Mic", "id": "coreaudio_input_capture", "settings": {} },
        ])));
        let iso = &imported.setup.recording.iso;
        assert!(iso.enabled);
        assert_eq!(iso.sources, ["cam"]);
        assert_eq!(iso.audio, None);
        assert_eq!(iso.source_audio["cam"], ["mic"]);
        assert_eq!(
            (iso.encoder.codec, iso.encoder.quality),
            (Codec::Prores422, 80.into())
        );
        assert!(
            imported.unsupported.is_empty(),
            "{:?}",
            imported.unsupported
        );
        assert_eq!(imported.counts.filters, 2);
    }

    #[test]
    fn profiles_give_the_frame_rate_and_recording_settings() {
        let simple = Profile::parse(
            "[General]\nName=Show\n[Output]\nMode=Simple\nFilenameFormatting=%CCYY-%MM-%DD \
             %hh-%mm-%ss\n[SimpleOutput]\nFilePath=/Users/me/Movies\nRecFormat2=fragmented_mp4\n\
             RecEncoder=apple_h264\n[Video]\nBaseCX=1920\nBaseCY=1080\nFPSType=0\n\
             FPSCommon=59.94\n",
        );
        assert_eq!(simple.name, "Show");
        assert_eq!(simple.canvas, Some((1920, 1080)));
        assert!((simple.fps.unwrap() - 59.94).abs() < 0.001);
        assert_eq!(simple.folder.as_deref(), Some("/Users/me/Movies"));
        let advanced = Profile::parse(
            "[Output]\nMode=Advanced\n[AdvOut]\nRecType=Standard\nRecFilePath=/r\n\
             RecEncoder=none\nEncoder=com.apple.videotoolbox.videoencoder.ave.hevc\n\
             RecFormat2=mkv\n[Video]\nFPSType=2\nFPSNum=30000\nFPSDen=1001\n",
        );
        assert_eq!(advanced.folder.as_deref(), Some("/r"));
        assert!((advanced.fps.unwrap() - 29.97).abs() < 0.001);

        let sizes = |_: &SourceKind| None;
        let import_with = |profile: &Profile| {
            let options = Options {
                canvas: None,
                fps: None,
                profile: Some(profile),
                devices: None,
                content_size: &sizes,
                home: Some("/Users/me"),
            };
            import(&collection(json!([])), &options).unwrap()
        };
        let imported = import_with(&simple);
        let setup = &imported.setup;
        assert_eq!(setup.canvas.fps, 60);
        assert_eq!(setup.recording.folder, "~/Movies");
        let folderless = import_with(&Profile::parse("[Output]\nMode=Simple\n"));
        assert_eq!(folderless.setup.recording.folder, FALLBACK_FOLDER);
        assert!(
            folderless
                .notes
                .iter()
                .any(|note| note.ends_with(&format!("using {FALLBACK_FOLDER}")))
        );
        if cfg!(any(windows, target_os = "linux")) {
            assert_eq!(folderless.setup.recording.folder, "~/Videos");
        }
        assert_eq!(setup.recording.program.filename, "{date} {time}");
        let encoder = &setup.recording.program.encoder;
        assert_eq!(
            (encoder.codec, encoder.container),
            (Codec::H264, Container::Mp4)
        );
        let imported = import_with(&advanced);
        assert_eq!(imported.setup.recording.program.encoder.codec, Codec::Hevc);
        assert_eq!(imported.unsupported[0].kind, "mkv");
        assert_eq!(imported.setup.stream, None);

        // The stream service comes over without its key.
        let mut streaming =
            Profile::parse("[Output]\nMode=Simple\n[SimpleOutput]\nVBitrate=4500\nABitrate=128\n");
        let folder = std::env::temp_dir().join(format!("isocast-obs-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("service.json"),
            r#"{"type":"rtmp_common","settings":{"service":"Twitch","server":"auto","key":"live_x"}}"#,
        )
        .unwrap();
        streaming.read_stream(&folder);
        let _ = std::fs::remove_dir_all(&folder);
        let imported = import_with(&streaming);
        let stream = imported.setup.stream.clone().unwrap();
        assert_eq!(stream.service, crate::stream::StreamService::Twitch);
        assert_eq!(stream.server, None);
        assert_eq!(
            (stream.video_bitrate_kbps, stream.audio_bitrate_kbps),
            (4500, 128)
        );
        assert!(!serde_json::to_string(&imported).unwrap().contains("live_x"));
    }
}
