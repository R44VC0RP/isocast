use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use isocast_protocol::Container;

use crate::mp4::{self, FragmentTrack, Track, TrackKind};

const AUDIO_RATE: u32 = 48_000;
const AAC_SAMPLES: u32 = 1024;
const FRAGMENT_BYTES: usize = 24 << 20;
const MAX_PACKET_BYTES: usize = 24 << 20;
const MAX_CONFIG_BYTES: usize = 1 << 20;
const MAX_NALS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

/// Encoder output description, available from NVENC/MF before or alongside
/// the first access unit. Decoder configuration is avcC/hvcC, not pixel data.
#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub codec: VideoCodec,
    pub width: u16,
    pub height: u16,
    pub timescale: u32,
    pub frame_duration: u32,
    /// A decoder configuration record using four-byte NAL lengths.
    pub decoder_config: Vec<u8>,
}

/// AAC-LC at the engine's 48 kHz mix rate. Priming is encoder-specific:
/// callers must supply it, never assume Apple's 2112 samples on Windows.
#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub channels: u16,
    pub priming: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub interval: Duration,
    /// Optional drive flush cadence, checked when a fragment is written.
    /// Explicit commit and finish always flush, irrespective of this setting.
    pub sync_every: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(500),
            sync_every: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Framing {
    /// The normal MOV/MP4 sample representation: four-byte big-endian sizes.
    LengthPrefixed,
    /// NVENC/MF access units with three/four-byte start codes. Parameter sets
    /// belong in decoder_config and are removed from avc1/hvc1 sample payloads.
    AnnexB,
}

pub struct VideoPacket {
    pub data: Vec<u8>,
    /// Saved-timeline ticks, relative to the start of this file.
    pub pts: u64,
    pub dts: u64,
    pub sync: bool,
    pub framing: Framing,
}

/// Committed fragment counts and separate admission headroom. Accepted/held
/// packets are not part of the committed counts until their fragment lands.
#[derive(Debug, Clone)]
pub struct Progress {
    pub video_frames: u64,
    pub audio_packets: Vec<u64>,
    pub fragments: u32,
    /// The shortest configured track's saved-timeline end. An empty track
    /// contributes zero; an I/O failure makes the promise unavailable.
    pub committed: Option<Duration>,
    /// Includes pending/held packets. Split before exhausting this budget,
    /// leaving headroom for the encoder's remaining in-flight output.
    pub remaining_samples: usize,
    pub accepted_video_frames: u64,
    pub accepted_audio_packets: Vec<u64>,
}

#[derive(Default)]
struct Pending {
    samples: Vec<(u32, u64, u32, bool)>,
    data: Vec<u8>,
    count: u64,
    accepted: u64,
    end: Option<u64>,
    last_input_end: Option<u64>,
}

struct Held {
    data: Vec<u8>,
    time: u64,
    sync: bool,
}

/// One serialized writer per output. The encoder adapter retains responsibility
/// for its GPU surfaces, fences, admission, encoder-drain barrier and lost-input
/// counters. This layer only accepts and indexes compressed outputs.
pub struct Writer {
    mux: Option<mp4::Writer>,
    video: VideoConfig,
    parameter_sets: Vec<Vec<u8>>,
    tracks: Vec<Track>,
    pending: Vec<Pending>,
    held: Option<Held>,
    last_video: Option<u64>,
    bytes: usize,
    fragments: u32,
    config: Config,
    last_sync: Instant,
    failed: Option<String>,
}

impl Writer {
    pub fn create(
        path: &Path,
        container: Container,
        created: SystemTime,
        video: VideoConfig,
        audio: &[AudioConfig],
        config: Config,
    ) -> Result<Self> {
        Self::with_mux(container, video, audio, config, |tracks| {
            let mut mux = mp4::Writer::create(path, container, created)?;
            mux.set_tracks(tracks)?;
            Ok(mux)
        })
    }

    /// Install actual encoder descriptions on an already-reserved file. Some
    /// hardware encoders reveal their configuration only with the first packet;
    /// ownership must still be acquired before the take is announced.
    pub fn from_reserved(
        mux: mp4::Writer,
        video: VideoConfig,
        audio: &[AudioConfig],
        config: Config,
    ) -> Result<Self> {
        Self::configure_reserved(&mut Some(mux), video, audio, config)
    }

    /// Configure an owned reservation without losing it on invalid encoder
    /// descriptions. Every fallible validation precedes taking the handle;
    /// callers can still preserve accepted audio if video never configures.
    pub fn configure_reserved(
        reserved: &mut Option<mp4::Writer>,
        video: VideoConfig,
        audio: &[AudioConfig],
        config: Config,
    ) -> Result<Self> {
        let container = reserved.as_ref().context("no reserved movie")?.container();
        Self::with_mux(container, video, audio, config, |tracks| {
            // set_tracks validates before mutation, and writes no file bytes.
            reserved
                .as_mut()
                .context("no reserved movie")?
                .set_tracks(tracks)?;
            Ok(reserved.take().expect("validated reservation"))
        })
    }

    fn with_mux(
        container: Container,
        video: VideoConfig,
        audio: &[AudioConfig],
        config: Config,
        create: impl FnOnce(Vec<Track>) -> Result<mp4::Writer>,
    ) -> Result<Self> {
        if !matches!(container, Container::Mov | Container::Mp4) {
            bail!("container: encoded movie writer supports mov or mp4");
        }
        if !(Duration::from_millis(50)..=Duration::from_secs(10)).contains(&config.interval)
            || config.sync_every.is_some_and(|d| d.is_zero())
        {
            bail!("fragment interval must be 50..10000 ms and sync cadence must be positive");
        }
        if video.width == 0
            || video.height == 0
            || video.timescale == 0
            || video.frame_duration == 0
        {
            bail!("video: dimensions, timescale and nominal frame duration must be positive");
        }
        let parameter_sets = validate_configuration(&video)?;
        if audio.len() > 16
            || audio
                .iter()
                .any(|a| !(1..=2).contains(&a.channels) || a.priming > AUDIO_RATE * 10)
        {
            bail!("audio: at most 16 AAC-LC tracks, one/two channels, priming at most 10 seconds");
        }
        let delay = audio.iter().map(|a| a.priming).max().unwrap_or(0);
        let (fourcc, atom, name) = match video.codec {
            VideoCodec::H264 => (*b"avc1", *b"avcC", "H.264"),
            VideoCodec::Hevc => (*b"hvc1", *b"hvcC", "HEVC"),
        };
        let mut tracks = vec![Track {
            kind: TrackKind::Video {
                width: video.width,
                height: video.height,
            },
            timescale: video.timescale,
            sample_entry: mp4::video_sample_entry(
                fourcc,
                video.width,
                video.height,
                24,
                name,
                &[(atom, video.decoder_config.clone())],
            )?,
            skip: 0,
            fragment_shift: (u64::from(delay) * u64::from(video.timescale))
                .div_ceil(u64::from(AUDIO_RATE)),
        }];
        for a in audio {
            tracks.push(Track {
                kind: TrackKind::Audio {
                    channels: a.channels,
                    sample_rate: AUDIO_RATE,
                },
                timescale: AUDIO_RATE,
                sample_entry: mp4::aac_sample_entry(container, a.channels, AUDIO_RATE)?,
                skip: u64::from(a.priming),
                fragment_shift: u64::from(delay - a.priming),
            });
        }
        let mux = create(tracks.clone())?;
        let pending = (0..tracks.len()).map(|_| Pending::default()).collect();
        Ok(Self {
            mux: Some(mux),
            video,
            parameter_sets,
            tracks,
            pending,
            held: None,
            last_video: None,
            bytes: 0,
            fragments: 0,
            config,
            last_sync: Instant::now(),
            failed: None,
        })
    }

    /// One access unit for one picture. Low-latency NVENC/MF adapters must
    /// disable reordering; B-frame timestamps are refused rather than corrupted.
    pub fn video(&mut self, packet: VideoPacket) -> Result<()> {
        self.healthy()?;
        self.room_for_sample()?;
        if packet.pts != packet.dts {
            bail!("video.pts/dts: reordered pictures are not supported");
        }
        if self.last_video.is_some_and(|at| packet.dts <= at) {
            bail!("video.dts: pictures must arrive in strictly increasing decode order");
        }
        if self.last_video.is_none() && !packet.sync {
            bail!("video: the first picture must be a random-access picture");
        }
        self.check_time(0, packet.dts, self.video.frame_duration)?;
        let duration = self
            .held
            .as_ref()
            .map(|held| {
                u32::try_from(packet.dts - held.time)
                    .context("video gap is too long for a sample duration")
            })
            .transpose()?;
        let data = access_unit(
            packet.data,
            packet.framing,
            self.video.codec,
            &self.parameter_sets,
        )?;
        if self.bytes + data.len() > FRAGMENT_BYTES {
            self.flush()?;
        }
        if let Some(held) = self.held.take() {
            self.push(
                0,
                held.data,
                held.time,
                duration.expect("held picture duration"),
                held.sync,
            );
        }
        self.last_video = Some(packet.dts);
        self.pending[0].accepted += 1;
        self.held = Some(Held {
            data,
            time: packet.dts,
            sync: packet.sync,
        });
        self.flush_if_due()
    }

    /// One raw AAC-LC packet (no ADTS header). Its decode time includes the
    /// encoder's priming. Time zero is that encoder stream's pre-roll origin;
    /// a missing first packet keeps its original time (never reapplies priming).
    /// Late capture sources must be aligned/filled before encoding, not start
    /// a new pre-roll in the middle of this stream. `samples` is its actual
    /// media duration, at most 1024.
    pub fn audio(&mut self, track: usize, data: Vec<u8>, time: u64, samples: u32) -> Result<()> {
        self.healthy()?;
        self.room_for_sample()?;
        let index = track
            .checked_add(1)
            .filter(|i| *i < self.tracks.len())
            .context("audio.track: no such track")?;
        if data.is_empty() || data.len() > MAX_PACKET_BYTES || !(1..=AAC_SAMPLES).contains(&samples)
        {
            bail!("audio packet must be nonempty, at most24MiB and contain 1..1024 sample frames");
        }
        let end = time
            .checked_add(u64::from(samples))
            .context("audio timestamp overflow")?;
        self.check_time(index, time, samples)?;
        if data.len() >= 2 && data[0] == 0xff && data[1] & 0xf6 == 0xf0 {
            bail!("audio.data: expected a raw AAC access unit, not an ADTS frame");
        }
        if self.pending[index]
            .last_input_end
            .is_some_and(|previous| time < previous)
        {
            bail!("audio.time: packets overlap or arrive out of order");
        }
        let previous_duration = self.pending[index]
            .samples
            .last()
            .map(|s| {
                u32::try_from(time - s.1).context("audio gap is too long for a sample duration")
            })
            .transpose()?;
        if self.bytes + data.len() > FRAGMENT_BYTES {
            self.flush()?;
        }
        // Inside one trun a gap belongs in the preceding sample's duration.
        // Across fragments tfdt retains it; finalization/recovery agree later.
        if let Some(previous) = self.pending[index].samples.last_mut() {
            previous.2 = previous_duration.expect("pending audio duration");
        }
        self.pending[index].last_input_end = Some(end);
        self.pending[index].accepted += 1;
        self.push(index, data, time, samples, true);
        self.flush_if_due()
    }

    /// Encoder adapters must first drain their accepted encoder work. This
    /// commits only packets actually delivered here, including the held picture.
    pub fn commit(&mut self) -> Result<Progress> {
        self.healthy()?;
        self.release_held();
        self.flush()?;
        let synced = self.mux.as_ref().context("movie is finished")?.sync();
        self.keep_error(synced)?;
        self.last_sync = Instant::now();
        Ok(self.progress())
    }

    /// Finish every delivered sample, keeping the longest track. The adapter
    /// supplies the last AAC packet's actual presentation length when trimming
    /// encoder tail padding; this method never guesses a shorter file end.
    pub fn finish(mut self) -> Result<Progress> {
        self.healthy()?;
        self.release_held();
        self.flush()?;
        let mux = self.mux.take().context("movie is finished")?;
        mux.finish()?;
        Ok(self.progress())
    }

    /// Cancel a file that has accepted no encoded media. Refusal after packet
    /// admission leaves this writer usable so its caller can finish those data.
    #[cfg(windows)]
    pub fn cancel_empty(&mut self) -> Result<()> {
        if self.pending.iter().any(|p| p.accepted != 0) {
            bail!("movie has accepted packets; finish it instead of cancelling");
        }
        let mux = self
            .mux
            .take()
            .context("movie is already finished or cancelled")?;
        self.failed = Some("movie was cancelled".into());
        let result = mux.cancel_empty();
        if let Err(error) = &result {
            self.failed = Some(format!(
                "empty-file cancellation failed; file retained: {error:#}"
            ));
        }
        result
    }

    pub fn progress(&self) -> Progress {
        let committed = self.failed.is_none().then(|| {
            self.pending
                .iter()
                .zip(&self.tracks)
                .map(|(p, t)| duration(p.end.unwrap_or(0).saturating_sub(t.skip), t.timescale))
                .min()
                .unwrap_or(Duration::ZERO)
        });
        Progress {
            video_frames: self.pending[0].count,
            audio_packets: self.pending[1..].iter().map(|p| p.count).collect(),
            fragments: self.fragments,
            committed,
            remaining_samples: self.remaining_samples(),
            accepted_video_frames: self.pending[0].accepted,
            accepted_audio_packets: self.pending[1..].iter().map(|p| p.accepted).collect(),
        }
    }

    fn remaining_samples(&self) -> usize {
        mp4::MAX_SAMPLES
            - self
                .pending
                .iter()
                .map(|p| p.accepted as usize)
                .sum::<usize>()
    }

    fn room_for_sample(&self) -> Result<()> {
        if self.remaining_samples() == 0 {
            bail!(
                "sample index budget exhausted: finish this file and split before submitting another packet"
            );
        }
        Ok(())
    }

    fn check_time(&self, index: usize, time: u64, length: u32) -> Result<()> {
        let track = &self.tracks[index];
        let end = time
            .checked_add(u64::from(length))
            .context("packet timestamp overflow")?;
        if end
            .checked_add(track.fragment_shift)
            .is_none_or(|end| end > i64::MAX as u64)
        {
            bail!("packet timestamp exceeds signed media time");
        }
        let movie_end = (u128::from(end.saturating_sub(track.skip))
            * u128::from(mp4::MOVIE_TIMESCALE)
            + u128::from(track.timescale / 2))
            / u128::from(track.timescale);
        if movie_end > u128::from(u64::MAX) {
            bail!("packet movie duration overflows");
        }
        Ok(())
    }

    fn healthy(&self) -> Result<()> {
        if let Some(error) = &self.failed {
            bail!("movie writer failed: {error}");
        }
        Ok(())
    }

    fn keep_error(&mut self, result: Result<()>) -> Result<()> {
        if let Err(error) = &result {
            self.failed = Some(format!("{error:#}"));
        }
        result
    }

    fn release_held(&mut self) {
        if let Some(held) = self.held.take() {
            self.push(
                0,
                held.data,
                held.time,
                self.video.frame_duration,
                held.sync,
            );
        }
    }

    fn push(&mut self, track: usize, data: Vec<u8>, time: u64, length: u32, sync: bool) {
        self.bytes += data.len();
        self.pending[track]
            .samples
            .push((data.len() as u32, time, length, sync));
        self.pending[track].data.extend_from_slice(&data);
    }

    fn flush_if_due(&mut self) -> Result<()> {
        let first_picture = self.pending[0].count == 0 && !self.pending[0].samples.is_empty();
        let interval = self.pending.iter().zip(&self.tracks).any(|(p, t)| {
            let Some(first) = p.samples.first() else {
                return false;
            };
            let last = p.samples.last().expect("nonempty");
            duration(last.1 + u64::from(last.2) - first.1, t.timescale) >= self.config.interval
        });
        if first_picture || interval || self.bytes >= FRAGMENT_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.healthy()?;
        if self.pending.iter().all(|p| p.samples.is_empty()) {
            return Ok(());
        }
        let next = self
            .fragments
            .checked_add(1)
            .context("fragment sequence exhausted")?;
        // A commit releases the held picture with nominal duration. A later
        // jittered timestamp can legitimately be slightly earlier than that
        // estimate. Preserve its actual DTS, but don't let a later fragment's
        // tail shorten the timeline already promised by an earlier commit.
        let video = &mut self.pending[0];
        if let (Some(end), Some(last)) = (video.end, video.samples.last_mut()) {
            let minimum = u32::try_from(end.saturating_sub(last.1))
                .context("video commit extension exceeds sample duration")?;
            last.2 = last.2.max(minimum);
        }
        let parts: Vec<_> = self
            .pending
            .iter()
            .map(|p| FragmentTrack {
                samples: &p.samples,
                data: &p.data,
            })
            .collect();
        let written = self
            .mux
            .as_mut()
            .context("movie is finished")?
            .write_fragment(&parts);
        self.keep_error(written)?;
        for part in &mut self.pending {
            if let Some(last) = part.samples.last() {
                part.end = Some(last.1 + u64::from(last.2));
            }
            part.count += part.samples.len() as u64;
            part.samples.clear();
            part.data.clear();
        }
        self.bytes = 0;
        self.fragments = next;
        if self
            .config
            .sync_every
            .is_some_and(|every| self.last_sync.elapsed() >= every)
        {
            let synced = self.mux.as_ref().context("movie is finished")?.sync();
            self.keep_error(synced)?;
            self.last_sync = Instant::now();
        }
        Ok(())
    }
}

fn duration(ticks: u64, scale: u32) -> Duration {
    let scale = u64::from(scale);
    Duration::new(
        ticks / scale,
        ((ticks % scale) * 1_000_000_000 / scale) as u32,
    )
}

fn validate_configuration(video: &VideoConfig) -> Result<Vec<Vec<u8>>> {
    let config = &video.decoder_config;
    let minimum = match video.codec {
        VideoCodec::H264 => 7,
        VideoCodec::Hevc => 23,
    };
    let length_byte = match video.codec {
        VideoCodec::H264 => 4,
        VideoCodec::Hevc => 21,
    };
    if config.len() < minimum
        || config.len() > MAX_CONFIG_BYTES
        || config[0] != 1
        || config[length_byte] & 3 != 3
    {
        bail!("video.decoder_config: expected bounded avcC/hvcC with four-byte NAL lengths");
    }
    let mut parameters = Vec::new();
    let mut total_nals = 0;
    let mut take_nals = |at: &mut usize, count: usize, kind: u8| -> Result<()> {
        if count == 0 || count > MAX_NALS {
            bail!("decoder configuration has an invalid NAL count");
        }
        total_nals += count;
        if total_nals > MAX_NALS {
            bail!("decoder configuration has too many NALs");
        }
        for _ in 0..count {
            let size = config
                .get(*at..*at + 2)
                .context("truncated configuration NAL size")?;
            let size = u16::from_be_bytes(size.try_into().unwrap()) as usize;
            *at += 2;
            let end = at
                .checked_add(size)
                .filter(|end| *end <= config.len())
                .context("truncated configuration NAL")?;
            let nal = &config[*at..end];
            validate_nal(nal, video.codec)?;
            if nal_kind(nal, video.codec) != kind {
                bail!("decoder configuration NAL type mismatch");
            }
            if matches!(
                (video.codec, kind),
                (VideoCodec::H264, 7 | 8 | 13) | (VideoCodec::Hevc, 32..=34)
            ) {
                parameters.push(nal.to_vec());
            }
            *at = end;
        }
        Ok(())
    };
    match video.codec {
        VideoCodec::H264 => {
            let mut at = 6;
            take_nals(&mut at, usize::from(config[5] & 31), 7)?;
            let count = usize::from(*config.get(at).context("missing PPS count")?);
            at += 1;
            take_nals(&mut at, count, 8)?;
            if at < config.len() {
                let extension = config
                    .get(at..at + 4)
                    .context("truncated avcC profile extension")?;
                at += 4;
                if extension[3] != 0 {
                    take_nals(&mut at, usize::from(extension[3]), 13)?;
                }
            }
            if at != config.len() {
                bail!("unexpected bytes after avcC parameter sets");
            }
        }
        VideoCodec::Hevc => {
            let mut at = 23;
            for _ in 0..config[22] {
                let array = config.get(at..at + 3).context("truncated hvcC array")?;
                at += 3;
                let count = usize::from(u16::from_be_bytes([array[1], array[2]]));
                if count != 0 {
                    take_nals(&mut at, count, array[0] & 63)?;
                }
            }
            if at != config.len() {
                bail!("unexpected bytes after hvcC arrays");
            }
            if [32, 33, 34].iter().any(|kind| {
                !parameters
                    .iter()
                    .any(|nal| nal_kind(nal, video.codec) == *kind)
            }) {
                bail!("hvcC needs VPS, SPS and PPS parameter sets");
            }
        }
    }
    Ok(parameters)
}

fn nal_kind(nal: &[u8], codec: VideoCodec) -> u8 {
    match codec {
        VideoCodec::H264 => nal[0] & 31,
        VideoCodec::Hevc => (nal[0] >> 1) & 63,
    }
}

fn validate_nal(nal: &[u8], codec: VideoCodec) -> Result<()> {
    if nal.len() < 2 || nal[0] & 0x80 != 0 || (codec == VideoCodec::Hevc && nal[1] & 7 == 0) {
        bail!("invalid encoded NAL header");
    }
    Ok(())
}

fn access_unit(
    data: Vec<u8>,
    framing: Framing,
    codec: VideoCodec,
    parameters: &[Vec<u8>],
) -> Result<Vec<u8>> {
    if data.is_empty() || data.len() > MAX_PACKET_BYTES {
        bail!("video packet must be nonempty and at most24MiB");
    }
    let mut units = Vec::new();
    match framing {
        Framing::LengthPrefixed => {
            let mut at = 0;
            while at < data.len() {
                let size = data.get(at..at + 4).context("truncated NAL length")?;
                let size = u32::from_be_bytes(size.try_into().unwrap()) as usize;
                at += 4;
                let end = at
                    .checked_add(size)
                    .filter(|end| *end <= data.len())
                    .context("NAL exceeds access unit")?;
                if size == 0 {
                    bail!("empty NAL in access unit");
                }
                units.push(&data[at..end]);
                if units.len() > MAX_NALS {
                    bail!("too many NALs in one access unit");
                }
                at = end;
            }
        }
        Framing::AnnexB => {
            let mut start = None;
            let mut zeroes = 0;
            for (at, &byte) in data.iter().enumerate() {
                if byte == 0 {
                    zeroes += 1;
                    continue;
                }
                if byte == 1 && zeroes >= 2 {
                    if let Some(start) = start {
                        if at - zeroes == start {
                            bail!("empty Annex B NAL");
                        }
                        units.push(&data[start..at - zeroes]);
                    } else if data[..at - zeroes].iter().any(|b| *b != 0) {
                        bail!("Annex B data precedes its first start code");
                    }
                    start = Some(at + 1);
                }
                zeroes = 0;
                if units.len() > MAX_NALS {
                    bail!("too many NALs in one access unit");
                }
            }
            let start = start.context("Annex B packet has no start code")?;
            let end = data.len() - zeroes;
            if start >= end {
                bail!("empty Annex B NAL");
            }
            units.push(&data[start..end]);
        }
    }
    if units.len() > MAX_NALS {
        bail!("too many NALs in one access unit");
    }
    for nal in &units {
        validate_nal(nal, codec)?;
    }
    let kind = |nal: &[u8]| nal_kind(nal, codec);
    if !units.iter().any(|nal| match codec {
        VideoCodec::H264 => (1..=5).contains(&kind(nal)),
        VideoCodec::Hevc => kind(nal) <= 31,
    }) {
        bail!("video access unit has no coded picture");
    }
    let parameter_set = |nal: &[u8]| match codec {
        VideoCodec::H264 => matches!(kind(nal), 7 | 8 | 13),
        VideoCodec::Hevc => matches!(kind(nal), 32..=34),
    };
    if units
        .iter()
        .any(|nal| parameter_set(nal) && !parameters.iter().any(|known| known.as_slice() == *nal))
    {
        bail!("video parameter sets changed: start a new file with the new configuration");
    }
    if matches!(framing, Framing::LengthPrefixed) && !units.iter().any(|nal| parameter_set(nal)) {
        return Ok(data);
    }
    let mut out = Vec::with_capacity(data.len());
    for nal in units.into_iter().filter(|nal| !parameter_set(nal)) {
        if out.len() + 4 + nal.len() > MAX_PACKET_BYTES {
            bail!("normalized access unit exceeds24MiB");
        }
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }
    Ok(out)
}
