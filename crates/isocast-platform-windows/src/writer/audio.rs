//! Bounded PCM admission and packet bookkeeping for one native AAC stream.

use std::collections::VecDeque;

use anyhow::{Context, Result, bail};
use isocast_engine::LiveAudioSpec;
use isocast_mux::{AudioConfig, mp4};
use isocast_protocol::Container;

use crate::{AacFinish, AacPacket, MfAacEncoder};

pub(super) const RATE: u32 = 48_000;
pub(super) const BLOCK: u64 = 1024;
// Native admission is atomic only within this batch size. The engine's mixer
// and silence use 480 frames; larger media-reader chunks must respect this cap.
pub(super) const MAX_BATCH: usize = 16_384;
// Includes ALL eventual packets, even the native backlog and terminal tail,
// while the video description is unavailable. No PCM queue is kept here.
pub(super) const MAX_HELD: usize = 32;

pub(super) struct AudioTrack {
    encoder: MfAacEncoder,
    pub config: AudioConfig,
    pub input_frames: u64,
    pub ready: VecDeque<AacPacket>,
    pub written_packets: u64,
    pub written_end: u64,
    received_packets: u64,
    received_end: u64,
    output_failed: bool,
}

impl AudioTrack {
    pub fn new(channels: u32, container: Container) -> Result<Self> {
        let encoder = MfAacEncoder::with_thread_start(
            &LiveAudioSpec {
                channels,
                bitrate_kbps: if channels == 1 { 128 } else { 192 },
            },
            crate::system::audio_thread,
        )?;
        // The mux's AAC helper generates exactly this ASC. Verify the ACTUAL
        // native description before reserving a file, not just our request.
        let asc = ((2u16 << 11) | (3u16 << 7) | ((channels as u16) << 3)).to_be_bytes();
        if encoder.channels() != channels || encoder.decoder_config() != asc.as_slice() {
            bail!("audio.decoder_config: expected raw AAC-LC, 48000 Hz, requested channel layout");
        }
        if encoder.delay() > RATE * 10 {
            bail!("audio.priming: mux supports at most 10 seconds");
        }
        mp4::aac_sample_entry(container, channels as u16, RATE)?;
        let config = AudioConfig {
            channels: channels as u16,
            priming: encoder.delay(),
        };
        Ok(Self {
            encoder,
            config,
            input_frames: 0,
            ready: VecDeque::new(),
            written_packets: 0,
            written_end: 0,
            received_packets: 0,
            received_end: 0,
            output_failed: false,
        })
    }

    pub fn validate(&self, samples: &[f32], at: u64) -> Result<u64> {
        let channels = usize::from(self.config.channels);
        if !samples.len().is_multiple_of(channels) {
            bail!("audio.samples: interleaved input must contain whole channel frames");
        }
        let frames = samples.len() / channels;
        if frames > MAX_BATCH {
            bail!(
                "audio.samples: at most {MAX_BATCH} frames per append; split larger chunks before calling (no input consumed)"
            );
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            bail!("audio.samples: NaN and infinity are not valid PCM");
        }
        if at < self.input_frames {
            bail!("audio.pts: overlaps accepted audio; clip upstream before appending");
        }
        at.checked_add(frames as u64)
            .context("audio.samples: endpoint overflow")
    }

    pub fn packet_budget(&self, frames: u64) -> u64 {
        if frames == 0 {
            0
        } else {
            (frames + u64::from(self.config.priming)).div_ceil(BLOCK)
        }
    }

    /// Caller has checked timeline, data, mux headroom and staging headroom.
    /// A native error can follow admission: account for that input even then,
    /// and retain the file rather than treating the call as a retryable false.
    pub fn submit(&mut self, samples: &[f32]) -> Result<bool> {
        let end = self.input_frames + (samples.len() / usize::from(self.config.channels)) as u64;
        match self.encoder.try_encode(samples) {
            Ok(false) => Ok(false),
            Ok(true) => {
                self.input_frames = end;
                Ok(true)
            }
            Err(error) => {
                // If the worker died, admission is unknown. Conservatively
                // retain the attempted input obligation and surface the error.
                self.input_frames = self.encoder.stats().map_or(end, |s| s.accepted_frames);
                Err(error)
            }
        }
    }

    pub fn silence(&mut self, frames: usize) -> Result<bool> {
        let samples = vec![0.0; frames * usize::from(self.config.channels)];
        self.submit(&samples)
    }

    /// None is only "not ready", never proof that accepted input has drained.
    /// Even after a native failure receive() exposes its valid queued prefix.
    pub fn receive_ready(&mut self) -> Result<()> {
        if self.output_failed {
            bail!("audio: an invalid packet ended this track's valid prefix");
        }
        while self.ready.len() < MAX_HELD {
            let Some(packet) = self.encoder.receive()? else {
                break;
            };
            let end = packet.dts_samples.checked_add(u64::from(packet.samples));
            if packet.dts_samples != self.received_end
                || !(1..=BLOCK as u32).contains(&packet.samples)
                || end.is_none_or(|end| end > self.input_frames + u64::from(self.config.priming))
                || packet.data.is_empty()
                || packet.data.len() > 65_536
                || (packet.data.len() >= 2
                    && packet.data[0] == 0xff
                    && packet.data[1] & 0xf6 == 0xf0)
            {
                self.output_failed = true;
                bail!(
                    "audio.packet: invalid native AAC duration, DTS or raw payload; valid prefix retained"
                );
            }
            self.received_end = end.expect("validated endpoint");
            self.received_packets += 1;
            self.ready.push_back(packet);
        }
        Ok(())
    }

    pub fn written(&mut self) {
        let packet = self.ready.pop_front().expect("persisted queued packet");
        self.written_packets += 1;
        self.written_end = packet.dts_samples + u64::from(packet.samples);
    }

    pub fn finish(&mut self) -> Result<AacFinish> {
        self.encoder.finish()
    }

    /// Called only AFTER terminal finish and receiving/persisting its output.
    /// Processing backlog during a normal append/commit is not a loss.
    pub fn verify_finished(&self, finished: AacFinish) -> Result<()> {
        let expected_end = if self.input_frames == 0 {
            0
        } else {
            self.input_frames + u64::from(self.config.priming)
        };
        let stats = self.encoder.stats()?;
        if finished.input_frames != self.input_frames
            || finished.priming != self.config.priming
            || finished.padding != ((BLOCK - self.input_frames % BLOCK) % BLOCK) as u32
            || finished.packets != self.packet_budget(self.input_frames)
            || finished.packets != self.received_packets
            || finished.packets != self.written_packets
            || self.received_end != expected_end
            || self.written_end != expected_end
            || stats.accepted_frames != self.input_frames
            || stats.buffered_frames != 0
            || stats.partial_frames != 0
            || stats.ready_packets != 0
            || !self.ready.is_empty()
        {
            bail!(
                "audio: terminal count mismatch: {} input frames, {} received packets / {} ticks, {} saved packets / {} ticks; native finish {finished:?}, stats {stats:?}",
                self.input_frames,
                self.received_packets,
                self.received_end,
                self.written_packets,
                self.written_end
            );
        }
        Ok(())
    }

    pub fn raw_track(&self, container: Container, delay: u32) -> Result<mp4::Track> {
        Ok(mp4::Track {
            kind: mp4::TrackKind::Audio {
                channels: self.config.channels,
                sample_rate: RATE,
            },
            timescale: RATE,
            sample_entry: mp4::aac_sample_entry(container, self.config.channels, RATE)?,
            skip: u64::from(self.config.priming),
            fragment_shift: u64::from(delay - self.config.priming),
        })
    }
}
