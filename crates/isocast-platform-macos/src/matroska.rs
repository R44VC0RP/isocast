//! A small Matroska/WebM demuxer: the EBML elements a media source needs
//! to play a file from its start (the tracks, their codec setup, and each
//! block's frames, timestamps and BlockAdditions, where VP9's alpha
//! travels). Files are read in order, a cluster at a time, so a long file
//! isn't read into memory. Cues aren't read: media sources and stingers
//! always play from the start.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};

/// The four bytes every EBML file (Matroska and WebM) starts with.
pub const EBML_MAGIC: [u8; 4] = [0x1a, 0x45, 0xdf, 0xa3];

const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114d_9b74;
const INFO: u32 = 0x1549_a966;
const TRACKS: u32 = 0x1654_ae6b;
const CUES: u32 = 0x1c53_bb6b;
const CLUSTER: u32 = 0x1f43_b675;
const TAGS: u32 = 0x1254_c367;
const CHAPTERS: u32 = 0x1043_a770;
const ATTACHMENTS: u32 = 0x1941_a469;
const SEEK: u32 = 0x4dbb;
const SEEK_ID: u32 = 0x53ab;
const SEEK_POSITION: u32 = 0x53ac;
const CUE_POINT: u32 = 0xbb;
const CUE_TRACK_POSITIONS: u32 = 0xb7;
const CUE_CLUSTER_POSITION: u32 = 0xf1;
const DOC_TYPE: u32 = 0x4282;
const TIMESTAMP_SCALE: u32 = 0x2a_d7b1;
const DURATION: u32 = 0x4489;
const TRACK_ENTRY: u32 = 0xae;
const TRACK_NUMBER: u32 = 0xd7;
const TRACK_TYPE: u32 = 0x83;
const FLAG_ENABLED: u32 = 0xb9;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63a2;
const DEFAULT_DURATION: u32 = 0x23_e383;
const CODEC_DELAY: u32 = 0x56aa;
const CONTENT_ENCODINGS: u32 = 0x6d80;
const VIDEO: u32 = 0xe0;
const PIXEL_WIDTH: u32 = 0xb0;
const PIXEL_HEIGHT: u32 = 0xba;
const ALPHA_MODE: u32 = 0x53c0;
const COLOUR: u32 = 0x55b0;
const MATRIX_COEFFICIENTS: u32 = 0x55b1;
const BITS_PER_CHANNEL: u32 = 0x55b2;
const RANGE: u32 = 0x55b9;
const TRANSFER_CHARACTERISTICS: u32 = 0x55ba;
const PRIMARIES: u32 = 0x55bb;
const AUDIO: u32 = 0xe1;
const SAMPLING_FREQUENCY: u32 = 0xb5;
const CHANNELS: u32 = 0x9f;
const CLUSTER_TIMESTAMP: u32 = 0xe7;
const SIMPLE_BLOCK: u32 = 0xa3;
const BLOCK_GROUP: u32 = 0xa0;
const BLOCK: u32 = 0xa1;
const BLOCK_ADDITIONS: u32 = 0x75a1;
const BLOCK_MORE: u32 = 0xa6;
const BLOCK_ADD_ID: u32 = 0xee;
const BLOCK_ADDITIONAL: u32 = 0xa5;

/// Children of a Segment: one of these ends a cluster of unknown size.
const SEGMENT_CHILDREN: [u32; 8] = [
    SEEK_HEAD,
    INFO,
    TRACKS,
    CUES,
    CLUSTER,
    TAGS,
    CHAPTERS,
    ATTACHMENTS,
];

/// The longest stated duration believed: 30 days, in nanoseconds.
pub const MAX_DURATION_NS: u64 = 30 * 24 * 3600 * 1_000_000_000;

/// The largest element read into memory (a frame, codec setup).
const MAX_ELEMENT: u64 = 256 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Other,
}

/// A track's colour description (Matroska's Colour element), in the
/// ISO/IEC 23091-4 codes; `None` where absent or unspecified.
#[derive(Debug, Clone, Copy, Default)]
pub struct Colour {
    pub matrix: Option<u64>,
    pub primaries: Option<u64>,
    pub transfer: Option<u64>,
    /// 1 broadcast (video) range, 2 full range.
    pub range: Option<u64>,
    pub bits: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub number: u64,
    pub kind: TrackKind,
    pub enabled: bool,
    pub codec: String,
    pub private: Vec<u8>,
    /// Nanoseconds per frame, if the file says.
    pub default_duration: Option<u64>,
    /// Nanoseconds the codec delays its output (Opus's pre-skip).
    pub codec_delay: u64,
    /// Content compression or encryption: frames aren't stored as is.
    pub encoded: bool,
    pub width: u32,
    pub height: u32,
    /// The video has an alpha channel in BlockAdditional (BlockAddID 1).
    pub alpha: bool,
    pub colour: Colour,
    pub sample_rate: f64,
    pub channels: u32,
}

impl Track {
    fn new() -> Self {
        Self {
            number: 0,
            kind: TrackKind::Other,
            enabled: true,
            codec: String::new(),
            private: Vec::new(),
            default_duration: None,
            codec_delay: 0,
            encoded: false,
            width: 0,
            height: 0,
            alpha: false,
            colour: Colour::default(),
            sample_rate: 8000.0,
            channels: 1,
        }
    }
}

/// One frame of a track.
#[derive(Debug)]
pub struct Packet {
    /// Nanoseconds from the start of the file (negative for an audio
    /// codec's priming frames).
    pub time: i64,
    pub data: Vec<u8>,
    /// BlockAdditional with BlockAddID 1: VP9's alpha frame.
    pub alpha: Option<Vec<u8>>,
}

/// What a file holds, read up to its first cluster.
pub struct Demuxer {
    reader: BufReader<File>,
    pos: u64,
    /// Where the segment's contents start (positions in the SeekHead and
    /// Cues count from here) and end; `None` if its size is unknown (a live
    /// recording), when it runs to the end of the file.
    segment_start: u64,
    segment_end: Option<u64>,
    /// Where the Cues are, if the file says.
    cues: Option<u64>,
    file_len: u64,
    /// Nanoseconds per timestamp tick.
    scale: u64,
    /// "webm" or "matroska".
    pub doc_type: String,
    /// The file's stated duration in nanoseconds.
    pub duration: Option<u64>,
    pub tracks: Vec<Track>,
    /// The cluster being read: its timestamp and where it ends.
    cluster: Option<(u64, Option<u64>)>,
    /// Read only blocks' times, not their frames (empty `data`).
    pub times_only: bool,
    /// Frames of a laced block not yet returned.
    queued: std::collections::VecDeque<(u64, Packet)>,
}

impl Demuxer {
    /// Open `path` and read its header and tracks.
    pub fn open(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("could not open {}", path.display()))?;
        let file_len = file.metadata()?.len();
        let mut demuxer = Self {
            reader: BufReader::with_capacity(256 << 10, file),
            pos: 0,
            segment_start: 0,
            segment_end: None,
            cues: None,
            file_len,
            scale: 1_000_000,
            doc_type: String::new(),
            duration: None,
            tracks: Vec::new(),
            cluster: None,
            times_only: false,
            queued: Default::default(),
        };
        demuxer.read_header()?;
        Ok(demuxer)
    }

    /// Nanoseconds per timestamp tick.
    pub fn tick(&self) -> u64 {
        self.scale
    }

    pub fn track(&self, kind: TrackKind) -> Option<&Track> {
        // The first enabled track of the kind, like a player's default.
        self.tracks
            .iter()
            .find(|track| track.kind == kind && track.enabled)
            .or_else(|| self.tracks.iter().find(|track| track.kind == kind))
    }

    fn read_header(&mut self) -> Result<()> {
        let (id, size) = self.element_header()?;
        if id != EBML_MAGIC_ID {
            bail!("not a Matroska or WebM file");
        }
        let size = size.context("the EBML header has no size")?;
        let header_end = self.pos + size;
        let mut doc_type = String::new();
        while self.pos < header_end {
            let (id, size) = self.child_header()?;
            if id == DOC_TYPE {
                doc_type = self.string(size)?;
            } else {
                self.skip(size)?;
            }
        }
        if doc_type != "webm" && doc_type != "matroska" {
            bail!("an EBML file of type {doc_type:?}, not Matroska or WebM");
        }
        self.doc_type = doc_type;
        // The first segment.
        loop {
            let (id, size) = self.element_header()?;
            if id == SEGMENT {
                self.segment_start = self.pos;
                self.segment_end = size.map(|size| self.pos + size);
                break;
            }
            self.skip(size.context("an element of unknown size before the segment")?)?;
        }
        let mut have_tracks = false;
        loop {
            if self.at_segment_end() {
                break;
            }
            let start = self.pos;
            let (id, size) = self.element_header()?;
            match id {
                INFO => self.read_info(self.known(size)?)?,
                SEEK_HEAD => self.read_seek_head(self.known(size)?)?,
                CUES => {
                    self.cues = Some(start);
                    self.skip(self.known(size)?)?;
                }
                TRACKS => {
                    self.read_tracks(self.known(size)?)?;
                    have_tracks = true;
                }
                CLUSTER => {
                    // Blocks start here; read them from this cluster on.
                    self.seek(start)?;
                    break;
                }
                _ => self.skip(self.known(size)?)?,
            }
        }
        if !have_tracks {
            bail!("has no track list before its first frames");
        }
        Ok(())
    }

    /// Note where the SeekHead says the Cues are.
    fn read_seek_head(&mut self, size: u64) -> Result<()> {
        let end = self.pos + size;
        while self.pos < end {
            let (id, size) = self.child_header()?;
            if id != SEEK {
                self.skip(size)?;
                continue;
            }
            let seek_end = self.pos + size;
            let (mut target, mut position) = (0, None);
            while self.pos < seek_end {
                let (id, size) = self.child_header()?;
                match id {
                    SEEK_ID => target = self.uint(size)?,
                    SEEK_POSITION => position = Some(self.uint(size)?),
                    _ => self.skip(size)?,
                }
            }
            if let (CUES_ID, Some(position)) = (target, position) {
                self.cues = self.segment_start.checked_add(position);
            }
        }
        Ok(())
    }

    /// The time of the last frame of track `number` and the gap before
    /// it, found from the last cue point on (the Cues point at clusters
    /// that start with keyframes), without reading the whole file. `None`
    /// if the file has no Cues. Moves the read position: use a demuxer of
    /// its own.
    pub fn last_frame(&mut self, number: u64) -> Result<Option<(i64, i64)>> {
        let Some(cues) = self.cues.filter(|&cues| cues < self.file_len) else {
            return Ok(None);
        };
        self.seek(cues)?;
        let (id, size) = self.element_header()?;
        if id != CUES {
            return Ok(None);
        }
        let end = self.pos + self.known(size)?;
        let mut last_cluster = None;
        while self.pos < end {
            let (id, size) = self.child_header()?;
            if id != CUE_POINT {
                self.skip(size)?;
                continue;
            }
            let point_end = self.pos + size;
            while self.pos < point_end {
                let (id, size) = self.child_header()?;
                if id != CUE_TRACK_POSITIONS {
                    self.skip(size)?;
                    continue;
                }
                let positions_end = self.pos + size;
                while self.pos < positions_end {
                    let (id, size) = self.child_header()?;
                    if id == CUE_CLUSTER_POSITION {
                        let position = self.uint(size)?;
                        last_cluster = last_cluster.max(Some(position));
                    } else {
                        self.skip(size)?;
                    }
                }
            }
        }
        let Some(cluster) = last_cluster else {
            return Ok(None);
        };
        let Some(cluster) = self.segment_start.checked_add(cluster) else {
            return Ok(None);
        };
        self.seek(cluster)?;
        self.cluster = None;
        self.queued.clear();
        self.times_only = true;
        let mut times = Vec::new();
        while let Some(packet) = self.next_packet(number)? {
            times.push(packet.time);
        }
        // In presentation order (H.264's frames are stored out of it).
        times.sort_unstable();
        Ok(match times[..] {
            [] => None,
            [only] => Some((only, 0)),
            [.., before, last] => Some((last, last.saturating_sub(before))),
        })
    }

    fn read_info(&mut self, size: u64) -> Result<()> {
        let end = self.pos + size;
        let mut duration = None;
        while self.pos < end {
            let (id, size) = self.child_header()?;
            match id {
                // Up to a second a tick, so times stay in range.
                TIMESTAMP_SCALE => self.scale = self.uint(size)?.clamp(1, 1_000_000_000),
                DURATION => duration = Some(self.float(size)?),
                _ => self.skip(size)?,
            }
        }
        // A stated duration beyond 30 days is nonsense (a damaged or
        // hostile file); it is treated as absent.
        self.duration = duration
            .map(|ticks| ticks * self.scale as f64)
            .filter(|ns| ns.is_finite() && *ns > 0.0 && *ns < MAX_DURATION_NS as f64)
            .map(|ns| ns as u64);
        Ok(())
    }

    fn read_tracks(&mut self, size: u64) -> Result<()> {
        let end = self.pos + size;
        while self.pos < end {
            let (id, size) = self.child_header()?;
            if id != TRACK_ENTRY {
                self.skip(size)?;
                continue;
            }
            let entry_end = self.pos + size;
            let mut track = Track::new();
            while self.pos < entry_end {
                let (id, size) = self.child_header()?;
                match id {
                    TRACK_NUMBER => track.number = self.uint(size)?,
                    TRACK_TYPE => {
                        track.kind = match self.uint(size)? {
                            1 => TrackKind::Video,
                            2 => TrackKind::Audio,
                            _ => TrackKind::Other,
                        }
                    }
                    FLAG_ENABLED => track.enabled = self.uint(size)? != 0,
                    CODEC_ID => track.codec = self.string(size)?,
                    CODEC_PRIVATE => track.private = self.bytes(size)?,
                    DEFAULT_DURATION => track.default_duration = Some(self.uint(size)?),
                    CODEC_DELAY => track.codec_delay = self.uint(size)?,
                    CONTENT_ENCODINGS => {
                        track.encoded = true;
                        self.skip(size)?;
                    }
                    VIDEO => self.read_video(size, &mut track)?,
                    AUDIO => {
                        let end = self.pos + size;
                        while self.pos < end {
                            let (id, size) = self.child_header()?;
                            match id {
                                SAMPLING_FREQUENCY => track.sample_rate = self.float(size)?,
                                CHANNELS => track.channels = self.uint(size)? as u32,
                                _ => self.skip(size)?,
                            }
                        }
                    }
                    _ => self.skip(size)?,
                }
            }
            self.tracks.push(track);
        }
        Ok(())
    }

    fn read_video(&mut self, size: u64, track: &mut Track) -> Result<()> {
        let end = self.pos + size;
        while self.pos < end {
            let (id, size) = self.child_header()?;
            match id {
                PIXEL_WIDTH => track.width = self.uint(size)? as u32,
                PIXEL_HEIGHT => track.height = self.uint(size)? as u32,
                ALPHA_MODE => track.alpha = self.uint(size)? == 1,
                COLOUR => {
                    let end = self.pos + size;
                    // 2 ("unspecified") and 0 ("reserved"/unknown) say nothing.
                    let given = |code: u64| (code != 2 && code != 0).then_some(code);
                    while self.pos < end {
                        let (id, size) = self.child_header()?;
                        match id {
                            MATRIX_COEFFICIENTS => {
                                // 0 is identity (RGB) for the matrix, so keep it.
                                let code = self.uint(size)?;
                                track.colour.matrix = (code != 2).then_some(code);
                            }
                            PRIMARIES => track.colour.primaries = given(self.uint(size)?),
                            TRANSFER_CHARACTERISTICS => {
                                track.colour.transfer = given(self.uint(size)?)
                            }
                            RANGE => {
                                let code = self.uint(size)?;
                                track.colour.range = matches!(code, 1 | 2).then_some(code);
                            }
                            BITS_PER_CHANNEL => {
                                track.colour.bits = Some(self.uint(size)?).filter(|&b| b > 0)
                            }
                            _ => self.skip(size)?,
                        }
                    }
                }
                _ => self.skip(size)?,
            }
        }
        Ok(())
    }

    /// The next frame of track `number`, `None` at the end of the file.
    pub fn next_packet(&mut self, number: u64) -> Result<Option<Packet>> {
        loop {
            while let Some((track, packet)) = self.queued.pop_front() {
                if track == number {
                    return Ok(Some(packet));
                }
            }
            if !self.read_block(number)? {
                return Ok(None);
            }
        }
    }

    /// Read on to the next block of track `number` (others are skipped
    /// unread) and queue its frames. False at the end of the file.
    fn read_block(&mut self, number: u64) -> Result<bool> {
        loop {
            let Some((cluster_time, cluster_end)) = self.cluster else {
                // Between clusters: find the next one.
                if self.at_segment_end() {
                    return Ok(false);
                }
                let (id, size) = match self.element_header() {
                    Ok(header) => header,
                    // A recording cut short ends at its last whole element.
                    Err(_) if self.segment_end.is_none() => return Ok(false),
                    Err(err) => return Err(err),
                };
                if id == CLUSTER {
                    self.cluster = Some((0, size.map(|size| self.pos + size)));
                } else {
                    match size {
                        Some(size) => self.skip(size)?,
                        None => return Ok(false),
                    }
                }
                continue;
            };
            let ended = match cluster_end {
                Some(end) => self.pos >= end,
                None => self.at_segment_end(),
            };
            if ended {
                self.cluster = None;
                continue;
            }
            let start = self.pos;
            let (id, size) = match self.element_header() {
                Ok(header) => header,
                // A truncated file (a recording that was cut off) ends
                // at its last whole block.
                Err(_) => return Ok(false),
            };
            if cluster_end.is_none() && SEGMENT_CHILDREN.contains(&id) {
                // A cluster of unknown size ends where the next one starts.
                self.seek(start)?;
                self.cluster = None;
                continue;
            }
            let Some(size) = size else {
                bail!("a block of unknown size at byte {start}");
            };
            if self.pos + size > self.file_len {
                // Cut off part way through this element.
                return Ok(false);
            }
            match id {
                CLUSTER_TIMESTAMP => {
                    let time = self.uint(size)?;
                    self.cluster = Some((time, cluster_end));
                }
                SIMPLE_BLOCK => {
                    if self.block_track()? != number {
                        self.skip(size)?;
                        continue;
                    }
                    let data = self.payload(size)?;
                    self.queue_block(number, cluster_time, &data, None)?;
                    return Ok(true);
                }
                BLOCK_GROUP => {
                    if self.read_group(number, cluster_time, size)? {
                        return Ok(true);
                    }
                }
                _ => self.skip(size)?,
            }
        }
    }

    /// The track number at the start of a block, leaving the position
    /// where it was.
    fn block_track(&mut self) -> Result<u64> {
        let start = self.pos;
        let (number, _) = self.vint(false)?;
        self.seek(start)?;
        number.context("a block with no track number")
    }

    fn read_group(&mut self, number: u64, cluster_time: u64, size: u64) -> Result<bool> {
        let end = self.pos + size;
        let mut block = None;
        let mut alpha = None;
        while self.pos < end {
            let (id, size) = self.child_header()?;
            match id {
                BLOCK => {
                    if self.block_track()? != number {
                        self.seek(end)?;
                        return Ok(false);
                    }
                    block = Some(self.payload(size)?);
                }
                BLOCK_ADDITIONS => {
                    let additions_end = self.pos + size;
                    while self.pos < additions_end {
                        let (id, size) = self.child_header()?;
                        if id != BLOCK_MORE {
                            self.skip(size)?;
                            continue;
                        }
                        let more_end = self.pos + size;
                        let (mut add_id, mut data) = (1, None);
                        while self.pos < more_end {
                            let (id, size) = self.child_header()?;
                            match id {
                                BLOCK_ADD_ID => add_id = self.uint(size)?,
                                BLOCK_ADDITIONAL if self.times_only => self.skip(size)?,
                                BLOCK_ADDITIONAL => data = Some(self.bytes(size)?),
                                _ => self.skip(size)?,
                            }
                        }
                        if add_id == 1 {
                            alpha = data;
                        }
                    }
                }
                _ => self.skip(size)?,
            }
        }
        let Some(block) = block else {
            return Ok(false);
        };
        self.queue_block(number, cluster_time, &block, alpha)?;
        Ok(true)
    }

    /// Split a block (a SimpleBlock's or a BlockGroup's Block) into its
    /// frames and queue them.
    fn queue_block(
        &mut self,
        number: u64,
        cluster_time: u64,
        block: &[u8],
        alpha: Option<Vec<u8>>,
    ) -> Result<()> {
        let mut at = 0;
        vint_at(block, &mut at, false).context("a block with no track number")?;
        if block.len() < at + 3 {
            bail!("a block too short for its header");
        }
        let relative = i16::from_be_bytes([block[at], block[at + 1]]);
        let flags = block[at + 2];
        at += 3;
        let ticks = (cluster_time.min(i64::MAX as u64) as i64).saturating_add(i64::from(relative));
        let time = ticks.saturating_mul(self.scale as i64);
        let frames = match (flags >> 1) & 3 {
            _ if self.times_only => vec![&block[at..]],
            0 => vec![&block[at..]],
            lacing => laced(&block[at..], lacing)?,
        };
        let step = self
            .tracks
            .iter()
            .find(|track| track.number == number)
            .and_then(|track| track.default_duration)
            .unwrap_or(0) as i64;
        let mut alpha = alpha;
        for (i, frame) in frames.into_iter().enumerate() {
            self.queued.push_back((
                number,
                Packet {
                    time: time.saturating_add(step.saturating_mul(i as i64)),
                    data: frame.to_vec(),
                    alpha: alpha.take(),
                },
            ));
        }
        Ok(())
    }

    fn at_segment_end(&self) -> bool {
        self.pos >= self.segment_end.unwrap_or(u64::MAX).min(self.file_len)
    }

    fn known(&self, size: Option<u64>) -> Result<u64> {
        size.context("an element of unknown size where one must be known")
    }

    fn seek(&mut self, pos: u64) -> Result<()> {
        if pos != self.pos {
            self.reader.seek_relative(pos as i64 - self.pos as i64)?;
            self.pos = pos;
        }
        Ok(())
    }

    fn skip(&mut self, size: u64) -> Result<()> {
        self.seek(self.pos + size)
    }

    fn byte(&mut self) -> Result<u8> {
        let mut byte = [0];
        self.reader
            .read_exact(&mut byte)
            .context("the file ends part way through")?;
        self.pos += 1;
        Ok(byte[0])
    }

    /// A variable-length integer: an element ID (with its length marker,
    /// `id`) or a size (without; `None` if all ones, unknown).
    fn vint(&mut self, id: bool) -> Result<(Option<u64>, usize)> {
        let first = self.byte()?;
        let len = first.leading_zeros() as usize + 1;
        if len > 8 || (id && len > 4) {
            bail!("a malformed element at byte {}", self.pos - 1);
        }
        let mut value = if id {
            u64::from(first)
        } else {
            u64::from(first) & (0xff >> len)
        };
        let mut all_ones = value == (0xff >> len);
        for _ in 1..len {
            let byte = self.byte()?;
            all_ones &= byte == 0xff;
            value = (value << 8) | u64::from(byte);
        }
        if !id && all_ones {
            return Ok((None, len));
        }
        Ok((Some(value), len))
    }

    fn element_header(&mut self) -> Result<(u32, Option<u64>)> {
        let (id, _) = self.vint(true)?;
        let (size, _) = self.vint(false)?;
        Ok((id.expect("ids are always known") as u32, size))
    }

    /// A child element's header; children inside a known-size parent
    /// must have known sizes.
    fn child_header(&mut self) -> Result<(u32, u64)> {
        let (id, size) = self.element_header()?;
        Ok((id, self.known(size)?))
    }

    /// A block of `size` bytes: all of it, or (scanning for times) just
    /// its header.
    fn payload(&mut self, size: u64) -> Result<Vec<u8>> {
        if !self.times_only {
            return self.bytes(size);
        }
        // A track number of up to 8 bytes, the time and the flags.
        let header = size.min(11);
        let data = self.bytes(header)?;
        self.skip(size - header)?;
        Ok(data)
    }

    fn bytes(&mut self, size: u64) -> Result<Vec<u8>> {
        if size > MAX_ELEMENT {
            bail!("an element of {size} bytes, larger than a frame can be");
        }
        let mut data = vec![0; size as usize];
        self.reader
            .read_exact(&mut data)
            .context("the file ends part way through")?;
        self.pos += size;
        Ok(data)
    }

    fn uint(&mut self, size: u64) -> Result<u64> {
        if size > 8 {
            bail!("an integer of {size} bytes");
        }
        Ok(self
            .bytes(size)?
            .iter()
            .fold(0, |value, &byte| (value << 8) | u64::from(byte)))
    }

    fn float(&mut self, size: u64) -> Result<f64> {
        let data = self.bytes(size)?;
        Ok(match size {
            0 => 0.0,
            4 => f64::from(f32::from_be_bytes(data[..4].try_into()?)),
            8 => f64::from_be_bytes(data[..8].try_into()?),
            _ => bail!("a float of {size} bytes"),
        })
    }

    fn string(&mut self, size: u64) -> Result<String> {
        let data = self.bytes(size)?;
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        Ok(String::from_utf8_lossy(&data[..end]).into_owned())
    }
}

const EBML_MAGIC_ID: u32 = u32::from_be_bytes(EBML_MAGIC);
const CUES_ID: u64 = CUES as u64;

/// Whether `path` is an EBML file (Matroska or WebM), by its first bytes.
pub fn sniff(path: &Path) -> bool {
    let mut magic = [0; 4];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut magic))
        .is_ok()
        && magic == EBML_MAGIC
}

/// A size-style variable-length integer in `data` at `at`, advancing it.
fn vint_at(data: &[u8], at: &mut usize, signed: bool) -> Option<(i64, usize)> {
    let first = *data.get(*at)?;
    let len = first.leading_zeros() as usize + 1;
    if len > 8 || data.len() < *at + len {
        return None;
    }
    let mut value = u64::from(first) & (0xff >> len);
    for &byte in &data[*at + 1..*at + len] {
        value = (value << 8) | u64::from(byte);
    }
    *at += len;
    let value = if signed {
        // EBML lacing's signed sizes are offset by half the range.
        value as i64 - ((1i64 << (7 * len - 1)) - 1)
    } else {
        value as i64
    };
    Some((value, len))
}

/// The frames of a laced block's payload (after its header): Xiph (1),
/// fixed-size (2) or EBML (3) lacing.
fn laced(data: &[u8], lacing: u8) -> Result<Vec<&[u8]>> {
    let malformed = || anyhow::anyhow!("a malformed laced block");
    let count = *data.first().ok_or_else(malformed)? as usize + 1;
    let mut at = 1;
    let mut sizes = Vec::with_capacity(count);
    match lacing {
        1 => {
            for _ in 0..count - 1 {
                let mut size = 0;
                loop {
                    let byte = *data.get(at).ok_or_else(malformed)?;
                    at += 1;
                    size += byte as usize;
                    if byte != 255 {
                        break;
                    }
                }
                sizes.push(size);
            }
        }
        2 => {
            let each = (data.len() - at) / count;
            sizes.resize(count - 1, each);
        }
        _ => {
            let (first, _) = vint_at(data, &mut at, false).ok_or_else(malformed)?;
            let mut size = first;
            sizes.push(size as usize);
            for _ in 1..count - 1 {
                let (delta, _) = vint_at(data, &mut at, true).ok_or_else(malformed)?;
                size += delta;
                if size < 0 {
                    return Err(malformed());
                }
                sizes.push(size as usize);
            }
        }
    }
    let used: usize = sizes.iter().sum();
    if at + used > data.len() {
        return Err(malformed());
    }
    sizes.push(data.len() - at - used);
    let mut frames = Vec::with_capacity(count);
    for size in sizes {
        frames.push(&data[at..at + size]);
        at += size;
    }
    Ok(frames)
}
