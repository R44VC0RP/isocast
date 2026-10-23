//! Software H.264/AAC recording through the existing crash-safe packet mux.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{FileWriter, Finished, RepairedFile, Time, VideoEncoder, WriterSpec};
use isocast_mux::{Config, Framing, Progress, VideoCodec, VideoConfig, VideoPacket, mp4};
use isocast_protocol::{Codec, Container};

use crate::audio::{AudioTrack, BLOCK, MAX_BATCH, Packet, RATE};
use crate::encoder::Encoder;
use crate::frames::Frame;

const FRAME_DURATION: u64 = 2000;
const MAX_FINISH_SILENCE: u64 = RATE as u64 * 10;

pub struct Writer {
    spec: WriterSpec,
    encoder: Encoder,
    mux: Option<isocast_mux::Writer>,
    audio: Vec<AudioTrack>,
    start: Option<Time>,
    timescale: u32,
    last_video: Option<(Frame, u64)>,
    completed: u64,
    repeated: u64,
    first_video_packet: Option<Box<dyn FnOnce() + Send>>,
    failed: Option<String>,
}

impl Writer {
    pub fn new(spec: &WriterSpec) -> Result<Self> {
        if !matches!(spec.container, Container::Mov | Container::Mp4) {
            bail!("container: Linux recording supports mov, mp4");
        }
        if spec.video.encoder.codec != Codec::H264 || spec.video.encoder.effective_bit_depth() != 8
        {
            bail!("video.encoder: Linux software recording supports h264 with bit_depth = 8 only");
        }
        if spec.audio.len() > 16 {
            bail!("audio: at most 16 mono/stereo AAC tracks are supported");
        }
        let encoder = Encoder::recording(
            spec.video.width,
            spec.video.height,
            spec.video.fps,
            spec.video.encoder.quality.value(),
        )?;
        let audio = spec
            .audio
            .iter()
            .enumerate()
            .map(|(index, track)| {
                AudioTrack::new(track.channels).with_context(|| format!("audio[{index}]"))
            })
            .collect::<Result<Vec<_>>>()?;
        let timescale = spec.video.fps * FRAME_DURATION as u32;
        // The native description is already known: no deferred video setup,
        // no unbounded queue of AAC waiting for the first picture. Creation is
        // exclusive; a collision never replaces an existing recording.
        let mux = isocast_mux::Writer::create(
            &spec.path,
            spec.container,
            spec.created,
            VideoConfig {
                codec: VideoCodec::H264,
                width: spec.video.width as u16,
                height: spec.video.height as u16,
                timescale,
                frame_duration: FRAME_DURATION as u32,
                decoder_config: encoder.decoder_config()?,
            },
            &audio
                .iter()
                .map(|track| track.config.clone())
                .collect::<Vec<_>>(),
            Config {
                sync_every: Some(Duration::from_secs(1)),
                ..Config::default()
            },
        )
        .with_context(|| {
            format!(
                "could not create {} (any partial file is retained)",
                spec.path.display()
            )
        })?;
        Ok(Self {
            spec: spec.clone(),
            encoder,
            mux: Some(mux),
            audio,
            start: None,
            timescale,
            last_video: None,
            completed: 0,
            repeated: 0,
            first_video_packet: None,
            failed: None,
        })
    }

    fn healthy(&self) -> Result<()> {
        if let Some(error) = &self.failed {
            bail!("file writer failed: {error}");
        }
        if self.mux.is_none() {
            bail!("file writer is finished");
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

    fn ticks(&self, pts: Time, rate: u32) -> Result<u64> {
        let start = self
            .start
            .context("timeline: start the file before appending")?;
        if pts < start {
            bail!("timeline.pts: must not precede the file start; clip upstream");
        }
        u64::try_from(pts.ticks_since(start, rate)).context("timeline.pts: invalid saved timestamp")
    }

    fn time_at(&self, ticks: u64) -> Result<Time> {
        let start = self.start.context("timeline: file has not started")?;
        let nanos = i128::from(start.as_nanos())
            + i128::from(ticks) * 1_000_000_000 / i128::from(self.timescale);
        Ok(Time::from_nanos(
            i64::try_from(nanos).context("timeline: endpoint exceeds master clock")?,
        ))
    }

    fn budget(&self, video: u64, changed_audio: Option<(usize, u64)>, tail: bool) -> Result<()> {
        let mut count = video + u64::from(tail);
        for (index, track) in self.audio.iter().enumerate() {
            let frames = changed_audio
                .filter(|(changed, _)| *changed == index)
                .map_or(track.input_frames, |(_, frames)| frames);
            if frames > 0 {
                count = count
                    .checked_add(track.packet_budget(frames))
                    .context("sample index budget overflow")?;
            }
        }
        if count > mp4::MAX_SAMPLES as u64 {
            bail!("sample index budget exhausted; finish and split before appending more input");
        }
        Ok(())
    }

    fn append_picture(&mut self, frame: &Frame, pts: Time, repeated: bool) -> Result<bool> {
        let ticks = self.ticks(pts, self.timescale)?;
        if self
            .last_video
            .as_ref()
            .is_some_and(|(_, last)| ticks <= *last)
        {
            bail!("video.pts: pictures must arrive in strictly increasing order");
        }
        self.budget(self.completed + 1, None, !repeated)?;
        let dropped = self.encoder.dropped();
        self.encoder.encode(frame, pts, self.last_video.is_none())?;
        let Some(packet) = self.encoder.receive()? else {
            if self.encoder.dropped() > dropped {
                return Ok(false);
            }
            bail!("video.encoder: synchronous encode returned neither a picture nor a drop");
        };
        if packet.pts != pts || packet.dts != pts {
            bail!("video.encoder: reordered or mismatched packet timestamp");
        }
        self.mux
            .as_mut()
            .context("file is finished")?
            .video(VideoPacket {
                data: packet.data,
                pts: ticks,
                dts: ticks,
                sync: packet.keyframe,
                framing: Framing::LengthPrefixed,
            })?;
        self.last_video = Some((frame.clone(), ticks));
        self.completed += 1;
        self.repeated += u64::from(repeated);
        self.publish_first_packet();
        Ok(true)
    }

    fn write_audio(&mut self, index: usize, packets: Vec<Packet>) -> Result<()> {
        let mux = self.mux.as_mut().context("file is finished")?;
        for packet in packets {
            mux.audio(index, packet.data, packet.time, packet.samples)?;
        }
        self.publish_first_packet();
        Ok(())
    }

    fn append_samples(&mut self, index: usize, samples: &[f32], pts: Time) -> Result<bool> {
        let at = self.ticks(pts, RATE)?;
        let end = self
            .audio
            .get(index)
            .context("audio.track: no such track")?
            .validate(samples, at)?;
        if samples.is_empty() {
            return Ok(true);
        }
        let start = self.start.expect("validated start");
        if i128::from(start.as_nanos()) + i128::from(end) * 1_000_000_000 / i128::from(RATE)
            > i128::from(i64::MAX)
        {
            bail!("audio.samples: endpoint exceeds the master clock");
        }
        self.budget(self.completed, Some((index, end)), true)?;
        // A refused call consumes NONE of the caller's samples. Catch up at
        // most two batches of missing time per call; never move later sound
        // earlier or allocate silence proportional to an arbitrary timestamp.
        for _ in 0..2 {
            let gap = at - self.audio[index].input_frames;
            if gap == 0 {
                break;
            }
            let packets = self.audio[index].silence(gap.min(MAX_BATCH as u64) as usize)?;
            self.write_audio(index, packets)?;
        }
        if self.audio[index].input_frames != at {
            return Ok(false);
        }
        let packets = self.audio[index].push(samples)?;
        self.write_audio(index, packets)?;
        Ok(true)
    }

    fn publish_first_packet(&mut self) {
        if self
            .mux
            .as_ref()
            .is_some_and(|mux| mux.progress().video_frames > 0)
            && let Some(notify) = self.first_video_packet.take()
        {
            notify();
        }
    }

    fn finish_tail(&mut self, end: Time) -> Result<()> {
        let start = self.start.context("timeline: file never started")?;
        let minimum = self
            .last_video
            .as_ref()
            .map_or(0, |(_, at)| *at + FRAME_DURATION);
        let audio_end = self
            .audio
            .iter()
            .map(|track| track.input_frames)
            .max()
            .unwrap_or(0);
        let ticks = if end == Time::MAX {
            minimum.max((audio_end * u64::from(self.timescale)).div_ceil(u64::from(RATE)))
        } else {
            if end < start {
                bail!("timeline.end: precedes the file start");
            }
            // Round up once from the origin, never accumulate frame periods.
            let elapsed = i128::from(end.as_nanos()) - i128::from(start.as_nanos());
            let frames = (elapsed * i128::from(self.spec.video.fps) + 999_999_999) / 1_000_000_000;
            u64::try_from(frames)?
                .checked_mul(FRAME_DURATION)
                .context("timeline.end: overflow")?
        };
        self.time_at(ticks)?;
        if ticks < minimum {
            bail!("video.end: would shorten the last accepted picture");
        }
        let target = (u128::from(ticks) * u128::from(RATE) / u128::from(self.timescale)) as u64;
        // Validate every track and the aggregate index budget before padding
        // or repeating anything. A bad end still finalizes the valid prefix.
        let mut count = self.completed;
        let repeat = self
            .last_video
            .as_ref()
            .is_some_and(|(_, at)| *at + FRAME_DURATION < ticks);
        count += u64::from(repeat);
        for track in &self.audio {
            // Capture can admit a chunk just before Stop fixes the cut.
            // Preserve its <=one-packet overhang, matching other writers.
            if track.input_frames.saturating_sub(target) > BLOCK {
                bail!(
                    "audio.end: precedes accepted PCM by more than one AAC packet; complete audio is retained"
                );
            }
            if target.saturating_sub(track.input_frames) > MAX_FINISH_SILENCE {
                bail!(
                    "audio.end: more than 10 seconds of missing tail; refusing unbounded finish work"
                );
            }
            count += track.packet_budget(target.max(track.input_frames));
        }
        if count > mp4::MAX_SAMPLES as u64 {
            bail!("timeline.end: padding would exhaust the sample index budget");
        }
        if repeat {
            let frame = self
                .last_video
                .as_ref()
                .expect("checked last picture")
                .0
                .clone();
            let pts = self.time_at(ticks - FRAME_DURATION)?;
            if !self.append_picture(&frame, pts, true)? {
                bail!("video.end: encoder dropped the final repeated picture");
            }
        }
        for index in 0..self.audio.len() {
            while self.audio[index].input_frames < target {
                let missing = (target - self.audio[index].input_frames).min(MAX_BATCH as u64);
                let packets = self.audio[index].silence(missing as usize)?;
                self.write_audio(index, packets)?;
            }
        }
        Ok(())
    }

    fn repaired(&self, progress: &Progress) -> RepairedFile {
        RepairedFile {
            fragments: progress.fragments,
            frames: Some(progress.video_frames),
            // The live watermark includes empty tracks. Do not misreport it
            // as the shortest track kept by terminal MP4 finalization.
            seconds: None,
            summary: format!(
                "video {} pictures; AAC packets {:?}; incomplete tail retained as a partial take",
                progress.video_frames, progress.audio_packets
            ),
        }
    }
}

impl FileWriter for Writer {
    type Frame = Frame;

    fn start(&mut self, at: Time) -> Result<()> {
        let result = (|| {
            self.healthy()?;
            if self.start.is_some() || at.as_nanos() < 0 {
                bail!("timeline.start: start once with a nonnegative master-clock timestamp");
            }
            self.start = Some(at);
            Ok(())
        })();
        self.keep_error(result)
    }

    fn append_video(&mut self, frame: &Frame, pts: Time) -> Result<bool> {
        let result = self
            .healthy()
            .and_then(|()| self.append_picture(frame, pts, false));
        self.keep_error(result)
    }

    fn append_audio(&mut self, track: usize, samples: &[f32], pts: Time) -> Result<bool> {
        let result = self
            .healthy()
            .and_then(|()| self.append_samples(track, samples, pts));
        self.keep_error(result)
    }

    fn completed_video_frames(&self) -> Option<u64> {
        Some(self.completed)
    }
    fn hardware_accelerated(&self) -> Option<bool> {
        Some(false)
    }

    fn on_first_video_packet(&mut self, notify: Box<dyn FnOnce() + Send>) -> bool {
        self.first_video_packet = Some(notify);
        self.publish_first_packet();
        true
    }

    fn finish(mut self, end: Time) -> Result<Finished> {
        if self.failed.is_none() {
            let result = self.finish_tail(end);
            // Even a timestamp/encoder failure must finalize the packets
            // already saved, through the original handle and folder lease.
            let _ = self.keep_error(result);
        }
        // Even after a bad caller timestamp or video failure, flush every
        // accepted PCM prefix instead of stranding its final partial hop.
        for index in 0..self.audio.len() {
            let result = self.audio[index]
                .finish()
                .and_then(|packets| self.write_audio(index, packets));
            let _ = self.keep_error(result);
        }
        let progress = self
            .mux
            .take()
            .context("file is finished")?
            .finish()
            .with_context(|| {
                format!(
                    "{}: finalization failed; keep the file for fragment recovery",
                    self.spec.path.display()
                )
            })?;
        if let Some(error) = &self.failed {
            return Err(anyhow!(error.clone()).context(self.repaired(&progress)));
        }
        Ok(Finished {
            repeated: self.repeated,
            ..Finished::default()
        })
    }

    fn cancel(mut self) {
        if self.completed != 0 || self.audio.iter().any(|track| track.input_frames != 0) {
            // cancel is only for empty files. Preserve unexpectedly accepted
            // audio/video rather than deleting a take with real media.
            if let Err(error) = self.finish(Time::MAX) {
                eprintln!("Linux writer: cancelled nonempty file retained: {error:#}");
            }
            return;
        }
        // Unix cannot atomically unlink a particular inode by its open
        // descriptor. Do not race a replaced path: retain the empty header
        // rather than risk deleting somebody else's file after a failed start.
        drop(self.mux.take());
        eprintln!(
            "Linux writer: cancelled empty reservation retained: {}",
            self.spec.path.display()
        );
    }

    fn crash_safe(&self) -> bool {
        true
    }

    fn commit(&mut self) -> Result<Option<Duration>> {
        let result = (|| {
            self.healthy()?;
            // Partial PCM stays bounded to one hop; committed() never claims
            // that unencoded tail is recoverable.
            let progress = self.mux.as_mut().context("file is finished")?.commit()?;
            self.publish_first_packet();
            Ok(progress.committed)
        })();
        self.keep_error(result)
    }

    fn committed(&self) -> Option<Duration> {
        self.mux.as_ref().and_then(|mux| mux.progress().committed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isocast_engine::{AudioTrackSpec, FramePool as _, VideoTrackSpec};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::SystemTime;

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn spec() -> WriterSpec {
        let folder = std::env::temp_dir().join(format!(
            "isocast-linux-writer-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&folder).unwrap();
        WriterSpec {
            path: folder.join("take.mp4"),
            container: Container::Mp4,
            video: VideoTrackSpec {
                width: 32,
                height: 32,
                fps: 30,
                encoder: isocast_protocol::Encoder {
                    codec: Codec::H264,
                    ..Default::default()
                },
            },
            audio: vec![AudioTrackSpec { channels: 2 }],
            created: SystemTime::now(),
        }
    }

    #[test]
    fn gaps_tail_audio_and_empty_cancel_retain_owned_files() {
        let spec = spec();
        let mut writer = Writer::new(&spec).unwrap();
        assert!(
            Writer::new(&spec).is_err(),
            "must not overwrite a reservation"
        );
        let mut pool = crate::frames::FramePool::new(32, 32).unwrap();
        let frame = pool
            .draw(&mut |pixels, _| pixels.fill(255))
            .unwrap()
            .unwrap();
        let start = Time::from_nanos(1_000_000_000);
        writer.start(start).unwrap();
        assert!(writer.append_video(&frame, start).unwrap());
        assert!(writer.append_audio(0, &vec![0.0; 1600 * 2], start).unwrap());
        assert!(writer.append_video(&frame, start.add_ticks(2, 30)).unwrap());
        assert_eq!(writer.completed_video_frames(), Some(2));
        assert!(writer.commit().unwrap().is_some());
        let finished = writer.finish(start.add_ticks(4, 30)).unwrap();
        assert_eq!(
            finished,
            Finished {
                repeated: 1,
                ..Finished::default()
            }
        );
        let file =
            mp4::open_for_repair(spec.path.parent().unwrap(), spec.path.file_name().unwrap())
                .unwrap();
        let inspection = mp4::inspect(&file).unwrap().unwrap();
        assert!(inspection.complete);
        assert_eq!(inspection.media.pictures(), Some(3));
        assert!((inspection.media.shortest().unwrap() - 4.0 / 30.0).abs() < 0.0001);
        drop(file);
        let mut empty = spec.clone();
        empty.path = spec.path.with_file_name("empty.mp4");
        Writer::new(&empty).unwrap().cancel();
        assert!(
            empty.path.exists(),
            "Unix cancellation must not unlink a potentially replaced path"
        );
        std::fs::remove_dir_all(spec.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn committed_fragments_survive_unfinished_drop() {
        let spec = spec();
        let mut writer = Writer::new(&spec).unwrap();
        let start = Time::from_nanos(0);
        writer.start(start).unwrap();
        // Accepted audio without a first video picture must still be safe.
        writer.append_audio(0, &vec![0.0; 4096 * 2], start).unwrap();
        writer.commit().unwrap();
        drop(writer);
        let mut file =
            mp4::open_for_repair(spec.path.parent().unwrap(), spec.path.file_name().unwrap())
                .unwrap();
        let recovered = mp4::recover(&mut file).unwrap().unwrap();
        assert!(
            recovered
                .tracks
                .iter()
                .any(|track| !track.video && track.samples == 4)
        );
        assert!(mp4::inspect(&file).unwrap().unwrap().complete);
        drop(file);
        std::fs::remove_dir_all(spec.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn aac_priming_matches_independent_decoded_impulse() {
        let spec = spec();
        let mut writer = Writer::new(&spec).unwrap();
        let mut pool = crate::frames::FramePool::new(32, 32).unwrap();
        let frame = pool.draw(&mut |pixels, _| pixels.fill(0)).unwrap().unwrap();
        let start = Time::from_nanos(0);
        writer.start(start).unwrap();
        for index in 0..30 {
            assert!(
                writer
                    .append_video(&frame, start.add_ticks(index, 30))
                    .unwrap()
            );
        }
        let impulse = 12_000usize;
        let mut samples = vec![0.0; RATE as usize * 2];
        samples[impulse * 2] = 0.8;
        samples[impulse * 2 + 1] = 0.8;
        for (index, chunk) in samples.chunks(1024 * 2).enumerate() {
            assert!(
                writer
                    .append_audio(0, chunk, start.add_ticks(index as u64 * 1024, RATE))
                    .unwrap()
            );
        }
        writer.finish(start.add_ticks(1, 1)).unwrap();
        // Independent decoder: this catches a wrong native delay even when
        // the mux's duration arithmetic is internally self-consistent.
        let decoded = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(&spec.path)
            .args([
                "-map", "0:a:0", "-ac", "1", "-ar", "48000", "-f", "f32le", "-",
            ])
            .output()
            .expect("ffmpeg is required for the Linux codec qualification tests");
        assert!(
            decoded.status.success(),
            "{}",
            String::from_utf8_lossy(&decoded.stderr)
        );
        assert_eq!(decoded.stdout.len() % 4, 0);
        let (peak, value) = decoded
            .stdout
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .map(|(index, bytes)| (index, f32::from_le_bytes(*bytes).abs()))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .expect("decoded PCM");
        assert!(value > 0.1, "impulse was lost: peak {value}");
        assert!(
            peak.abs_diff(impulse) <= 4,
            "AAC priming is wrong: impulse at {peak}, expected {impulse} ±4 samples"
        );
        std::fs::remove_dir_all(spec.path.parent().unwrap()).unwrap();
    }
}
