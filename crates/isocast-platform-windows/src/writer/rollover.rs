//! A physical movie owns its own zero, AAC encoders and sample entries.
//! The logical file worker and all incoming timestamps remain unchanged.
use super::*;
use isocast_engine::record::recovery::{OutputRecovery, PartFacts};
use std::path::PathBuf;

#[derive(Debug)]
pub(super) struct PartStorage;
impl std::fmt::Display for PartStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("output part storage")
    }
}

#[derive(Default)]
pub(super) struct Parts {
    pub changed: Option<Vec<u8>>,
    staged: VecDeque<(usize, Vec<f32>, Time)>,
    previous: Option<Box<VideoWriter>>,
    prefix_end: Option<Time>,
    root: Option<PathBuf>,
    index: u32, // zero-based
    context: Option<OutputRecovery>,
    pub commitment_limit: Option<Duration>,
    pub earlier_pictures: u64,
}

impl VideoWriter {
    pub(super) fn rotate(&mut self, cut: Time, config: Vec<u8>) -> Result<Time> {
        let context = self
            .recovery
            .context
            .clone()
            .context("missing rollover context")?;
        let ticket = self.recovery.attempt.context("missing rollover ticket")?;
        self.drain_audio()?;
        self.commit_packets()?; // seal available pre-cut AAC as well as the picture prefix
        let root = self
            .parts
            .root
            .clone()
            .unwrap_or_else(|| self.spec.path.clone());
        let index = self.parts.index + 1;
        let audio = Self::audio_tracks(&self.spec)?;
        let stem = root
            .file_stem()
            .and_then(|s| s.to_str())
            .context("part stem")?;
        let ext = root
            .extension()
            .and_then(|s| s.to_str())
            .context("part extension")?;
        let mux = self
            .mux
            .as_ref()
            .context("rollover needs a committed prefix")?;
        let mut collision = 0;
        let (name, reservation) = loop {
            let suffix = if collision == 0 {
                String::new()
            } else {
                format!(" ({collision})")
            };
            let name = format!("{stem}.part{:03}{suffix}.{ext}", index + 1);
            match mux.reserve_sibling(name.as_ref()) {
                Ok(file) => break (name, file),
                Err(e)
                    if collision < 100
                        && e.chain().any(|c| {
                            c.downcast_ref::<std::io::Error>()
                                .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
                        }) =>
                {
                    collision += 1
                }
                Err(e) => return Err(e.context(PartStorage)),
            }
        };
        let spec = WriterSpec {
            path: root.with_file_name(&name),
            ..self.spec.clone()
        };
        let mut next = Self::part(&spec, self.codec, self.timescale, reservation, audio);
        next.configure(config)?;
        next.start = Some(cut);
        #[cfg(debug_assertions)]
        self.crash_at_reservation("orphan");
        // Retain even an empty reservation on ambiguous publication failure.
        // No packet is written before the manifest writer acknowledges sync.
        next.closed = true;
        self.parts.context = Some(context.clone()); // preserve original facts on publication failure
        let encoder = self
            .encoder
            .as_ref()
            .context("missing replacement encoder")?;
        let intent = context.reserve_part(
            ticket,
            name,
            cut,
            crate::now(),
            format!("{:?}", encoder.backend()),
            encoder.nvenc_preset(),
        )?;
        let checked_at = intent.admit(index + 1, crate::now(), || {
            #[cfg(debug_assertions)]
            self.crash_at_reservation("acknowledged");
            crate::now()
        })?;
        next.closed = false;
        next.parts.index = index;
        next.parts.root = Some(root);
        next.parts.earlier_pictures = self.parts.earlier_pictures + self.completed;
        // The original all-tracks prefix remains safe across a physical
        // picture gap; a later part's local watermark cannot replace it.
        next.parts.commitment_limit = self.committed();
        next.parts.context = Some(context);
        self.parts.prefix_end = Some(cut);
        self.parts.changed = None;
        next.parts.staged = std::mem::take(&mut self.parts.staged);
        next.encoder = self.encoder.take();
        next.recovery = std::mem::take(&mut self.recovery);
        next.losses = std::mem::take(&mut self.losses);
        next.pending = std::mem::take(&mut self.pending);
        next.accepted = next.pending.len() as u64;
        next.accepted_originals = next.pending.iter().filter(|p| !p.repeated).count() as u64;
        self.accepted -= next.accepted;
        self.accepted_originals -= next.accepted_originals;
        next.last_video = self.last_video.take();
        let old = std::mem::replace(self, next);
        self.parts.previous = Some(Box::new(old));
        Ok(checked_at)
    }

    #[cfg(debug_assertions)]
    fn crash_at_reservation(&self, stage: &str) {
        if self
            .recovery
            .recipe
            .as_ref()
            .is_some_and(|r| r.changed_profile)
            && isocast_diagnostics::developer()
            && std::env::var("ISOCAST_TEST_PART_CRASH").as_deref() == Ok(stage)
        {
            // No Rust drops or recording finalizers. The harness checks the
            // exact header and published document before it starts recovery.
            std::process::exit(86);
        }
    }

    pub(super) fn flush_staged(&mut self) -> Result<()> {
        while let Some((i, samples, pts)) = self.parts.staged.pop_front() {
            self.route_required(i, &samples, pts)?;
        }
        Ok(())
    }
    fn route_required(&mut self, i: usize, samples: &[f32], pts: Time) -> Result<()> {
        // Caller already owns this PCM. A false after accepting part of a
        // straddling packet must never be returned as a retry of the whole.
        let deadline = Instant::now() + ADMISSION_WAIT;
        while !self.route_audio(i, samples, pts)? {
            anyhow::ensure!(
                Instant::now() < deadline,
                "audio[{i}]: rollover could not admit {} interleaved samples at {pts:?}",
                samples.len()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }
    pub(super) fn route_audio(&mut self, i: usize, samples: &[f32], pts: Time) -> Result<bool> {
        if self.parts.changed.is_some() {
            let ch = self
                .audio
                .get(i)
                .context("unknown rollover audio track")?
                .config
                .channels as usize;
            anyhow::ensure!(
                samples.len().is_multiple_of(ch) && samples.len() / ch <= MAX_BATCH,
                "invalid rollover PCM block"
            );
            let held: usize = self
                .parts
                .staged
                .iter()
                .filter(|(t, ..)| *t == i)
                .map(|(_, s, _)| s.len() / ch)
                .sum();
            if held + samples.len() / ch > RATE as usize || self.parts.staged.len() >= 2048 {
                self.parts.changed = None;
                self.flush_staged()?;
                self.route_required(i, samples, pts)?;
                bail!(
                    "rollover: no verified IDR within the bounded PCM handoff; old audio retained"
                );
            }
            self.parts.staged.push_back((i, samples.to_vec(), pts));
            return Ok(true);
        }
        let Some(old) = self.parts.previous.as_mut() else {
            return self.append_samples(i, samples, pts);
        };
        let cut = self.start.expect("part started");
        let ch = self
            .audio
            .get(i)
            .context("unknown rollover audio track")?
            .config
            .channels as usize;
        anyhow::ensure!(
            samples.len().is_multiple_of(ch)
                && samples.len() / ch <= MAX_BATCH
                && samples.iter().all(|s| s.is_finite()),
            "invalid straddling PCM block"
        );
        let before = (cut.ticks_since(pts, RATE).max(0) as usize).min(samples.len() / ch);
        if before > 0 {
            old.route_required(i, &samples[..before * ch], pts)?;
        }
        let after = &samples[before * ch..];
        if !after.is_empty() {
            let at = if before > 0 { cut } else { pts.max(cut) };
            let deadline = Instant::now() + ADMISSION_WAIT;
            while !self.append_samples(i, after, at)? {
                if before == 0 {
                    return Ok(false);
                }
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "audio[{i}]: could not admit samples after the rollover boundary"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        self.finish_prior(false)?;
        Ok(true)
    }
    pub(super) fn finish_prior(&mut self, force: bool) -> Result<()> {
        let Some(old) = self.parts.previous.as_ref() else {
            return Ok(());
        };
        let cut = old.parts.prefix_end.expect("closing prefix");
        let end = old.audio_ticks(cut)?;
        if !force && old.audio.iter().any(|a| a.input_frames < end) {
            return Ok(());
        }
        let mut old = self.parts.previous.take().expect("checked");
        old.finish_at(Some(cut))
            .context("finishing the pre-rollover movie")?;
        Ok(())
    }

    pub(super) fn prefix_end(&self) -> Option<Time> {
        self.parts.prefix_end
    }
    pub(super) fn part_facts(&self, progress: &Progress, end: Option<Time>) {
        let Some(context) = &self.parts.context else {
            return;
        };
        let video = self.last_written.map_or(Duration::ZERO, |t| {
            duration(t + u64::from(FRAME_DURATION), self.timescale)
        });
        let longest = self
            .audio
            .iter()
            .map(|t| duration(t.written_end.saturating_sub(t.config.priming as u64), RATE))
            .fold(video, Duration::max);
        let slots = end
            .filter(|t| *t != Time::MAX)
            .zip(self.start)
            .map_or(progress.video_frames, |(end, start)| {
                end.ticks_since(start, self.spec.video.fps).max(0) as u64
            });
        context.finish_part(
            self.parts.index + 1,
            PartFacts {
                frames: progress.video_frames,
                repeated: self.repeated,
                dropped: slots.saturating_sub(progress.video_frames.saturating_sub(self.repeated)),
                duration: longest,
                video,
            },
        );
    }
}
