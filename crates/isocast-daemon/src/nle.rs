//! `timeline_export_nle` (#104): a timeline as a project Final Cut Pro
//! (FCPXML 1.10), Premiere Pro (xmeml 5, Final Cut Pro 7 XML) or DaVinci
//! Resolve (either, or OpenTimelineIO) opens, pointing at the takes' own
//! program and ISO files: nothing is encoded, copied or changed, and the
//! only file written is the project file.
//!
//! Layout, the same in every format: one video track per ISO source (in the
//! order the takes list them) with the program's on top, each holding
//! every clip's stretch of that source's file, so all angles stay lined up
//! and a cut in another angle is a click away. At each clip only its angle's
//! track is on (the others are there, switched off), so the sequence plays
//! what Isocast's edited file shows; a crop is a crop and a transform on
//! the clip's items (all of its angles, as a crop belongs to the clip). The
//! program's audio tracks are on and each ISO file's are there but off, as
//! the edited file has the program's sound. Each clip's video and audio
//! from one file are linked.
//!
//! Why stacked tracks and not FCPXML's multicam clip: the same layout works
//! in all three formats and in Resolve, which reads stacked lanes reliably
//! but multicam clips only partly; and an angle switch stays a visible cut
//! on its own track. (In FCPXML the lanes hang off one gap per clip: the
//! ISO angles on lanes 1…n, the program above them, the audio below.)
//!
//! Timing comes from the take manifests: each file's `start_offset_seconds`
//! places its first frame on the take's clock, so a clip's in point maps to
//! the frame of each file that is on screen then (as Isocast's export picks
//! it). The sequence runs at the first clip's program frame rate; frames
//! are whole frames, so a file that started part of a frame after the
//! take's zero is lined up to the frame, like the export.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use isocast_protocol::PROGRAM;
use isocast_protocol::api::RpcError;
use isocast_protocol::manifest::SessionManifest;
use isocast_protocol::timeline::{
    Crop, NleApp, NleExported, NleFormat, NleTrack, TimelineExportNleParams, take_length,
};
use serde_json::{Value, json};

use crate::timeline::{failed, invalid, read_manifest, unfinished, wrong_state};
use crate::{Daemon, check_folder};

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

/// One clip of the timeline, read from its take's manifest.
pub(crate) struct TakeClip {
    pub id: String,
    pub folder: PathBuf,
    pub manifest: SessionManifest,
    /// The clip's in point and length in frames of its take's grid.
    pub in_frame: u64,
    pub frames: u64,
    pub fps: u32,
    /// The source the edited video shows for it, and its crop.
    pub angle: String,
    pub crop: Option<Crop>,
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
    crop: Option<Crop>,
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

/// The timeline laid out on tracks, the same for every format.
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
    let first = takes.first().ok_or("the timeline has no clips")?;
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
        let in_seconds = take.in_frame as f64 / take_rate.fps();
        let out_seconds = in_seconds + take.frames as f64 / take_rate.fps();
        for file in &take.manifest.files {
            let path = take.folder.join(&file.path);
            let (Some(offset), Some(seconds)) = (file.start_offset_seconds, file.duration_seconds)
            else {
                note(format!(
                    "clip {}'s {} file {} never finished (its take was interrupted); it is left \
                     out",
                    take.id,
                    file.source,
                    path.display()
                ));
                continue;
            };
            if !path.exists() {
                note(format!(
                    "clip {}'s {} file {} is missing: the editor shows it offline until it is \
                     relinked",
                    take.id,
                    file.source,
                    path.display()
                ));
            }
            let file_rate = Rate::whole(file.fps);
            let file_frames = file
                .frames
                .unwrap_or_else(|| (seconds * file_rate.fps()).round().max(0.0) as u64);
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
            // The file's frame on screen at the clip's in point.
            let first_frame =
                ((in_seconds - offset) * file_rate.fps() + FRAME_SLACK).floor() as i64;
            let (lead, source_in) = match first_frame {
                k if k < 0 => {
                    note(format!(
                        "clip {}'s {} file starts {:.1} ms after the clip's in point: its first \
                         frame comes {} frame(s) into the clip (Isocast's export holds it from \
                         the clip's start)",
                        take.id,
                        file.source,
                        (offset - in_seconds) * 1000.0,
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
                    "clip {}'s {} file ends {} frame(s) before the clip does (Isocast's export \
                     holds its last frame)",
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
                enabled: file.source == take.angle,
                crop: take.crop,
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
                audio[t].items.push(Item {
                    enabled: file.source == PROGRAM,
                    crop: None,
                    ..item.clone()
                });
                audio_index[t].push(number);
            }
        }
        for marker in &take.manifest.markers {
            if marker.time_seconds >= in_seconds && marker.time_seconds < out_seconds {
                let at = start + ((marker.time_seconds - in_seconds) * rate.fps()).round() as u64;
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

/// How a clip item's picture is placed on the sequence: Isocast fits its
/// crop (the whole picture without one) into the sequence's size, centred.
#[derive(Debug, Clone, Copy)]
struct Placement {
    crop: Crop,
    /// Sequence pixels per source pixel: Isocast's, and the plain fit that
    /// editors apply to a picture of another size by themselves.
    scale: f64,
    fit: f64,
    /// Where the picture's centre goes, in sequence pixels from the
    /// sequence's centre (y down).
    x: f64,
    y: f64,
    /// The source's width over its height.
    aspect: f64,
    /// The sequence's size.
    sequence: (f64, f64),
}

fn placement(crop: Option<Crop>, source: (u32, u32), sequence: (u32, u32)) -> Placement {
    let (w, h) = (f64::from(source.0.max(1)), f64::from(source.1.max(1)));
    let (sw, sh) = (f64::from(sequence.0.max(1)), f64::from(sequence.1.max(1)));
    let fit = (sw / w).min(sh / h);
    let crop = crop.unwrap_or(Crop::FULL);
    let scale = (sw / (crop.width * w)).min(sh / (crop.height * h));
    let dx = (crop.x + crop.width / 2.0 - 0.5) * w;
    let dy = (crop.y + crop.height / 2.0 - 0.5) * h;
    Placement {
        crop,
        scale,
        fit,
        x: -dx * scale,
        y: -dy * scale,
        aspect: w / h,
        sequence: (sw, sh),
    }
}

impl Placement {
    fn cropped(&self) -> bool {
        !self.crop.is_full()
    }

    /// The picture is shown other than an editor shows it by itself
    /// (fitted, centred).
    fn moved(&self) -> bool {
        (self.scale / self.fit - 1.0).abs() > 1e-6 || self.x.abs() > 1e-3 || self.y.abs() > 1e-3
    }

    /// The edges cut off, as fractions: left, right, top, bottom.
    fn edges(&self) -> [f64; 4] {
        let c = self.crop;
        [
            c.x,
            (1.0 - c.x - c.width).max(0.0),
            c.y,
            (1.0 - c.y - c.height).max(0.0),
        ]
    }
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
                let place = placement(
                    item.crop,
                    (media.width, media.height),
                    (model.width, model.height),
                );
                clip = clip.children(fcpxml_place(&place));
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

/// A clip's crop and transform in FCPXML. Final Cut Pro fits a picture of
/// another size into the sequence by itself (scale 1), measures every edge
/// of a trim crop in percent of the picture's height (the DTD: "a
/// percentage of original frame height"; Resolve 21.1 reads it so too), and
/// a position in percent of the sequence's height from its centre, up
/// positive.
fn fcpxml_place(place: &Placement) -> Vec<El> {
    let mut out = Vec::new();
    let [left, right, top, bottom] = place.edges();
    let (_, sh) = place.sequence;
    if place.cropped() {
        out.push(
            el("adjust-crop").attr("mode", "trim").child(
                el("trim-rect")
                    .attr("left", num(left * place.aspect * 100.0))
                    .attr("top", num(top * 100.0))
                    .attr("right", num(right * place.aspect * 100.0))
                    .attr("bottom", num(bottom * 100.0)),
            ),
        );
    }
    if place.moved() {
        let scale = place.scale / place.fit;
        let (x, y) = (place.x / sh * 100.0, -place.y / sh * 100.0);
        out.push(
            el("adjust-transform")
                .attr("position", format!("{} {}", num(x), num(y)))
                .attr("scale", format!("{} {}", num(scale), num(scale))),
        );
    }
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

/// A clip's crop and placement in xmeml: the Crop filter (percent of each
/// side) and Basic Motion (centre as a fraction of the sequence's width and
/// height from its centre, down positive; scale in percent). Premiere
/// counts the scale in the picture's own pixels and shows a picture of
/// another size at them, so a picture that only needs fitting gets a scale
/// too; Resolve fits it first and counts the scale from there (measured:
/// Resolve 21.1 rendered a 1920x1080 angle in a 1280x720 sequence at two
/// thirds of the frame with Premiere's 66.7%).
fn xmeml_place(place: &Placement, app: NleApp) -> Vec<El> {
    let param = |id: &'static str, name: &'static str, min: &str, max: &str, value: f64| {
        el("parameter")
            .child(leaf("parameterid", id))
            .child(leaf("name", name))
            .child(leaf("valuemin", min))
            .child(leaf("valuemax", max))
            .child(leaf("value", num(value)))
    };
    let effect = |name: &'static str, id: &'static str, params: Vec<El>| {
        el("filter").child(
            el("effect")
                .child(leaf("name", name))
                .child(leaf("effectid", id))
                .child(leaf("effectcategory", "motion"))
                .child(leaf("effecttype", "motion"))
                .child(leaf("mediatype", "video"))
                .children(params),
        )
    };
    let mut out = Vec::new();
    let (needed, scale) = match app {
        NleApp::Resolve => (place.moved(), place.scale / place.fit),
        _ => (
            (place.scale - 1.0).abs() > 1e-6 || place.x.abs() > 1e-3 || place.y.abs() > 1e-3,
            place.scale,
        ),
    };
    if needed {
        let (sw, sh) = place.sequence;
        out.push(effect(
            "Basic Motion",
            "basic",
            vec![
                param("scale", "Scale", "0", "1000", scale * 100.0),
                el("parameter")
                    .child(leaf("parameterid", "center"))
                    .child(leaf("name", "Center"))
                    .child(
                        el("value")
                            .child(leaf("horiz", num(place.x / sw)))
                            .child(leaf("vert", num(place.y / sh))),
                    ),
            ],
        ));
    }
    if place.cropped() {
        let [left, right, top, bottom] = place.edges();
        out.push(effect(
            "Crop",
            "crop",
            vec![
                param("left", "left", "0", "100", left * 100.0),
                param("right", "right", "0", "100", right * 100.0),
                param("top", "top", "0", "100", top * 100.0),
                param("bottom", "bottom", "0", "100", bottom * 100.0),
            ],
        ));
    }
    out
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
            let place = placement(
                item.crop,
                (media.width, media.height),
                (model.width, model.height),
            );
            clip = clip.children(xmeml_place(&place, app));
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
/// `file://` URL, and writes plain paths itself. OpenTimelineIO has no crop
/// effect or audio track selection: crops and which audio track of the file
/// a clip is are kept in each clip's `metadata.isocast`.
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
            if let Some(crop) = item.crop.filter(|c| !c.is_full()) {
                meta["crop"] = json!(crop);
            }
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
            std::fs::rename(&partial, path).inspect_err(|_| {
                let _ = std::fs::remove_file(&partial);
            })
        })
        .map_err(|err| format!("could not write {}: {err}", path.display()))
}

impl Daemon {
    pub(crate) fn timeline_export_nle(
        &self,
        params: TimelineExportNleParams,
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
        self.heal_timelines();
        let (id, timeline) = {
            let timelines = self.timelines.lock();
            let id = timelines.resolve(params.timeline.as_deref())?;
            let timeline = timelines.get(&id).cloned().expect("resolved");
            (id, timeline)
        };
        if timeline.clips.is_empty() {
            return Err(wrong_state(format!(
                "timeline {id:?} has no clips to export yet; record a take while it is current"
            )));
        }
        let mut takes = Vec::new();
        for clip in &timeline.clips {
            let manifest = read_manifest(&clip.take)
                .map_err(|problem| failed(format!("clip {}: {problem}", clip.id)))?;
            if let Some(problem) = unfinished(&clip.id, &clip.take, &manifest) {
                return Err(failed(problem));
            }
            // The clip's stretch on its take's frame grid, as the export
            // rounds it.
            let (take_frames, fps) = take_length(&manifest).unwrap_or((
                (manifest.duration_seconds.unwrap_or(0.0) * 60.0).round() as u64,
                60,
            ));
            let frame = |seconds: f64| (seconds * f64::from(fps)).round().max(0.0) as u64;
            let in_frame = frame(clip.in_seconds).min(take_frames);
            let out_frame = frame(clip.out_seconds).min(take_frames);
            if out_frame <= in_frame {
                return Err(failed(format!(
                    "clip {} covers no frames of its take ({:.3}–{:.3} s); trim it (`isocast \
                     timeline trim`, MCP trim_clip) or take it off",
                    clip.id, clip.in_seconds, clip.out_seconds
                )));
            }
            takes.push(TakeClip {
                id: clip.id.clone(),
                folder: clip.take.parent().unwrap_or(Path::new("/")).to_path_buf(),
                manifest,
                in_frame,
                frames: out_frame - in_frame,
                fps,
                angle: clip.angle.clone().unwrap_or_else(|| PROGRAM.to_string()),
                crop: clip.crop.filter(|crop| !crop.is_full()),
            });
        }
        let model = build(&timeline.name, &takes).map_err(failed)?;
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
                        "path: {} is one of the takes' files",
                        path.display()
                    )));
                }
                if path.symlink_metadata().is_ok() && !params.replace && !ours(path) {
                    return Err(failed(format!(
                        "path: {} is there and isn't an Isocast timeline export; give replace: \
                         true (`--replace`) to replace it, or another path",
                        path.display()
                    )));
                }
                write_atomic(path, &text).map_err(failed)?;
                path.clone()
            }
            None => {
                let folder = params
                    .folder
                    .clone()
                    .unwrap_or_else(|| takes[0].folder.clone());
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
                let stem = crate::export::file_stem(&timeline.name);
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
                    let path = folder.join(format!("{stem}{label}{number}.{}", format.extension()));
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
                 srcEnable), so the program's sound doubles and uncropped angles show around \
                 crops; export xmeml for Resolve (`--nle xmeml --app resolve`)"
                    .into(),
            );
        }
        if format == NleFormat::Otio && takes.iter().any(|t| t.crop.is_some()) {
            notes.push(
                "OpenTimelineIO has no crop: crops are in each clip's metadata.isocast, not \
                 applied (export fcpxml or xmeml for them)"
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
            timeline: id,
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
    fn placement_of_a_crop() {
        // The right half of a 1920x1080 picture in a 1920x1080 sequence:
        // twice the size, its centre (480 px right of the picture's)
        // moved to the sequence's.
        let half = Crop {
            x: 0.5,
            y: 0.0,
            width: 0.5,
            height: 1.0,
        };
        let p = placement(Some(half), (1920, 1080), (1920, 1080));
        assert!((p.scale - 1.0).abs() < 1e-9 && (p.fit - 1.0).abs() < 1e-9);
        // 960x1080 fits by height: scale 1, centre moved 480 px left.
        assert!((p.x + 480.0).abs() < 1e-9 && p.y.abs() < 1e-9);
        // A 4K picture in 1080p only needs fitting.
        let p = placement(None, (3840, 2160), (1920, 1080));
        assert!((p.scale - 0.5).abs() < 1e-9 && !p.moved() && !p.cropped());
        assert_eq!(url_path(Path::new("/a b/é.mov")), "/a%20b/%C3%A9.mov");
    }
}
