//! Recording through caller-configured native video and per-track MF AAC. This
//! adapter does not translate recording quality into a guessed live bitrate.

mod audio;

use std::collections::VecDeque;
use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{
    EncodedVideo, FileWriter, Finished, RepairedFile, Time, VideoEncoder, WriterSpec,
};
use isocast_mux::{Config, Framing, Progress, VideoCodec, VideoConfig, VideoPacket, mp4};
use isocast_protocol::{Codec, Container};

use crate::{Frame, VideoEncoding, WindowsVideoEncoder};
use audio::{AudioTrack, MAX_BATCH, MAX_HELD, RATE};

const FRAME_DURATION: u32 = 2000;
// MF admits at most eight pictures; NVENC admits four. Keep our timestamp
// bookkeeping bounded independently of either provider's admission limit.
const MAX_PENDING: usize = 8;
// File routes already buffer input. Absorb a brief encoder completion stall
// there instead of turning retryable admission pressure into a permanent
// timestamp gap. After waiting, leave a cooldown to drain queued input without
// more waits: a slower-than-real-time provider must not accumulate audio/video
// in the route until that queue overflows.
const ADMISSION_WAIT: Duration = Duration::from_millis(100);
// Admission/no-output retry limit, not a deadline around native COM calls or
// worker joins; those can block in the provider.
const DRAIN_WAIT: Duration = Duration::from_secs(5);

struct Pending {
    pts: Time,
    repeated: bool,
}

pub struct VideoWriter {
    spec: WriterSpec,
    encoder: WindowsVideoEncoder,
    reserved: Option<mp4::Writer>,
    mux: Option<isocast_mux::Writer>,
    audio: Vec<AudioTrack>,
    // Terminal fallback when no actual video description ever arrived. It
    // uses reserved's original handle/lease, never reopens the output path.
    audio_only: bool,
    audio_fragments: u32,
    decoder_config: Option<Vec<u8>>,
    codec: VideoCodec,
    timescale: u32,
    start: Option<Time>,
    pending: VecDeque<Pending>,
    last_video: Option<(Frame, Time)>,
    admission_wait_until: Option<Instant>,
    accepted: u64,
    accepted_originals: u64,
    completed: u64,
    repeated: u64,
    reported_losses: u64,
    last_written: Option<u64>,
    failed: Option<String>,
    output_failed: bool,
    closed: bool,
}

impl VideoWriter {
    /// Reserve a new file, without replacing an existing file. The encoder
    /// must be fresh, configured for this size/rate and the caller's actual
    /// recording-quality policy, with no reordered pictures. Its current API
    /// exposes codec/depth and counters, but not its size/rate/quality settings;
    /// those remain the caller's responsibility. Audio is 48 kHz AAC-LC,
    /// 128 kb/s mono or 192 kb/s stereo. Each audio append is atomic and accepts
    /// at most 16384 frames (larger media chunks must be split upstream).
    pub fn from_encoder(spec: &WriterSpec, encoder: WindowsVideoEncoder) -> Result<Self> {
        let (codec, encoding, timescale) = Self::validate(spec)
            .with_context(|| format!("could not create {}", spec.path.display()))?;
        if encoder.encoding() != encoding {
            bail!(
                "{}: video.encoder: requested {encoding:?}, but the supplied encoder produces {:?}",
                spec.path.display(),
                encoder.encoding()
            );
        }
        let stats = encoder
            .stats()
            .with_context(|| format!("could not inspect encoder for {}", spec.path.display()))?;
        if stats.accepted != 0
            || stats.completed != 0
            || stats.dropped != 0
            || stats.in_flight != 0
            || stats.ready_packets != 0
        {
            bail!(
                "{}: video.encoder: recording requires a fresh encoder",
                spec.path.display()
            );
        }
        let audio = spec
            .audio
            .iter()
            .enumerate()
            .map(|(index, track)| {
                AudioTrack::new(track.channels, spec.container)
                    .with_context(|| format!("audio[{index}]"))
            })
            .collect::<Result<Vec<_>>>()?;
        // MF may not have decoder configuration until its first picture. Own
        // the path now, rather than discovering a collision after start.
        let reserved =
            mp4::Writer::create(&spec.path, spec.container, spec.created).with_context(|| {
                format!(
                    "could not reserve {} (any partial file is retained)",
                    spec.path.display()
                )
            })?;
        let mut writer = Self {
            spec: spec.clone(),
            encoder,
            reserved: Some(reserved),
            mux: None,
            audio,
            audio_only: false,
            audio_fragments: 0,
            decoder_config: None,
            codec,
            timescale,
            start: None,
            pending: VecDeque::new(),
            last_video: None,
            admission_wait_until: None,
            accepted: 0,
            accepted_originals: 0,
            completed: 0,
            repeated: 0,
            reported_losses: 0,
            last_written: None,
            failed: None,
            output_failed: false,
            closed: false,
        };
        // NVENC already exposes its real description. Let audio publish now,
        // even if the first picture has not completed. MF may expose it later.
        // Keep the established video-only reservation/first-picture path.
        if !writer.audio.is_empty()
            && let Ok(config) = writer.encoder.decoder_config()
        {
            writer.configure(config)?;
        }
        Ok(writer)
    }

    fn validate(spec: &WriterSpec) -> Result<(VideoCodec, VideoEncoding, u32)> {
        if spec.audio.len() > 16 {
            bail!("audio: at most 16 AAC tracks are supported");
        }
        for (index, track) in spec.audio.iter().enumerate() {
            if !matches!(track.channels, 1 | 2) {
                bail!("audio[{index}].channels: valid values are 1, 2");
            }
        }
        if !matches!(spec.container, Container::Mov | Container::Mp4) {
            bail!("container: valid Windows recording containers are mov, mp4");
        }
        let video = &spec.video;
        if video.width == 0
            || video.height == 0
            || video.width > u32::from(u16::MAX)
            || video.height > u32::from(u16::MAX)
            || !video.width.is_multiple_of(2)
            || !video.height.is_multiple_of(2)
        {
            bail!(
                "video.width/height: 4:2:0 recording requires positive even dimensions at most 65534"
            );
        }
        if !(1..=240).contains(&video.fps) {
            bail!("video.fps: valid range is 1..=240");
        }
        let encoding = match (video.encoder.codec, video.encoder.effective_bit_depth()) {
            (Codec::H264, 8) => VideoEncoding::H264,
            (Codec::Hevc, 8) => VideoEncoding::Hevc,
            (Codec::Hevc, 10) => VideoEncoding::Hevc10,
            (Codec::H264 | Codec::Hevc, _) => {
                bail!("video.encoder.bit_depth: h264 supports 8; hevc supports 8 or 10");
            }
            _ => bail!(
                "video.encoder.codec: native Windows ProRes recording is not implemented; valid codecs are h264, hevc"
            ),
        };
        let codec = match encoding {
            VideoEncoding::H264 => VideoCodec::H264,
            VideoEncoding::Hevc | VideoEncoding::Hevc10 => VideoCodec::Hevc,
        };
        let timescale = video
            .fps
            .checked_mul(FRAME_DURATION)
            .context("video.fps: timescale overflow")?;
        Ok((codec, encoding, timescale))
    }

    fn healthy(&self) -> Result<()> {
        if self.closed {
            bail!("the file writer is finished");
        }
        if let Some(error) = &self.failed {
            bail!("the file writer failed: {error}");
        }
        Ok(())
    }

    fn keep_error<T>(&mut self, result: Result<T>) -> Result<T> {
        result.map_err(|error| {
            let error = error.context(format!("recording {}", self.spec.path.display()));
            if self.failed.is_none() {
                self.failed = Some(format!("{error:#}"));
            }
            error
        })
    }

    fn ticks(&self, pts: Time) -> Result<u64> {
        let start = self
            .start
            .context("video: start the file before appending")?;
        if pts < start {
            bail!("video.pts: must not precede the file start");
        }
        // Nonnegative host times and the validated <=480000 Hz timescale
        // keep Time::ticks_since's rounded result inside signed 64-bit ticks.
        u64::try_from(pts.ticks_since(start, self.timescale))
            .context("video.pts: negative saved-timeline timestamp")
    }

    fn time_at(&self, ticks: u64) -> Result<Time> {
        let start = self.start.context("video: the file has not started")?;
        let nanos = i128::from(start.as_nanos())
            + i128::from(ticks) * 1_000_000_000 / i128::from(self.timescale);
        Ok(Time::from_nanos(
            i64::try_from(nanos).context("video timestamp overflow")?,
        ))
    }

    fn configure(&mut self, config: Vec<u8>) -> Result<()> {
        let audio: Vec<_> = self
            .audio
            .iter()
            .map(|track| track.config.clone())
            .collect();
        self.mux = Some(isocast_mux::Writer::configure_reserved(
            &mut self.reserved,
            VideoConfig {
                codec: self.codec,
                width: self.spec.video.width as u16,
                height: self.spec.video.height as u16,
                timescale: self.timescale,
                frame_duration: FRAME_DURATION,
                decoder_config: config.clone(),
            },
            &audio,
            Config::default(),
        )?);
        self.decoder_config = Some(config);
        Ok(())
    }

    fn check_budget(&self, video: u64, audio: Option<(usize, u64)>, repeat: bool) -> Result<()> {
        // Count accepted input, not just packets already handed to the mux:
        // this includes every in-flight picture, pre-roll and trimmed AAC tail.
        let mut samples = video + u64::from(repeat);
        for (index, track) in self.audio.iter().enumerate() {
            let frames = audio
                .filter(|(changed, _)| *changed == index)
                .map_or(track.input_frames, |(_, frames)| frames);
            samples += track.packet_budget(frames);
        }
        if samples > mp4::MAX_SAMPLES as u64 {
            bail!("sample index budget exhausted; finish this file and split before another input");
        }
        Ok(())
    }

    fn audio_ticks(&self, pts: Time) -> Result<u64> {
        let start = self
            .start
            .context("audio: start the file before appending")?;
        if pts < start {
            bail!("audio.pts: must not precede the file start; clip upstream");
        }
        u64::try_from(pts.ticks_since(start, RATE)).context("audio.pts: invalid saved timestamp")
    }

    fn append_samples(&mut self, index: usize, samples: &[f32], pts: Time) -> Result<bool> {
        // Validate ALL caller input before pumping, filling gaps, or admitting
        // any PCM. In particular, do not consume the first slice of a large
        // buffer and then return false for the whole buffer.
        let track = self
            .audio
            .get(index)
            .context("audio.track: no such track")?;
        let at = self.audio_ticks(pts)?;
        let end = track.validate(samples, at)?;
        let start = self.start.expect("validated start");
        if i128::from(start.as_nanos()) + i128::from(end) * 1_000_000_000 / i128::from(RATE)
            > i128::from(i64::MAX)
        {
            bail!("audio.samples: endpoint exceeds the master clock");
        }
        if samples.is_empty() {
            return Ok(true);
        }
        self.check_budget(self.accepted, Some((index, end)), true)?;
        self.drain_audio_track(index)?;
        if self.mux.is_none() && self.audio[index].packet_budget(end) > MAX_HELD as u64 {
            return Ok(false);
        }
        // The engine advances its written counter after a false return. Fill
        // that refused interval rather than moving later sound earlier. At
        // most two bounded silence batches and one caller batch enter per
        // call. Recovery must outpace even a maximum-sized caller advancing
        // after false, rather than perpetually chasing the same gap. False
        // ALWAYS means none of this caller's samples were consumed.
        for _ in 0..2 {
            let gap = at - self.audio[index].input_frames;
            if gap == 0 {
                break;
            }
            let fill = gap.min(MAX_BATCH as u64) as usize;
            if !self.audio[index].silence(fill)? {
                return Ok(false);
            }
            self.drain_audio_track(index)?;
        }
        if self.audio[index].input_frames != at {
            return Ok(false);
        }
        let accepted = self.audio[index].submit(samples)?;
        self.drain_audio_track(index)?;
        Ok(accepted)
    }

    fn drain_audio_track(&mut self, index: usize) -> Result<()> {
        // Do not let receive's sticky native error strand the prefix it queued
        // first. Writing errors likewise do not suppress the native drain.
        let mut failure = None;
        if let Err(error) = self.write_audio_ready(index) {
            note_error(&mut failure, error);
        }
        if let Err(error) = self.audio[index].receive_ready() {
            note_error(&mut failure, error);
        }
        if let Err(error) = self.write_audio_ready(index) {
            note_error(&mut failure, error);
        }
        match failure {
            Some(error) => Err(error).with_context(|| format!("audio[{index}]")),
            None => Ok(()),
        }
    }

    fn drain_audio(&mut self) -> Result<()> {
        let mut failure = None;
        for index in 0..self.audio.len() {
            if let Err(error) = self.drain_audio_track(index) {
                note_error(&mut failure, error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn write_audio_ready(&mut self, index: usize) -> Result<()> {
        if let Some(mux) = &mut self.mux {
            let track = &mut self.audio[index];
            while let Some(packet) = track.ready.front() {
                // Keep the queued packet until the mux accepted it. A failed
                // write must not silently remove data from our bookkeeping.
                mux.audio(
                    index,
                    packet.data.clone(),
                    packet.dts_samples,
                    packet.samples,
                )?;
                track.written();
            }
        } else if self.audio_only && !self.audio[index].ready.is_empty() {
            // Terminal-only, bounded batches through the SAME reserved file.
            // The normal A/V path keeps the packet mux's 500 ms publication.
            let track = &self.audio[index];
            let samples: Vec<_> = track
                .ready
                .iter()
                .map(|packet| {
                    (
                        packet.data.len() as u32,
                        packet.dts_samples,
                        packet.samples,
                        true,
                    )
                })
                .collect();
            let data: Vec<_> = track
                .ready
                .iter()
                .flat_map(|packet| packet.data.iter().copied())
                .collect();
            let parts: Vec<_> = (0..self.audio.len())
                .map(|i| mp4::FragmentTrack {
                    samples: if i == index { &samples } else { &[] },
                    data: if i == index { &data } else { &[] },
                })
                .collect();
            self.reserved
                .as_mut()
                .context("audio: reserved file unavailable")?
                .write_fragment(&parts)?;
            self.audio_fragments += 1;
            while !self.audio[index].ready.is_empty() {
                self.audio[index].written();
            }
        }
        Ok(())
    }

    fn configure_audio_only(&mut self) -> Result<()> {
        let delay = self
            .audio
            .iter()
            .map(|track| track.config.priming)
            .max()
            .unwrap_or(0);
        let tracks = self
            .audio
            .iter()
            .map(|track| track.raw_track(self.spec.container, delay))
            .collect::<Result<Vec<_>>>()?;
        self.reserved
            .as_mut()
            .context("audio: reserved file unavailable")?
            .set_tracks(tracks)?;
        self.audio_only = true;
        Ok(())
    }

    fn pad_audio(&mut self, index: usize, target: u64) -> Result<()> {
        self.check_budget(self.accepted, Some((index, target)), false)?;
        let mut deadline = Instant::now() + DRAIN_WAIT;
        while self.audio[index].input_frames < target {
            self.drain_audio_track(index)?;
            let frames = (target - self.audio[index].input_frames).min(MAX_BATCH as u64) as usize;
            if self.audio[index].silence(frames)? {
                deadline = Instant::now() + DRAIN_WAIT;
            } else {
                if Instant::now() >= deadline {
                    bail!(
                        "audio[{index}]: terminal silence admission made no progress within 5 seconds"
                    );
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    fn check_losses(&self) -> Result<()> {
        let lost = self.encoder.dropped();
        if lost != 0 {
            // These providers currently report accepted-input loss only on
            // failure. A count alone cannot identify which FIFO timestamp to
            // remove. Never guess and pair the next output with another input.
            bail!(
                "native video encoder lost {lost} accepted picture(s); cannot safely continue its timestamp FIFO"
            );
        }
        Ok(())
    }

    fn append(&mut self, frame: &Frame, pts: Time, repeated: bool) -> Result<bool> {
        self.drain_ready()?;
        self.drain_audio()?;
        let ticks = self.ticks(pts)?;
        if (frame.width(), frame.height()) != (self.spec.video.width, self.spec.video.height) {
            bail!(
                "video.size: frame is {}x{}, file is {}x{}",
                frame.width(),
                frame.height(),
                self.spec.video.width,
                self.spec.video.height
            );
        }
        if let Some((_, last)) = &self.last_video {
            let previous = self.ticks(*last)?;
            if pts <= *last || ticks <= previous {
                bail!(
                    "video.pts: accepted pictures must have strictly increasing saved timestamps"
                );
            }
            if ticks - previous > u64::from(u32::MAX) {
                bail!("video.pts: gap exceeds a 32-bit sample duration; split the file");
            }
        }
        self.check_budget(self.accepted + 1, None, !repeated)?;
        let now = Instant::now();
        // Audio arrives in 10 ms chunks per track. Files with many tracks
        // leave less room in the bounded route for a stalled video append.
        let wait = if self.audio.len() <= 2 {
            ADMISSION_WAIT
        } else {
            Duration::from_millis(20)
        };
        // Share one wait window across the queued frames catching up after
        // a stall. Once it ends, allow an equally long window without waits;
        // successful appends never reset either deadline.
        let deadline = self
            .admission_wait_until
            .filter(|until| now < *until + wait)
            .unwrap_or(now + wait);
        loop {
            if self.pending.len() < MAX_PENDING
                && self.encoder.try_encode(frame, pts, self.accepted == 0)?
            {
                break;
            }
            // Do not call VideoEncoder::encode: it counts retryable refusals
            // as drops. Only a refusal after our bounded wait reaches the
            // engine's loss counter. Live/streaming admission is unchanged.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            self.admission_wait_until = Some(deadline);
            if let Some(packet) = self
                .encoder
                .receive_timeout(remaining.min(Duration::from_millis(5)))?
            {
                self.collect(packet)?;
                self.drain_ready()?;
            } else {
                // Synchronous MFTs may report NEED_MORE_INPUT immediately;
                // retry busy admission without spinning or moving the deadline.
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(1)),
                );
            }
        }
        self.pending.push_back(Pending { pts, repeated });
        self.accepted += 1;
        self.accepted_originals += u64::from(!repeated);
        self.last_video = Some((frame.clone(), pts));
        Ok(true)
    }

    fn collect(&mut self, packet: EncodedVideo) -> Result<()> {
        self.completed += 1;
        let result = self.collect_inner(packet);
        if result.is_err() {
            // A later inter-picture packet may reference the refused picture.
            // Retain the decodable prefix, not a corrupt continuation of it.
            self.output_failed = true;
        }
        result
    }

    fn collect_inner(&mut self, packet: EncodedVideo) -> Result<()> {
        let pending = self
            .pending
            .front()
            .context("video: encoder returned an unsubmitted picture")?;
        if packet.pts != pending.pts || packet.dts != pending.pts {
            bail!(
                "video.pts/dts: encoder returned reordered or unexpected timestamps (expected {:?}, got {:?}/{:?})",
                pending.pts,
                packet.pts,
                packet.dts
            );
        }
        let repeated = pending.repeated;
        self.pending.pop_front();
        if self.output_failed {
            bail!(
                "video: cannot append dependent pictures after an invalid encoded packet; valid prefix retained"
            );
        }
        let config = self
            .encoder
            .decoder_config()
            .context("video.decoder_config: native encoder has no actual configuration")?;
        // H.264's native packet boundary strips SPS/PPS from the payload, so
        // the mux alone cannot detect a changed description. Check the native
        // config on EVERY packet, and its per-picture parameter sets too (EOS
        // can return several packets after the provider's config has changed).
        if let Some(sets) = &packet.parameter_sets
            && (self.codec != VideoCodec::H264 || crate::h264::config(sets)? != config)
        {
            bail!(
                "video.decoder_config: packet parameter sets disagree with the encoder configuration"
            );
        }
        if let Some(frozen) = &self.decoder_config {
            if *frozen != config {
                bail!(
                    "video.decoder_config: parameter sets changed; start a new file rather than changing its fixed sample entry"
                );
            }
        } else {
            self.configure(config)?;
        }
        let pts = self.ticks(packet.pts)?;
        self.mux
            .as_mut()
            .context("video: packet mux is unavailable")?
            .video(VideoPacket {
                data: packet.data,
                pts,
                dts: pts,
                sync: packet.keyframe,
                framing: Framing::LengthPrefixed,
            })?;
        self.last_written = Some(pts);
        self.repeated += u64::from(repeated);
        Ok(())
    }

    fn drain_ready(&mut self) -> Result<()> {
        // A correct provider returns no more than our eight pending pictures.
        // The extra poll also pumps MF's input-credit events when it is empty.
        for _ in 0..=MAX_PENDING {
            let Some(packet) = self.encoder.receive()? else {
                return self.check_losses();
            };
            self.collect(packet)?;
        }
        bail!("video: encoder exceeded its bounded output queue")
    }

    fn wait_output(&mut self, deadline: Instant) -> Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "video: accepted pictures did not drain within 5 seconds; this provider may require EOS and cannot promise a nonterminal commit"
            );
        }
        if let Some(packet) = self
            .encoder
            .receive_timeout(remaining.min(Duration::from_millis(50)))?
        {
            self.collect(packet)?;
        } else {
            // Synchronous MFTs can return NEED_MORE_INPUT immediately. Avoid
            // spinning, but never extend the caller's five-second deadline.
            std::thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(1)),
            );
        }
        self.check_losses()
    }

    fn drain_accepted(&mut self, deadline: Instant) -> Result<()> {
        self.drain_ready()?;
        while !self.pending.is_empty() {
            self.wait_output(deadline)?;
        }
        Ok(())
    }

    fn commit_packets(&mut self) -> Result<Option<Duration>> {
        if let Some(mux) = &mut self.mux {
            return Ok(mux.commit()?.committed);
        }
        self.reserved
            .as_ref()
            .context("the reserved file is unavailable")?
            .sync()?;
        Ok(Some(Duration::ZERO))
    }

    fn commit_now(&mut self) -> Result<Option<Duration>> {
        self.healthy()?;
        // EOS is terminal in the current native API. A pause/split barrier
        // must leave this encoder usable for the next append.
        let mut failure = None;
        if let Err(error) = self.drain_accepted(Instant::now() + DRAIN_WAIT) {
            note_error(&mut failure, error);
        }
        // AAC can retain partial PCM and 2–3 processing blocks. Pump only
        // normal output, never pad/EOS to manufacture a stronger watermark.
        if let Err(error) = self.drain_audio() {
            note_error(&mut failure, error);
        }
        let committed = self.commit_packets();
        if let Some(error) = failure {
            return match committed {
                Ok(_) => Err(error),
                Err(commit) => Err(error.context(format!(
                    "also could not commit the valid prefix: {commit:#}"
                ))),
            };
        }
        committed
    }

    fn end_ticks(&self, end: Option<Time>) -> Result<u64> {
        let start = self.start.context("video: the file never started")?;
        let (_, last) = self
            .last_video
            .as_ref()
            .context("video: no picture was accepted")?;
        let minimum = self
            .ticks(*last)?
            .checked_add(u64::from(FRAME_DURATION))
            .context("video end overflow")?;
        let Some(end) = end.filter(|end| *end != Time::MAX) else {
            return Ok(minimum);
        };
        if end <= *last {
            bail!(
                "video.end: must be after every accepted picture; complete packets will not be trimmed"
            );
        }
        // Match the engine's file_end: round up to the next frame boundary,
        // using a single division from the origin, not accumulated durations.
        let frames = u64::try_from(end.ticks_since(start, self.spec.video.fps))
            .context("video.end: precedes start")?;
        let mut ticks = frames
            .checked_mul(u64::from(FRAME_DURATION))
            .context("video end overflow")?;
        if self.time_at(ticks)? < end {
            ticks = ticks
                .checked_add(u64::from(FRAME_DURATION))
                .context("video end overflow")?;
        }
        self.time_at(ticks)?;
        if ticks < minimum {
            bail!(
                "video.end: would shorten the last accepted picture; complete packets will not be trimmed"
            );
        }
        Ok(ticks)
    }

    fn repeat_tail(&mut self, end: u64) -> Result<()> {
        let last_tick = end
            .checked_sub(u64::from(FRAME_DURATION))
            .context("video.end: shorter than one frame")?;
        let (frame, last) = self
            .last_video
            .as_ref()
            .context("video: no picture to repeat")?;
        if self.ticks(*last)? >= last_tick {
            return Ok(());
        }
        let frame = frame.clone();
        let pts = self.time_at(last_tick)?;
        let deadline = Instant::now() + DRAIN_WAIT;
        loop {
            if Instant::now() >= deadline {
                bail!("video.end: encoder did not accept the tail repeat within 5 seconds");
            }
            if self.append(&frame, pts, true)? {
                return Ok(());
            }
            self.wait_output(deadline)?;
        }
    }

    fn finish_at(&mut self, end: Option<Time>) -> Result<Finished> {
        // Every exit below either finishes through the SAME mux/lease, or
        // leaves its complete fragments for later exclusive-folder recovery.
        // Drop must not attempt a second adapter drain after an error.
        self.closed = true;
        let mut failure = self.failed.take().map(anyhow::Error::msg);
        let meaningful = !self.empty();
        let requested_audio_end = match end.filter(|end| *end != Time::MAX) {
            Some(end) if meaningful => match self.audio_ticks(end) {
                Ok(end) => Some(end),
                Err(error) => {
                    note_error(&mut failure, error);
                    None
                }
            },
            _ => None,
        };
        let end = match self.end_ticks(end) {
            Ok(end) => Some(end),
            Err(error) => {
                note_error(&mut failure, error);
                None
            }
        };
        if failure.is_none()
            && let Some(end) = end
            && let Err(error) = self.repeat_tail(end)
        {
            note_error(&mut failure, error);
        }
        // Harvest everything already available before EOS. The native finish
        // API returns a vector only on success; pre-draining limits what its
        // error path can strand inside that API.
        if let Err(error) = self.drain_ready() {
            note_error(&mut failure, error);
        }
        match self.encoder.finish() {
            Ok(packets) => {
                for packet in packets {
                    if let Err(error) = self.collect(packet) {
                        note_error(&mut failure, error);
                    }
                }
            }
            Err(error) => {
                note_error(&mut failure, error);
                // Native finish preserves validated packets queued before a
                // later failure. Receive that prefix before closing the mux.
                if let Err(error) = self.drain_ready() {
                    note_error(&mut failure, error);
                }
            }
        }
        if let Err(error) = self.check_losses() {
            note_error(&mut failure, error);
        }
        if !self.pending.is_empty() || self.completed != self.accepted {
            note_error(
                &mut failure,
                anyhow!(
                    "video: accepted {} pictures, received {}, {} timestamps still pending",
                    self.accepted,
                    self.completed,
                    self.pending.len()
                ),
            );
        }
        if let Some(end) = end
            && self
                .last_written
                .and_then(|time| time.checked_add(u64::from(FRAME_DURATION)))
                != Some(end)
        {
            note_error(
                &mut failure,
                anyhow!(
                    "video.end: encoded pictures do not cover the requested end; the actual complete media is retained"
                ),
            );
        }

        // Even without a single picture/description, accepted audio belongs
        // to the caller. Install only its actual AAC tracks on the pinned
        // reserved handle; no fabricated SPS and no path-based reopen/delete.
        if self.mux.is_none()
            && self.audio.iter().any(|track| track.input_frames != 0)
            && let Err(error) = self.configure_audio_only()
        {
            note_error(&mut failure, error);
        }
        if let Err(error) = self.drain_audio() {
            note_error(&mut failure, error);
        }
        let target = if meaningful {
            // Engine ends are already frame-rounded. Keep that same rounding
            // for direct callers; never shorten audio already admitted.
            end.and_then(|end| self.time_at(end).ok())
                .and_then(|end| self.audio_ticks(end).ok())
                .or(requested_audio_end)
                .unwrap_or_else(|| {
                    self.audio
                        .iter()
                        .map(|track| track.input_frames)
                        .max()
                        .unwrap_or(0)
                })
        } else {
            0
        };
        for index in 0..self.audio.len() {
            // Raw capture can admit a chunk before Stop establishes its cut.
            // Keep that accepted PCM within the same one-AAC-packet tolerance
            // as the media contracts, measured from the rounded video end.
            // This is not encoder padding/priming; those are verified below.
            let tail = self.audio[index].input_frames.saturating_sub(target);
            if requested_audio_end.is_some() && tail > audio::BLOCK {
                note_error(
                    &mut failure,
                    anyhow!(
                        "audio[{index}].end: precedes accepted input by {tail} sample frames (maximum AAC tail is {}); complete audio is retained, not trimmed",
                        audio::BLOCK
                    ),
                );
            }
            // Only terminal finish may fill the logical tail. Do not create a
            // stream during empty cancellation or while committing a pause.
            if target > self.audio[index].input_frames
                && (self.mux.is_some() || self.audio_only)
                && let Err(error) = self.pad_audio(index, target)
            {
                note_error(&mut failure, error);
            }
            if let Err(error) = self.drain_audio_track(index) {
                note_error(&mut failure, error);
            }
            let finished = self.audio[index].finish();
            // finish, including a failed finish, leaves its valid queue intact.
            if let Err(error) = self.drain_audio_track(index) {
                note_error(&mut failure, error);
            }
            let verified =
                finished.and_then(|finished| self.audio[index].verify_finished(finished));
            if let Err(error) = verified {
                note_error(&mut failure, error.context(format!("audio[{index}]")));
            }
        }
        let progress = match self.mux.take() {
            Some(mux) => match mux.finish() {
                Ok(progress) => Some(progress),
                Err(error) => {
                    note_error(&mut failure, error.context("could not finish the movie; complete fragments retained for later recovery"));
                    None
                }
            },
            None if self.audio_only => {
                match self.finish_audio_only() {
                    Ok(progress) => Some(progress),
                    Err(error) => {
                        note_error(&mut failure, error.context("could not finish the audio-only movie; complete fragments retained"));
                        None
                    }
                }
            }
            None => {
                note_error(
                    &mut failure,
                    anyhow!(
                        "video: no encoded picture reached a configured movie; partial file retained"
                    ),
                );
                None
            }
        };
        if let Some(progress) = &progress
            && (progress.video_frames != self.accepted
                || progress.accepted_video_frames != self.accepted)
        {
            note_error(
                &mut failure,
                anyhow!(
                    "video: movie contains {} pictures, but {} were accepted",
                    progress.video_frames,
                    self.accepted
                ),
            );
        }
        if let Some(progress) = &progress {
            for (index, track) in self.audio.iter().enumerate() {
                if progress.audio_packets.get(index) != Some(&track.written_packets)
                    || progress.accepted_audio_packets.get(index) != Some(&track.written_packets)
                {
                    note_error(
                        &mut failure,
                        anyhow!(
                            "audio[{index}]: finished movie packet counts disagree with delivered packets"
                        ),
                    );
                }
            }
        }
        if let Some(mut error) = failure {
            if let Some(progress) = progress {
                error = error.context(self.repaired(&progress));
            }
            return Err(error)
                .with_context(|| format!("could not finish {}", self.spec.path.display()));
        }
        Ok(Finished {
            repeated: self.repeated,
            lost_frames: self.take_losses(),
            // A terminal AAC mismatch is an error with actual RepairedFile
            // counts, never success. Refusals were already counted upstream.
            lost_audio_tracks: 0,
        })
    }

    fn finish_audio_only(&mut self) -> Result<Progress> {
        self.reserved
            .take()
            .context("audio: reserved file unavailable")?
            .finish()?;
        let packets: Vec<_> = self
            .audio
            .iter()
            .map(|track| track.written_packets)
            .collect();
        let samples: u64 = packets.iter().sum();
        Ok(Progress {
            video_frames: 0,
            audio_packets: packets.clone(),
            fragments: self.audio_fragments,
            committed: self
                .audio
                .iter()
                .map(|track| {
                    duration(
                        track
                            .written_end
                            .saturating_sub(u64::from(track.config.priming)),
                        RATE,
                    )
                })
                .min(),
            remaining_samples: mp4::MAX_SAMPLES - samples as usize,
            accepted_video_frames: 0,
            accepted_audio_packets: packets,
        })
    }

    fn repaired(&self, progress: &Progress) -> RepairedFile {
        // Unlike the live watermark, a final movie omits empty tracks. Report
        // the shortest KEPT track and every actual duration, not the requested
        // end or a zero contributed by the missing video track.
        let mut shortest: Option<Duration> = None;
        let mut parts = Vec::new();
        if progress.video_frames != 0 {
            let end = duration(
                self.last_written.unwrap_or(0) + u64::from(FRAME_DURATION),
                self.timescale,
            );
            shortest = Some(end);
            parts.push(format!(
                "video {} pictures ({:.6} s)",
                progress.video_frames,
                end.as_secs_f64()
            ));
        } else {
            parts.push("left out: video (no picture)".into());
        }
        for (index, track) in self.audio.iter().enumerate() {
            if progress.audio_packets[index] == 0 {
                parts.push(format!(
                    "left out: audio[{index}] (no packet, {} input frames)",
                    track.input_frames
                ));
                continue;
            }
            let frames = track
                .written_end
                .saturating_sub(u64::from(track.config.priming));
            let end = duration(frames, RATE);
            shortest = Some(shortest.map_or(end, |shortest| shortest.min(end)));
            parts.push(format!(
                "audio[{index}] {} packets ({:.6} s, {} of {} input frames)",
                progress.audio_packets[index],
                end.as_secs_f64(),
                frames,
                track.input_frames
            ));
        }
        RepairedFile {
            fragments: progress.fragments,
            frames: Some(progress.video_frames),
            seconds: shortest.map(|duration| duration.as_secs_f64()),
            summary: parts.join("; "),
        }
    }

    fn take_losses(&mut self) -> u64 {
        // try_encode never counts refusals. Repeats only occur during finish;
        // any failure there returns actual mux counts via RepairedFile instead
        // of attributing an unidentified native loss to an original picture.
        let lost = self.encoder.dropped().min(self.accepted_originals);
        let delta = lost.saturating_sub(self.reported_losses);
        self.reported_losses = lost;
        delta
    }

    fn empty(&self) -> bool {
        self.accepted == 0
            && self.completed == 0
            && self.audio.iter().all(|track| track.input_frames == 0)
            && self.mux.as_ref().is_none_or(|mux| {
                let progress = mux.progress();
                progress.accepted_video_frames == 0
                    && progress
                        .accepted_audio_packets
                        .iter()
                        .all(|count| *count == 0)
            })
    }

    fn cancel_empty(&mut self) -> Result<()> {
        self.closed = true;
        if let Some(mux) = &mut self.mux {
            mux.cancel_empty()
        } else {
            self.reserved
                .take()
                .context("no owned empty file remains")?
                .cancel_empty()
        }
    }

    fn diagnostic(&self, level: &str, message: &str, detail: &str) {
        let detail = format!("{}: {detail}", self.spec.path.display());
        isocast_diagnostics::emit(level, "windows writer", message, detail.clone().into());
        // Direct library callers need the diagnostic even without a logger.
        let _ = writeln!(std::io::stderr(), "Windows writer: {message}: {detail}");
    }
}

impl FileWriter for VideoWriter {
    type Frame = Frame;

    fn prioritize_video(&mut self, canvas: isocast_protocol::CanvasId) {
        self.encoder.prioritize_video(canvas);
    }

    fn start(&mut self, at: Time) -> Result<()> {
        let result = (|| {
            self.healthy()?;
            if self.start.is_some() {
                bail!("video.start: the file timeline can only be started once");
            }
            if at.as_nanos() < 0 {
                bail!("video.start: native Windows encoder timestamps must be nonnegative");
            }
            self.start = Some(at);
            Ok(())
        })();
        self.keep_error(result)
    }

    fn append_video(&mut self, frame: &Frame, pts: Time) -> Result<bool> {
        let result = self.healthy().and_then(|()| self.append(frame, pts, false));
        self.keep_error(result)
    }

    fn skip_video(&mut self) -> Result<()> {
        let result = self
            .healthy()
            .and_then(|()| self.drain_ready())
            .and_then(|()| self.drain_audio());
        self.keep_error(result)
    }

    fn queue_depth(&self) -> usize {
        self.pending.len()
    }

    fn completed_video_frames(&self) -> Option<u64> {
        Some(self.completed)
    }

    fn append_audio(&mut self, track: usize, samples: &[f32], pts: Time) -> Result<bool> {
        let result = self
            .healthy()
            .and_then(|()| self.append_samples(track, samples, pts));
        self.keep_error(result)
    }

    fn finish(mut self, end: Time) -> Result<Finished> {
        self.finish_at(Some(end))
    }

    fn cancel(mut self) {
        if self.empty() {
            if let Err(error) = self.cancel_empty() {
                self.diagnostic(
                    "error",
                    "empty cancellation failed; file retained",
                    &format!("{error:#}"),
                );
            }
        } else {
            let result = self.finish_at(None);
            match result {
                Ok(_) => self.diagnostic(
                    "warn",
                    "refused cancellation after accepted media; file finished and retained",
                    "no media was deleted",
                ),
                Err(error) => self.diagnostic(
                    "error",
                    "refused cancellation after accepted media; file retained",
                    &format!("{error:#}"),
                ),
            }
        }
    }

    fn crash_safe(&self) -> bool {
        true
    }

    fn commit(&mut self) -> Result<Option<Duration>> {
        let result = self.commit_now();
        self.keep_error(result)
    }

    fn committed(&self) -> Option<Duration> {
        match &self.mux {
            Some(mux) => mux.progress().committed,
            None if self.reserved.is_some() => Some(Duration::ZERO),
            None => None,
        }
    }

    fn lost_frames(&mut self) -> u64 {
        self.take_losses()
    }
}

impl Drop for VideoWriter {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if self.empty() {
            if let Err(error) = self.cancel_empty() {
                self.diagnostic(
                    "error",
                    "drop could not cancel empty file; retained",
                    &format!("{error:#}"),
                );
            }
        } else {
            match self.finish_at(None) {
                Ok(_) => self.diagnostic(
                    "warn",
                    "unfinished writer dropped; accepted media finished and retained",
                    "no media was deleted",
                ),
                Err(error) => self.diagnostic(
                    "error",
                    "drop could not finish accepted media; fragments retained",
                    &format!("{error:#}"),
                ),
            }
        }
    }
}

fn note_error(failure: &mut Option<anyhow::Error>, error: anyhow::Error) {
    *failure = Some(match failure.take() {
        Some(first) => first.context(format!("also: {error:#}")),
        None => error,
    });
}

fn duration(ticks: u64, rate: u32) -> Duration {
    let rate = u64::from(rate);
    Duration::new(ticks / rate, ((ticks % rate) * 1_000_000_000 / rate) as u32)
}
