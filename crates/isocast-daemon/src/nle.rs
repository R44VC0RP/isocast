//! `take_export_nle` (#104): a take as a project Final Cut Pro (FCPXML
//! 1.10), Premiere Pro (xmeml 5, Final Cut Pro 7 XML) or DaVinci Resolve
//! (either, or OpenTimelineIO) opens, pointing at the take's own program
//! and ISO files: nothing is encoded, copied or changed, and the only file
//! written is the project file.
//!
//! Layout, the same in every format: one video track per ISO source (in the
//! order the take lists them) with the program's on top, each holding that
//! source's file, so all angles stay lined up and a cut to another angle is
//! a click away. Only the program's track is on (the others are there,
//! switched off). The program's audio tracks are on and each ISO file's are
//! there but off. Each file's video and audio are linked.
//!
//! Why stacked tracks and not FCPXML's multicam clip: the same layout works
//! in all three formats and in Resolve, which reads stacked lanes reliably
//! but multicam clips only partly. (In FCPXML the lanes hang off one gap:
//! the ISO angles on lanes 1…n, the program above them, the audio below.)
//!
//! Timing comes from the take's manifest: each file's `start_offset_seconds`
//! places its first frame on the take's clock. The sequence runs at the
//! program's frame rate; frames are whole frames, so a file that started
//! part of a frame after the take's zero is lined up to the frame.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use isocast_protocol::PROGRAM;
use isocast_protocol::api::RpcError;
use isocast_protocol::manifest::SessionManifest;
use isocast_protocol::manifest::SessionStatus;
use isocast_protocol::nle::{
    NleApp, NleExported, NleFormat, NleTrack, TakeExportNleParams, take_length,
};
use serde_json::{Value, json};

use crate::{Daemon, check_folder};

fn invalid(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::INVALID_PARAMS,
        message: message.into(),
    }
}

fn failed(message: impl Into<String>) -> RpcError {
    RpcError {
        code: RpcError::FAILED,
        message: message.into(),
    }
}

/// Audio is 48 kHz in every recording (the engine's `SAMPLE_RATE`).
const SAMPLE_RATE: u32 = 48_000;
/// Floating-point slack when finding the frame on screen at a moment, in
/// frames (a manifest's times are rounded to microseconds).
const FRAME_SLACK: f64 = 1e-3;

/// A frame rate as a fraction, `num / den` frames a second (`60000/1001`
/// for 59.94).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rate {
    num: u64,
    den: u64,
}

impl Rate {
    fn whole(fps: u32) -> Self {
        Rate {
            num: u64::from(fps.max(1)),
            den: 1,
        }
    }

    fn fps(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// xmeml's timebase: frames per second rounded (60 for 59.94).
    fn timebase(self) -> u64 {
        (self.num + self.den / 2) / self.den
    }

    /// An NTSC rate (29.97, 59.94, 23.976): xmeml's `ntsc`.
    fn ntsc(self) -> bool {
        self.den == 1001
    }

    fn text(self) -> String {
        match self.den {
            1 => self.num.to_string(),
            den => format!("{}/{den}", self.num),
        }
    }

    /// `frames` frames as FCPXML's rational seconds (`"1001/60000s"`).
    fn seconds(self, frames: i64) -> String {
        time_text(frames * self.den as i64, self.num)
    }

    /// `frames` frames at this rate as frames at `to`, rounded.
    fn convert(self, frames: u64, to: Rate) -> u64 {
        let num = u128::from(frames) * u128::from(to.num) * u128::from(self.den);
        let den = u128::from(to.den) * u128::from(self.num);
        ((num + den / 2) / den) as u64
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    match b {
        0 => a.max(1),
        b => gcd(b, a % b),
    }
}

/// `num / den` seconds as FCPXML writes times: `0s`, `5s`, `1001/60000s`.
fn time_text(num: i64, den: u64) -> String {
    if num == 0 {
        return "0s".into();
    }
    let g = gcd(num.unsigned_abs(), den);
    let (num, den) = (num / g as i64, den / g);
    match den {
        1 => format!("{num}s"),
        den => format!("{num}/{den}s"),
    }
}

/// `frame` as non-drop-frame timecode at `rate` (`00:01:02:03`).
fn timecode(frame: u64, rate: Rate) -> String {
    let base = rate.timebase().max(1);
    let (ff, seconds) = (frame % base, frame / base);
    format!(
        "{:02}:{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60,
        ff
    )
}

/// A take, read from its manifest.
struct TakeClip {
    id: String,
    folder: PathBuf,
    manifest: SessionManifest,
    /// Its length in frames of its program's grid.
    frames: u64,
    fps: u32,
}

/// A media file the project points at.
#[derive(Debug, Clone)]
struct Media {
    path: PathBuf,
    name: String,
    source: String,
    width: u32,
    height: u32,
    rate: Rate,
    /// Its length in its own frames.
    frames: u64,
    /// Its audio tracks: the audio source and its channels.
    audio: Vec<(String, u32)>,
}

/// One clip's stretch of one file on one track.
#[derive(Debug, Clone)]
struct Item {
    /// Index into the clips, and into the media.
    clip: usize,
    media: usize,
    /// Where it starts on the sequence, and how long it is, in sequence
    /// frames.
    start: u64,
    length: u64,
    /// Its first frame in the file, in the file's frames.
    source_in: u64,
    enabled: bool,
}

#[derive(Debug, Clone)]
struct Track {
    /// The video source of its files.
    source: String,
    /// An audio track: which of the files' audio tracks (from 0) and its
    /// audio source.
    audio: Option<(usize, String)>,
    items: Vec<Item>,
}

impl Track {
    fn name(&self) -> String {
        match &self.audio {
            None => self.source.clone(),
            Some((_, audio)) => format!("{} · {audio}", self.source),
        }
    }
}

#[derive(Debug, Clone)]
struct Span {
    id: String,
    start: u64,
    length: u64,
}

/// The takes laid out on tracks, the same for every format.
#[derive(Debug, Clone)]
struct Model {
    name: String,
    rate: Rate,
    width: u32,
    height: u32,
    /// Sequence frames.
    duration: u64,
    clips: Vec<Span>,
    /// Bottom to top: the ISO sources, then the program.
    video: Vec<Track>,
    /// The program's audio tracks, then each ISO file's.
    audio: Vec<Track>,
    media: Vec<Media>,
    /// Sequence frame and label.
    markers: Vec<(u64, String)>,
    notes: Vec<String>,
}

/// Lays the clips out on tracks (see the module docs).
fn build(name: &str, takes: &[TakeClip]) -> Result<Model, String> {
    let first = takes.first().ok_or("no take to export")?;
    let rate = Rate::whole(first.fps);
    let size_file = first
        .manifest
        .files
        .iter()
        .find(|f| f.source == PROGRAM)
        .or_else(|| first.manifest.files.first())
        .ok_or_else(|| format!("clip {}'s take lists no files", first.id))?;
    let (width, height) = (size_file.width.max(2), size_file.height.max(2));
    // Sources in the order the takes list them, the program last (on top).
    let mut sources: Vec<String> = Vec::new();
    let mut audio_keys: Vec<(String, String)> = Vec::new();
    for take in takes {
        for file in &take.manifest.files {
            if !sources.contains(&file.source) {
                sources.push(file.source.clone());
            }
            for audio in &file.audio {
                let key = (file.source.clone(), audio.source.clone());
                if !audio_keys.contains(&key) {
                    audio_keys.push(key);
                }
            }
        }
    }
    sources.sort_by_key(|source| source == PROGRAM);
    // The program's audio first.
    audio_keys.sort_by_key(|(source, _)| source != PROGRAM);
    let mut video: Vec<Track> = sources
        .iter()
        .map(|source| Track {
            source: source.clone(),
            audio: None,
            items: Vec::new(),
        })
        .collect();
    let mut audio: Vec<Track> = audio_keys
        .iter()
        .map(|(source, audio)| Track {
            source: source.clone(),
            // The file's track number is set per item (takes may differ).
            audio: Some((0, audio.clone())),
            items: Vec::new(),
        })
        .collect();
    let mut media: Vec<Media> = Vec::new();
    let mut clips = Vec::new();
    let mut markers = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut note = |text: String| {
        if !notes.contains(&text) {
            notes.push(text);
        }
    };
    // Track numbers of each audio item within its file, per audio track.
    let mut audio_index: Vec<Vec<usize>> = vec![Vec::new(); audio.len()];
    let mut elapsed = 0.0;
    for (index, take) in takes.iter().enumerate() {
        // The track on: the program's, or (a take recorded without one) the
        // top ISO source's.
        let on = match take.manifest.files.iter().any(|f| f.source == PROGRAM) {
            true => PROGRAM.to_string(),
            false => sources
                .iter()
                .rev()
                .find(|s| take.manifest.files.iter().any(|f| &f.source == *s))
                .cloned()
                .unwrap_or_default(),
        };
        let take_rate = Rate::whole(take.fps);
        let start = (elapsed * rate.fps()).round() as u64;
        elapsed += take.frames as f64 / take_rate.fps();
        let end = (elapsed * rate.fps()).round() as u64;
        let length = end.saturating_sub(start);
        clips.push(Span {
            id: take.id.clone(),
            start,
            length,
        });
        if take.fps != first.fps {
            note(format!(
                "clip {} was recorded at {} fps and the sequence runs at {} fps (the first \
                 clip's); the editor converts it",
                take.id, take.fps, first.fps
            ));
        }
        let out_seconds = take.frames as f64 / take_rate.fps();
        for file in &take.manifest.files {
            let path = take.folder.join(&file.path);
            let (Some(offset), Some(seconds)) = (file.start_offset_seconds, file.duration_seconds)
            else {
                note(format!(
                    "take {}'s {} file {} never finished (the take was interrupted); it is left \
                     out",
                    take.id,
                    file.source,
                    path.display()
                ));
                continue;
            };
            if !path.exists() {
                note(format!(
                    "take {}'s {} file {} is missing: the editor shows it offline until it is \
                     relinked",
                    take.id,
                    file.source,
                    path.display()
                ));
            }
            let file_rate = Rate::whole(file.fps);
            // Its length on its own frame grid, from its duration: a file
            // that dropped frames still spans its whole time (an editor
            // holds a picture over a dropped slot), so its stored frame
            // count would end it early and cut the angle short. A recovered file without final counters
            // keeps its stored frames, when known.
            let from_duration = (seconds * file_rate.fps()).round().max(0.0) as u64;
            let file_frames = match (file.dropped_frames, file.frames) {
                (None, Some(frames)) => frames,
                _ => from_duration,
            };
            let media_index = match media.iter().position(|m| m.path == path) {
                Some(i) => i,
                None => {
                    media.push(Media {
                        name: path
                            .file_name()
                            .map_or_else(|| file.path.clone(), |n| n.to_string_lossy().into()),
                        path: path.clone(),
                        source: file.source.clone(),
                        width: file.width.max(2),
                        height: file.height.max(2),
                        rate: file_rate,
                        frames: file_frames,
                        audio: file
                            .audio
                            .iter()
                            .map(|a| (a.source.clone(), a.channels.max(1)))
                            .collect(),
                    });
                    media.len() - 1
                }
            };
            // The file's frame on screen at the take's start.
            let first_frame = (-offset * file_rate.fps() + FRAME_SLACK).floor() as i64;
            let (lead, source_in) = match first_frame {
                k if k < 0 => {
                    note(format!(
                        "take {}'s {} file starts {:.1} ms after the take: its first frame comes \
                         {} frame(s) in",
                        take.id,
                        file.source,
                        offset * 1000.0,
                        file_rate.convert(k.unsigned_abs(), rate)
                    ));
                    (file_rate.convert(k.unsigned_abs(), rate), 0)
                }
                k => (0, k as u64),
            };
            let available = file_rate.convert(file_frames.saturating_sub(source_in), rate);
            let item_length = length.saturating_sub(lead).min(available);
            if item_length < length.saturating_sub(lead) {
                note(format!(
                    "take {}'s {} file ends {} frame(s) before the program does",
                    take.id,
                    file.source,
                    length.saturating_sub(lead) - item_length
                ));
            }
            if item_length == 0 {
                continue;
            }
            let item = Item {
                clip: index,
                media: media_index,
                start: start + lead,
                length: item_length,
                source_in,
                enabled: file.source == on,
            };
            if let Some(track) = video.iter_mut().find(|t| t.source == file.source) {
                track.items.push(item.clone());
            }
            for (number, stream) in file.audio.iter().enumerate() {
                let Some(t) = audio.iter().position(|t| {
                    t.source == file.source
                        && t.audio.as_ref().is_some_and(|(_, a)| *a == stream.source)
                }) else {
                    continue;
                };
                audio[t].items.push(item.clone());
                audio_index[t].push(number);
            }
        }
        for marker in &take.manifest.markers {
            if marker.time_seconds >= 0.0 && marker.time_seconds < out_seconds {
                let at = start + (marker.time_seconds * rate.fps()).round() as u64;
                markers.push((at.min(end.saturating_sub(1)), marker.label.clone()));
            }
        }
    }
    // An audio track's file track number: the one most of its items have
    // (normally all of them).
    for (track, numbers) in audio.iter_mut().zip(&audio_index) {
        let number = numbers.first().copied().unwrap_or(0);
        if numbers.iter().any(|n| *n != number) {
            note(format!(
                "audio track {}: its takes carry it as different tracks of their files; the \
                 first take's is used",
                track.name()
            ));
        }
        if let Some((n, _)) = track.audio.as_mut() {
            *n = number;
        }
    }
    let duration = clips.last().map_or(0, |c| c.start + c.length);
    Ok(Model {
        name: name.to_string(),
        rate,
        width,
        height,
        duration,
        clips,
        video,
        audio,
        media,
        markers,
        notes,
    })
}

/// Sequence pixels per picture pixel when a picture of `source`'s size is
/// fitted into the sequence's, centred.
fn fit(source: (u32, u32), sequence: (u32, u32)) -> f64 {
    let (w, h) = (f64::from(source.0.max(1)), f64::from(source.1.max(1)));
    let (sw, sh) = (f64::from(sequence.0.max(1)), f64::from(sequence.1.max(1)));
    (sw / w).min(sh / h)
}

/// A number for XML: up to 4 decimals, no trailing zeros, no `-0`.
fn num(value: f64) -> String {
    let text = format!("{:.4}", value);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    match text {
        "-0" | "" => "0".into(),
        text => text.into(),
    }
}

/// A minimal XML element tree.
struct El {
    name: &'static str,
    attrs: Vec<(&'static str, String)>,
    text: Option<String>,
    children: Vec<El>,
}

fn el(name: &'static str) -> El {
    El {
        name,
        attrs: Vec::new(),
        text: None,
        children: Vec::new(),
    }
}

fn leaf(name: &'static str, text: impl ToString) -> El {
    El {
        text: Some(text.to_string()),
        ..el(name)
    }
}

fn flag(name: &'static str, on: bool) -> El {
    leaf(name, if on { "TRUE" } else { "FALSE" })
}

impl El {
    fn attr(mut self, name: &'static str, value: impl ToString) -> Self {
        self.attrs.push((name, value.to_string()));
        self
    }

    fn child(mut self, child: El) -> Self {
        self.children.push(child);
        self
    }

    fn children(mut self, children: impl IntoIterator<Item = El>) -> Self {
        self.children.extend(children);
        self
    }

    fn render(&self, out: &mut String, depth: usize) {
        let pad = "    ".repeat(depth);
        out.push_str(&pad);
        out.push('<');
        out.push_str(self.name);
        for (name, value) in &self.attrs {
            out.push_str(&format!(" {name}=\"{}\"", escape(value)));
        }
        match (&self.text, self.children.is_empty()) {
            (Some(text), _) => {
                out.push_str(&format!(">{}</{}>\n", escape(text), self.name));
            }
            (None, true) => out.push_str("/>\n"),
            (None, false) => {
                out.push_str(">\n");
                for child in &self.children {
                    child.render(out, depth + 1);
                }
                out.push_str(&format!("{pad}</{}>\n", self.name));
            }
        }
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 has no other control characters.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// `path` percent-encoded for a URL (every byte but unreserved ones and
/// `/`).
fn url_path(path: &Path) -> String {
    let mut out = String::new();
    for &byte in path.as_os_str().as_encoded_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            byte => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

const XML_HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n";

/// Marks a project file as one of these exports (near its start), so `path`
/// replaces only its own files unless told to replace (`replace`).
/// The mark that makes a project file ours (replaceable without `replace`).
/// Still "timeline export", as 0.0.8 wrote it, so its exports count too.
const MARK: &str = "Isocast timeline export";
const MARK_COMMENT: &str = "<!-- Isocast timeline export -->\n";

/// FCPXML 1.10 (see the module docs for the layout).
fn fcpxml(model: &Model) -> String {
    let rate = model.rate;
    let mut resources: Vec<El> = Vec::new();
    // Formats: the sequence's (r1), then one per kind of media file.
    let mut formats: Vec<((u32, u32, Rate), String)> = Vec::new();
    let mut format_of = |resources: &mut Vec<El>, size: (u32, u32), r: Rate| -> String {
        if let Some((_, id)) = formats.iter().find(|(k, _)| *k == (size.0, size.1, r)) {
            return id.clone();
        }
        let id = format!("r{}", formats.len() + 1);
        resources.push(
            el("format")
                .attr("id", &id)
                .attr("frameDuration", r.seconds(1))
                .attr("width", size.0)
                .attr("height", size.1)
                .attr("colorSpace", "1-1-1 (Rec. 709)"),
        );
        formats.push(((size.0, size.1, r), id.clone()));
        id
    };
    let sequence_format = format_of(&mut resources, (model.width, model.height), rate);
    let mut assets: Vec<String> = Vec::new();
    for media in &model.media {
        let format = format_of(&mut resources, (media.width, media.height), media.rate);
        let id = format!("a{}", assets.len() + 1);
        let mut asset = el("asset")
            .attr("id", &id)
            .attr("name", stem(&media.name))
            .attr("start", "0s")
            .attr("duration", media.rate.seconds(media.frames as i64))
            .attr("hasVideo", "1")
            .attr("format", format)
            .attr("videoSources", "1");
        if !media.audio.is_empty() {
            asset = asset
                .attr("hasAudio", "1")
                .attr("audioSources", media.audio.len())
                .attr(
                    "audioChannels",
                    media.audio.iter().map(|(_, ch)| *ch).max().unwrap_or(2),
                )
                .attr("audioRate", SAMPLE_RATE);
        }
        asset = asset.child(
            el("media-rep")
                .attr("kind", "original-media")
                .attr("src", format!("file://{}", url_path(&media.path))),
        );
        resources.push(asset);
        assets.push(id);
    }
    // Lanes: ISO video 1…n, the program n+1; audio below, the program's
    // first. A file's audio tracks travel together in one clip.
    let mut audio_sources: Vec<String> = Vec::new();
    for track in &model.audio {
        if !audio_sources.contains(&track.source) {
            audio_sources.push(track.source.clone());
        }
    }
    let mut spine = el("spine");
    for (index, span) in model.clips.iter().enumerate() {
        let mut gap = el("gap")
            .attr("name", &span.id)
            .attr("offset", rate.seconds(span.start as i64))
            .attr("start", "0s")
            .attr("duration", rate.seconds(span.length as i64));
        let local = |item: &Item| rate.seconds((item.start - span.start) as i64);
        for (lane, track) in model.video.iter().enumerate() {
            for item in track.items.iter().filter(|i| i.clip == index) {
                let media = &model.media[item.media];
                let mut clip = el("asset-clip")
                    .attr("ref", &assets[item.media])
                    .attr("lane", lane + 1)
                    .attr("offset", local(item))
                    .attr("name", format!("{} {}", span.id, track.source))
                    .attr("start", media.rate.seconds(item.source_in as i64))
                    .attr("duration", rate.seconds(item.length as i64))
                    .attr("srcEnable", "video")
                    .attr("tcFormat", "NDF");
                if !item.enabled {
                    clip = clip.attr("enabled", "0");
                }
                gap = gap.child(clip);
            }
        }
        for (n, source) in audio_sources.iter().enumerate() {
            let Some(track) = model.audio.iter().find(|t| &t.source == source) else {
                continue;
            };
            for item in track.items.iter().filter(|i| i.clip == index) {
                let media = &model.media[item.media];
                let mut clip = el("asset-clip")
                    .attr("ref", &assets[item.media])
                    .attr("lane", -(n as i64) - 1)
                    .attr("offset", local(item))
                    .attr("name", format!("{} {} audio", span.id, source))
                    .attr("start", media.rate.seconds(item.source_in as i64))
                    .attr("duration", rate.seconds(item.length as i64))
                    .attr("srcEnable", "audio")
                    .attr("tcFormat", "NDF");
                if !item.enabled {
                    clip = clip.attr("enabled", "0");
                }
                gap = gap.child(clip);
            }
        }
        for (at, label) in model
            .markers
            .iter()
            .filter(|(at, _)| *at >= span.start && *at < span.start + span.length)
        {
            gap = gap.child(
                el("marker")
                    .attr("start", rate.seconds((at - span.start) as i64))
                    .attr("duration", rate.seconds(1))
                    .attr("value", label),
            );
        }
        spine = spine.child(gap);
    }
    let sequence = el("sequence")
        .attr("format", sequence_format)
        .attr("duration", rate.seconds(model.duration as i64))
        .attr("tcStart", "0s")
        .attr("tcFormat", "NDF")
        .attr("audioLayout", "stereo")
        .attr("audioRate", "48k")
        .child(spine);
    let root = el("fcpxml")
        .attr("version", "1.10")
        .child(el("resources").children(resources))
        .child(
            el("library").child(
                el("event")
                    .attr("name", "Isocast")
                    .child(el("project").attr("name", &model.name).child(sequence)),
            ),
        );
    let mut out = String::from(XML_HEAD);
    out.push_str("<!DOCTYPE fcpxml>\n");
    out.push_str(MARK_COMMENT);
    root.render(&mut out, 0);
    out
}

/// A file name without its extension.
fn stem(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .map_or_else(|| name.to_string(), |s| s.to_string_lossy().into())
}

fn xmeml_rate(rate: Rate) -> El {
    el("rate")
        .child(leaf("timebase", rate.timebase()))
        .child(flag("ntsc", rate.ntsc()))
}

fn xmeml_timecode(rate: Rate) -> El {
    el("timecode")
        .child(xmeml_rate(rate))
        .child(leaf("string", timecode(0, rate)))
        .child(leaf("frame", 0))
        .child(leaf("displayformat", "NDF"))
}

/// A picture of another size than the sequence in xmeml: Basic Motion's
/// scale (in percent). Premiere counts the scale in the picture's own
/// pixels and shows a picture of another size at them, so a picture that
/// needs fitting gets its fit as a scale; Resolve fits it first by itself
/// (measured: Resolve 21.1 rendered a 1920x1080 angle in a 1280x720
/// sequence at two thirds of the frame with Premiere's 66.7%), so it gets
/// none.
fn xmeml_fit(fit: f64, app: NleApp) -> Option<El> {
    if app == NleApp::Resolve || (fit - 1.0).abs() <= 1e-6 {
        return None;
    }
    let param = |id: &'static str, name: &'static str, min: &str, max: &str, value: f64| {
        el("parameter")
            .child(leaf("parameterid", id))
            .child(leaf("name", name))
            .child(leaf("valuemin", min))
            .child(leaf("valuemax", max))
            .child(leaf("value", num(value)))
    };
    Some(
        el("filter").child(
            el("effect")
                .child(leaf("name", "Basic Motion"))
                .child(leaf("effectid", "basic"))
                .child(leaf("effectcategory", "motion"))
                .child(leaf("effecttype", "motion"))
                .child(leaf("mediatype", "video"))
                .child(param("scale", "Scale", "0", "1000", fit * 100.0))
                .child(
                    el("parameter")
                        .child(leaf("parameterid", "center"))
                        .child(leaf("name", "Center"))
                        .child(
                            el("value")
                                .child(leaf("horiz", num(0.0)))
                                .child(leaf("vert", num(0.0))),
                        ),
                ),
        ),
    )
}

/// A linked clip item in xmeml: is audio, track number, place on its track
/// (from 1).
type Linked = (bool, usize, usize);

/// Final Cut Pro 7 XML (xmeml 5), as Premiere Pro and Resolve read it.
/// Audio: for Premiere each audio track of a file is one clip on one track
/// (stereo); Resolve reads a clip's `trackindex` as one channel of the file
/// (it made a stereo clip a mono track of its left channel), so for it
/// each channel gets its own track, a stereo pair panned left and right.
fn xmeml(model: &Model, app: NleApp) -> String {
    let rate = model.rate;
    // The audio tracks written: a model track, and for Resolve which of its
    // channels (and how many it has).
    let channels_of = |track: &Track| -> u32 {
        track
            .items
            .first()
            .and_then(|i| {
                let stream = track.audio.as_ref().map_or(0, |(n, _)| *n);
                model.media[i.media].audio.get(stream).map(|(_, ch)| *ch)
            })
            .unwrap_or(2)
    };
    let mut emitted: Vec<(usize, Option<(u32, u32)>)> = Vec::new();
    for (t, track) in model.audio.iter().enumerate() {
        match app {
            NleApp::Resolve => {
                let channels = channels_of(track);
                emitted.extend((0..channels).map(|k| (t, Some((k, channels)))));
            }
            _ => emitted.push((t, None)),
        }
    }
    let mut written = vec![false; model.media.len()];
    let file = |written: &mut Vec<bool>, index: usize| -> El {
        let id = format!("file-{}", index + 1);
        if std::mem::replace(&mut written[index], true) {
            return el("file").attr("id", id);
        }
        let media = &model.media[index];
        let mut tracks = vec![
            el("video").child(
                el("samplecharacteristics")
                    .child(xmeml_rate(media.rate))
                    .child(leaf("width", media.width))
                    .child(leaf("height", media.height))
                    .child(flag("anamorphic", false))
                    .child(leaf("pixelaspectratio", "square"))
                    .child(leaf("fielddominance", "none")),
            ),
        ];
        for (_, channels) in &media.audio {
            tracks.push(
                el("audio")
                    .child(
                        el("samplecharacteristics")
                            .child(leaf("depth", 16))
                            .child(leaf("samplerate", SAMPLE_RATE)),
                    )
                    .child(leaf("channelcount", channels)),
            );
        }
        el("file")
            .attr("id", id)
            .child(leaf("name", &media.name))
            .child(leaf(
                "pathurl",
                format!("file://localhost{}", url_path(&media.path)),
            ))
            .child(xmeml_rate(media.rate))
            .child(leaf("duration", media.frames))
            .child(xmeml_timecode(media.rate))
            .child(el("media").children(tracks))
    };
    // Each clip's items from one file are linked: (clip, media) → every
    // item's (is audio, track number, place on its track), from 1.
    let mut groups: BTreeMap<(usize, usize), Vec<Linked>> = BTreeMap::new();
    for (t, track) in model.video.iter().enumerate() {
        for (i, item) in track.items.iter().enumerate() {
            groups
                .entry((item.clip, item.media))
                .or_default()
                .push((false, t + 1, i + 1));
        }
    }
    for (n, (t, _)) in emitted.iter().enumerate() {
        for (i, item) in model.audio[*t].items.iter().enumerate() {
            groups
                .entry((item.clip, item.media))
                .or_default()
                .push((true, n + 1, i + 1));
        }
    }
    let id_of = |audio: bool, track: usize, place: usize| {
        format!("clipitem-{}{track}-{place}", if audio { "a" } else { "v" })
    };
    let clipitem = |written: &mut Vec<bool>,
                    audio: bool,
                    t: usize,
                    i: usize,
                    track: &Track,
                    channel: Option<(u32, u32)>| {
        let item = &track.items[i];
        let media = &model.media[item.media];
        let source_frames = rate.convert(item.length, media.rate);
        let mut clip = el("clipitem")
            .attr("id", id_of(audio, t + 1, i + 1))
            .child(leaf(
                "masterclipid",
                format!("masterclip-{}", item.media + 1),
            ))
            .child(leaf("name", &media.name))
            .child(flag("enabled", item.enabled))
            .child(leaf("duration", media.frames))
            .child(xmeml_rate(media.rate))
            .child(leaf("start", item.start))
            .child(leaf("end", item.start + item.length))
            .child(leaf("in", item.source_in))
            .child(leaf("out", item.source_in + source_frames))
            .child(file(written, item.media));
        let stream = track.audio.as_ref().map_or(0, |(n, _)| *n);
        // Resolve: the channel's number among all the file's channels.
        let index = match channel {
            Some((k, _)) => {
                media.audio[..stream.min(media.audio.len())]
                    .iter()
                    .map(|(_, ch)| *ch as usize)
                    .sum::<usize>()
                    + k as usize
            }
            None => stream,
        };
        clip = clip.child(
            el("sourcetrack")
                .child(leaf("mediatype", if audio { "audio" } else { "video" }))
                .child(leaf("trackindex", index + 1)),
        );
        if audio {
            match channel {
                None => {
                    let channels = media.audio.get(stream).map_or(2, |(_, ch)| *ch);
                    clip = clip.attr(
                        "premiereChannelType",
                        if channels == 1 { "mono" } else { "stereo" },
                    );
                }
                Some((k, 2)) => {
                    clip = clip.child(
                        el("filter").child(
                            el("effect")
                                .child(leaf("name", "Audio Pan"))
                                .child(leaf("effectid", "audiopan"))
                                .child(leaf("effectcategory", "audiopan"))
                                .child(leaf("effecttype", "audiopan"))
                                .child(leaf("mediatype", "audio"))
                                .child(
                                    el("parameter")
                                        .child(leaf("parameterid", "pan"))
                                        .child(leaf("name", "pan"))
                                        .child(leaf("valuemin", -1))
                                        .child(leaf("valuemax", 1))
                                        .child(leaf("value", if k == 0 { -1 } else { 1 })),
                                ),
                        ),
                    );
                }
                Some(_) => {}
            }
        } else {
            let fit = fit((media.width, media.height), (model.width, model.height));
            clip = clip.children(xmeml_fit(fit, app));
        }
        for &(is_audio, track, place) in &groups[&(item.clip, item.media)] {
            let mut link = el("link")
                .child(leaf("linkclipref", id_of(is_audio, track, place)))
                .child(leaf("mediatype", if is_audio { "audio" } else { "video" }))
                .child(leaf("trackindex", track))
                .child(leaf("clipindex", place));
            if is_audio {
                link = link.child(leaf("groupindex", 1));
            }
            clip = clip.child(link);
        }
        clip
    };
    let mut video = el("video").child(
        el("format").child(
            el("samplecharacteristics")
                .child(xmeml_rate(rate))
                .child(leaf("width", model.width))
                .child(leaf("height", model.height))
                .child(flag("anamorphic", false))
                .child(leaf("pixelaspectratio", "square"))
                .child(leaf("fielddominance", "none")),
        ),
    );
    for (t, track) in model.video.iter().enumerate() {
        let mut xml = el("track");
        for i in 0..track.items.len() {
            xml = xml.child(clipitem(&mut written, false, t, i, track, None));
        }
        video = video.child(
            xml.child(flag("enabled", true))
                .child(flag("locked", false)),
        );
    }
    let mut audio = el("audio").child(leaf("numOutputChannels", 2)).child(
        el("format").child(
            el("samplecharacteristics")
                .child(leaf("depth", 16))
                .child(leaf("samplerate", SAMPLE_RATE)),
        ),
    );
    for (n, &(t, channel)) in emitted.iter().enumerate() {
        let track = &model.audio[t];
        let kind = match channel {
            Some(_) => "Mono",
            None if channels_of(track) == 1 => "Mono",
            None => "Stereo",
        };
        let mut xml = el("track").attr("premiereTrackType", kind);
        for i in 0..track.items.len() {
            xml = xml.child(clipitem(&mut written, true, n, i, track, channel));
        }
        audio = audio.child(
            xml.child(flag("enabled", true))
                .child(flag("locked", false)),
        );
    }
    let mut sequence = el("sequence")
        .attr("id", "sequence-1")
        .child(leaf("name", &model.name))
        .child(leaf("duration", model.duration))
        .child(xmeml_rate(rate))
        .child(xmeml_timecode(rate))
        .child(el("media").child(video).child(audio));
    for (at, label) in &model.markers {
        sequence = sequence.child(
            el("marker")
                .child(leaf("comment", ""))
                .child(leaf("name", label))
                .child(leaf("in", at))
                .child(leaf("out", -1)),
        );
    }
    let mut out = String::from(XML_HEAD);
    out.push_str("<!DOCTYPE xmeml>\n");
    out.push_str(MARK_COMMENT);
    el("xmeml")
        .attr("version", "5")
        .child(sequence)
        .render(&mut out, 0);
    out
}

fn otio_time(value: u64, rate: Rate) -> Value {
    json!({"OTIO_SCHEMA": "RationalTime.1", "rate": rate.fps(), "value": value as f64})
}

fn otio_range(start: u64, duration: u64, rate: Rate) -> Value {
    json!({
        "OTIO_SCHEMA": "TimeRange.1",
        "start_time": otio_time(start, rate),
        "duration": otio_time(duration, rate),
    })
}

/// OpenTimelineIO (JSON). Media paths are absolute, as plain paths: Resolve
/// (21.1) imported no clip whose `target_url` was relative to the file or a
/// `file://` URL, and writes plain paths itself. OpenTimelineIO has no
/// audio track selection: which audio track of the file a clip is is kept
/// in each clip's `metadata.isocast`.
fn otio(model: &Model) -> String {
    let rate = model.rate;
    let url = |path: &Path| path.to_string_lossy().into_owned();
    let track = |track: &Track, kind: &str| {
        let mut children = Vec::new();
        let mut at = 0u64;
        for item in &track.items {
            if item.start > at {
                children.push(json!({
                    "OTIO_SCHEMA": "Gap.1", "metadata": {}, "name": "", "effects": [],
                    "markers": [], "enabled": true,
                    "source_range": otio_range(0, item.start - at, rate),
                }));
            }
            let media = &model.media[item.media];
            let mut meta = json!({
                "clip": model.clips[item.clip].id,
                "source": track.source,
            });
            if let Some((n, audio)) = &track.audio {
                meta["audio_track"] = json!(n);
                meta["audio"] = json!(audio);
            }
            children.push(json!({
                "OTIO_SCHEMA": "Clip.2",
                "metadata": {"isocast": meta},
                "name": format!("{} {}", model.clips[item.clip].id, track.name()),
                "source_range": {
                    "OTIO_SCHEMA": "TimeRange.1",
                    "start_time": otio_time(item.source_in, media.rate),
                    "duration": otio_time(rate.convert(item.length, media.rate), media.rate),
                },
                "effects": [], "markers": [], "enabled": item.enabled,
                "media_references": {"DEFAULT_MEDIA": {
                    "OTIO_SCHEMA": "ExternalReference.1",
                    "metadata": {},
                    "name": media.name,
                    "available_range": otio_range(0, media.frames, media.rate),
                    "available_image_bounds": null,
                    "target_url": url(&media.path),
                }},
                "active_media_reference_key": "DEFAULT_MEDIA",
            }));
            at = item.start + item.length;
        }
        json!({
            "OTIO_SCHEMA": "Track.1", "metadata": {}, "name": track.name(),
            "source_range": null, "effects": [], "markers": [], "enabled": true,
            "kind": kind, "children": children,
        })
    };
    let mut tracks: Vec<Value> = model.video.iter().map(|t| track(t, "Video")).collect();
    tracks.extend(model.audio.iter().map(|t| track(t, "Audio")));
    let markers: Vec<Value> = model
        .markers
        .iter()
        .map(|(at, label)| {
            json!({
                "OTIO_SCHEMA": "Marker.2", "metadata": {}, "name": label, "color": "RED",
                "comment": "", "marked_range": otio_range(*at, 0, rate),
            })
        })
        .collect();
    let timeline = json!({
        "OTIO_SCHEMA": "Timeline.1",
        "metadata": {"isocast": {"export": MARK, "width": model.width, "height": model.height}},
        "name": model.name,
        "global_start_time": otio_time(0, rate),
        "tracks": {
            "OTIO_SCHEMA": "Stack.1", "metadata": {}, "name": "tracks",
            "source_range": null, "effects": [], "markers": markers, "enabled": true,
            "children": tracks,
        },
    });
    serde_json::to_string_pretty(&timeline).unwrap_or_default() + "\n"
}

/// The project file for `model`.
fn render(model: &Model, format: NleFormat, app: NleApp) -> String {
    match format {
        NleFormat::Fcpxml => fcpxml(model),
        NleFormat::Xmeml => xmeml(model, app),
        NleFormat::Otio => otio(model),
    }
}

/// Whether the file at `path` is one of these exports (its marker near the
/// start).
fn ours(path: &Path) -> bool {
    use std::io::Read;
    let mut head = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(4096).read_to_end(&mut head))
        .is_ok()
        && String::from_utf8_lossy(&head).contains(MARK)
}

/// Writes `text` to `path` atomically (a new partial file, then renamed),
/// so a crash leaves the previous file or none, never half of one. The
/// partial file's name is unique and it is made new, so nothing already
/// there (a file, a symlink) is written through.
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut partial = path.as_os_str().to_owned();
    partial.push(format!(".{}-{nanos}.partial", std::process::id()));
    let partial = PathBuf::from(partial);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)
        .and_then(|mut file| {
            let result = file
                .write_all(text.as_bytes())
                .and_then(|()| file.sync_all());
            if result.is_err() {
                let _ = std::fs::remove_file(&partial);
            }
            result
        });
    written
        .and_then(|()| {
            isocast_protocol::fs::replace(&partial, path).inspect_err(|_| {
                let _ = std::fs::remove_file(&partial);
            })
        })
        .map_err(|err| format!("could not write {}: {err}", path.display()))
}

/// The take `path` names: a manifest, or a project folder holding exactly
/// one.
fn take_manifest(path: &Path) -> Result<PathBuf, RpcError> {
    if !path.is_dir() {
        return Ok(path.to_path_buf());
    }
    let mut manifests: Vec<PathBuf> = std::fs::read_dir(path)
        .map_err(|err| failed(format!("take: could not read {}: {err}", path.display())))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|p| p.to_string_lossy().ends_with(".session.json"))
        .collect();
    manifests.sort();
    match manifests.len() {
        1 => Ok(manifests.remove(0)),
        0 => Err(invalid(format!(
            "take: {} holds no take; give the take's `<name>.session.json`",
            path.display()
        ))),
        n => Err(invalid(format!(
            "take: {} holds {n} takes; give one take's `<name>.session.json`: {}",
            path.display(),
            manifests
                .iter()
                .filter_map(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

impl Daemon {
    /// See [`Method::TakeExportNle`](isocast_protocol::api::Method::TakeExportNle).
    pub(crate) fn take_export_nle(
        &self,
        params: TakeExportNleParams,
    ) -> Result<NleExported, RpcError> {
        check_folder(params.folder.as_deref())?;
        let format = params.format;
        if cfg!(windows) && matches!(format, NleFormat::Fcpxml | NleFormat::Xmeml) {
            return Err(invalid(
                "format: XML media URLs are not supported on Windows yet; use otio",
            ));
        }
        let app = params.app.unwrap_or(match format {
            NleFormat::Fcpxml => NleApp::FinalCut,
            NleFormat::Xmeml => NleApp::Premiere,
            NleFormat::Otio => NleApp::Resolve,
        });
        if params.folder.is_some() && params.path.is_some() {
            return Err(invalid(
                "folder, path: give one: folder (a numbered name in it) or path (exactly that \
                 file)",
            ));
        }
        if let Some(path) = &params.path {
            let extension = path.extension().map(|e| e.to_string_lossy().to_lowercase());
            if !path.is_absolute() {
                return Err(invalid(format!(
                    "path: {:?} isn't an absolute path, and isocastd doesn't run in your \
                     folder; give an absolute path",
                    path.display().to_string()
                )));
            }
            if extension.as_deref() != Some(format.extension()) {
                return Err(invalid(format!(
                    "path: {} must end in .{} for format {}",
                    path.display(),
                    format.extension(),
                    format.name()
                )));
            }
            if path.is_dir() {
                return Err(invalid(format!("path: {} is a folder", path.display())));
            }
        }
        // A network caller's `take` was checked against the folder roots
        // (`check_params`).
        let manifest_path = take_manifest(&params.take)?;
        if !manifest_path.is_absolute() {
            return Err(invalid(format!(
                "take: {:?} isn't an absolute path; give the take's manifest (`….session.json`, \
                 as `start`, `stop` and `take_finished` give it)",
                manifest_path.display().to_string()
            )));
        }
        let manifest: SessionManifest = std::fs::read(&manifest_path)
            .map_err(|err| {
                failed(format!(
                    "take: could not read {}: {err}",
                    manifest_path.display()
                ))
            })
            .and_then(|bytes| {
                serde_json::from_slice(&bytes).map_err(|err| {
                    invalid(format!(
                        "take: {} isn't a take's manifest (`….session.json`): {err}",
                        manifest_path.display()
                    ))
                })
            })?;
        if manifest.status == SessionStatus::Recording {
            return Err(RpcError {
                code: RpcError::WRONG_STATE,
                message: format!(
                    "take: {} is still being recorded (or never finished: isocastd stopped \
                     while it recorded); export it once it has finished",
                    manifest_path.display()
                ),
            });
        }
        let Some((frames, fps)) = take_length(&manifest).filter(|(frames, _)| *frames > 0) else {
            return Err(failed(format!(
                "take: {} has no finished file to export",
                manifest_path.display()
            )));
        };
        let name = manifest_path
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
            .trim_end_matches(".session.json")
            .to_string();
        let take = TakeClip {
            id: name.clone(),
            folder: manifest_path
                .parent()
                .unwrap_or(Path::new("/"))
                .to_path_buf(),
            manifest,
            frames,
            fps,
        };
        let model = build(&name, std::slice::from_ref(&take)).map_err(failed)?;
        let text = render(&model, format, app);
        let path = match &params.path {
            Some(path) => {
                if let Some(parent) = path.parent().filter(|p| !p.is_dir()) {
                    return Err(failed(format!(
                        "path: the folder {} doesn't exist",
                        parent.display()
                    )));
                }
                // Never a file the project points at (their extensions
                // differ, but a take's file could be named anything).
                if model.media.iter().any(|m| m.path == *path) {
                    return Err(invalid(format!(
                        "path: {} is one of the take's files",
                        path.display()
                    )));
                }
                if path.symlink_metadata().is_ok() && !params.replace && !ours(path) {
                    return Err(failed(format!(
                        "path: {} is there and isn't an Isocast export; give replace: \
                         true (`--replace`) to replace it, or another path",
                        path.display()
                    )));
                }
                write_atomic(path, &text).map_err(failed)?;
                path.clone()
            }
            None => {
                let folder = params.folder.clone().unwrap_or_else(|| take.folder.clone());
                // As for exports: the default folder is checked too.
                let folder = self.checked_path(
                    "folder",
                    &folder.to_string_lossy(),
                    crate::roots::Access::Write,
                )?;
                std::fs::create_dir_all(&folder).map_err(|err| {
                    failed(format!(
                        "folder: could not make {}: {err}",
                        folder.display()
                    ))
                })?;
                let label = match format {
                    NleFormat::Otio => String::new(),
                    _ => format!(" {}", app.title()),
                };
                // Take the first free numbered name by making it (empty),
                // so two exports at once never pick the same one.
                let mut reserved = None;
                for n in 1..10_000 {
                    let number = match n {
                        1 => String::new(),
                        n => format!(" {n}"),
                    };
                    let path = folder.join(format!("{name}{label}{number}.{}", format.extension()));
                    match std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                    {
                        Ok(_) => {
                            reserved = Some(path);
                            break;
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(err) => {
                            return Err(failed(format!(
                                "folder: could not write {}: {err}",
                                path.display()
                            )));
                        }
                    }
                }
                let path = reserved.ok_or_else(|| {
                    failed(format!(
                        "folder: no free name for the project in {}",
                        folder.display()
                    ))
                })?;
                if let Err(problem) = write_atomic(&path, &text) {
                    // Only the empty file just made for it.
                    let _ = std::fs::remove_file(&path);
                    return Err(failed(problem));
                }
                path
            }
        };
        let mut notes = model.notes.clone();
        if format == NleFormat::Fcpxml && app == NleApp::Resolve {
            notes.push(
                "Resolve reads an FCPXML clip's picture and sound together (it ignores \
                 srcEnable), so the program's sound doubles; export xmeml for Resolve \
                 (`--format xmeml --app resolve`)"
                    .into(),
            );
        }
        if format == NleFormat::Fcpxml
            && model.audio.iter().any(|t| {
                model
                    .media
                    .iter()
                    .any(|m| m.source == t.source && m.audio.len() > 1)
            })
        {
            notes.push(
                "FCPXML has no tracks: each file's audio tracks travel together in one clip on \
                 their own lane"
                    .into(),
            );
        }
        let tracks = |tracks: &[Track]| -> Vec<NleTrack> {
            tracks
                .iter()
                .map(|t| NleTrack {
                    name: t.name(),
                    source: t.source.clone(),
                    audio: t.audio.as_ref().map(|(_, a)| a.clone()),
                    clips: t.items.len(),
                    enabled: t.items.iter().filter(|i| i.enabled).count(),
                })
                .collect()
        };
        Ok(NleExported {
            take: manifest_path,
            format,
            app,
            path,
            duration_seconds: model.duration as f64 / model.rate.fps(),
            frame_rate: model.rate.text(),
            width: model.width,
            height: model.height,
            video_tracks: tracks(&model.video),
            audio_tracks: tracks(&model.audio),
            media: model.media.len(),
            markers: model.markers.len(),
            notes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_and_times() {
        let ntsc = Rate {
            num: 60000,
            den: 1001,
        };
        assert_eq!(ntsc.seconds(1), "1001/60000s");
        assert_eq!(ntsc.seconds(60), "1001/1000s");
        assert_eq!(
            (ntsc.timebase(), ntsc.ntsc(), ntsc.text()),
            (60, true, "60000/1001".into())
        );
        assert_eq!(Rate::whole(60).seconds(90), "3/2s");
        assert_eq!(Rate::whole(30).seconds(0), "0s");
        assert_eq!(Rate::whole(30).convert(15, Rate::whole(60)), 30);
        assert_eq!(
            timecode(3 * 3600 * 30 + 61 * 30 + 7, Rate::whole(30)),
            "03:01:01:07"
        );
    }

    #[test]
    fn pictures_of_another_size_are_fitted() {
        // A 4K picture in 1080p: half size, which Premiere is told and
        // Resolve works out by itself.
        let half = fit((3840, 2160), (1920, 1080));
        assert!((half - 0.5).abs() < 1e-9);
        assert!(xmeml_fit(half, NleApp::Premiere).is_some());
        assert!(xmeml_fit(half, NleApp::Resolve).is_none());
        assert!(xmeml_fit(fit((1920, 1080), (1920, 1080)), NleApp::Premiere).is_none());
        assert_eq!(url_path(Path::new("/a b/é.mov")), "/a%20b/%C3%A9.mov");
    }

    #[test]
    fn an_iso_that_dropped_frames_still_covers_the_take() {
        // A 5 s take at 30 fps whose ISO stored 122 of its 150 frame
        // slots (28 dropped, as on a loaded hosted runner): the file still
        // spans 5 s, so its angle must cover the whole clip like the
        // program's, not end 28 frames early.
        let file = |path: &str, source: &str, frames: u64, dropped: u64| {
            json!({
                "path": path, "source": source, "kind": "test_pattern",
                "width": 1280, "height": 720, "fps": 30, "audio": [],
                "start_offset_seconds": 0.0, "duration_seconds": 5.0,
                "frames": frames, "repeated_frames": 0, "dropped_frames": dropped,
                "audio_dropouts": 0
            })
        };
        let manifest: SessionManifest = serde_json::from_value(json!({
            "version": 1, "setup": "e2e", "status": "complete",
            "started_at": "2026-10-22T12:00:00+00:00", "host_time_ns": 0,
            "duration_seconds": 5.0,
            "files": [file("iso.mov", "bars", 122, 28), file("program.mov", PROGRAM, 150, 0)],
            "errors": []
        }))
        .unwrap();
        let take = TakeClip {
            id: "t1".into(),
            folder: PathBuf::from("/take"),
            manifest,
            frames: 150,
            fps: 30,
        };
        let model = build("e2e", &[take]).unwrap();
        assert_eq!(model.duration, 150);
        for track in &model.video {
            let covered: u64 = track.items.iter().map(|i| i.length).sum();
            assert_eq!(
                covered, 150,
                "{} covers {covered} of 150 frames",
                track.source
            );
        }
        let iso = model.media.iter().find(|m| m.source == "bars").unwrap();
        assert_eq!(iso.frames, 150, "the ISO's length on its frame grid");
    }

    #[test]
    fn a_recovered_file_without_counters_keeps_its_stored_frames() {
        // A file recovered after a crash has a duration and its stored frame
        // count but no dropped_frames: its length is the frames it holds
        // (100), not its duration on the frame grid (150), so its track ends
        // where its pictures do.
        let manifest: SessionManifest = serde_json::from_value(json!({
            "version": 1, "setup": "e2e", "status": "complete",
            "started_at": "2026-10-22T12:00:00+00:00", "host_time_ns": 0,
            "duration_seconds": 5.0,
            "files": [
                {"path": "iso.mov", "source": "bars", "kind": "test_pattern", "width": 1280,
                 "height": 720, "fps": 30, "audio": [], "start_offset_seconds": 0.0,
                 "duration_seconds": 5.0, "frames": 100},
                {"path": "program.mov", "source": PROGRAM, "kind": "program", "width": 1280,
                 "height": 720, "fps": 30, "audio": [], "start_offset_seconds": 0.0,
                 "duration_seconds": 5.0, "frames": 150, "repeated_frames": 0,
                 "dropped_frames": 0, "audio_dropouts": 0}
            ],
            "errors": []
        }))
        .unwrap();
        let model = build(
            "e2e",
            &[TakeClip {
                id: "t1".into(),
                folder: PathBuf::from("/take"),
                manifest,
                frames: 150,
                fps: 30,
            }],
        )
        .unwrap();
        let iso = model.media.iter().find(|m| m.source == "bars").unwrap();
        assert_eq!(iso.frames, 100, "the recovered file's stored frames");
        let bars = model.video.iter().find(|t| t.source == "bars").unwrap();
        let covered: u64 = bars.items.iter().map(|i| i.length).sum();
        assert_eq!(covered, 100, "its track ends with its last stored frame");
        let program = model.media.iter().find(|m| m.source == PROGRAM).unwrap();
        assert_eq!(program.frames, 150);
    }
}
