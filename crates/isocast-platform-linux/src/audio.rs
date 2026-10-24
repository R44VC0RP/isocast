//! Bounded streaming AAC-LC at the engine's mix rate, using Apache-2.0
//! libxaac. No external process, FDK or GPL codec dependency.

use anyhow::{Context, Result, bail};
use isocast_mux::AudioConfig;
use xaac_rs::{EncoderConfig, OutputFormat, Profile};

pub(crate) const RATE: u32 = 48_000;
pub(crate) const BLOCK: u64 = 1024;
pub(crate) const MAX_BATCH: usize = 16_384;
// libxaac encoder/ixheaace_rom.h CORE_DELAY_LC, for 1024-sample classic
// AAC-LC without SBR, resampling, or delay adjustment.
const PRIMING: u32 = 1600;

struct NativeEncoder(xaac_rs::Encoder);

// SAFETY: libxaac's state and scratch memory are owned by this one encoder,
// on the heap, and its allocator callbacks use Rust's thread-safe global
// allocator. No thread-affine OS objects or borrowed buffers are retained.
// All native calls require &mut self; this wrapper is Send, never Sync.
unsafe impl Send for NativeEncoder {}

pub(crate) struct Packet {
    pub data: Vec<u8>,
    pub time: u64,
    pub samples: u32,
}

pub(crate) struct AudioTrack {
    encoder: NativeEncoder,
    pub config: AudioConfig,
    pub input_frames: u64,
    pending: Vec<i16>,
    output_frames: u64,
}

/// An AAC-LC encoder at 48 kHz with 1024-sample hops and raw access units
/// (no ADTS), checked to have the AudioSpecificConfig MP4 and FLV assume.
fn native(channels: u32, bitrate: u32) -> Result<NativeEncoder> {
    if !matches!(channels, 1 | 2) {
        bail!("audio.channels: valid values are 1, 2");
    }
    let encoder = xaac_rs::Encoder::new(EncoderConfig {
        profile: Profile::AacLc,
        sample_rate: RATE,
        channels: channels as u16,
        bitrate,
        frame_length: Some(BLOCK as u16),
        output_format: OutputFormat::Raw,
        use_delay_adjustment: false,
        ..EncoderConfig::default()
    })?;
    let asc = ((2u16 << 11) | (3u16 << 7) | ((channels as u16) << 3)).to_be_bytes();
    if encoder.input_frame_bytes() != BLOCK as usize * channels as usize * 2
        || encoder.input_samples_per_channel() != BLOCK as usize
        || encoder.audio_specific_config() != asc
    {
        bail!(
            "audio.decoder_config: expected AAC-LC 48000 Hz with 1024-sample hops and the requested channels"
        );
    }
    Ok(NativeEncoder(encoder))
}

/// Interleaved float samples as 16-bit PCM, non-finite ones as silence.
fn pcm(sample: f32) -> i16 {
    if !sample.is_finite() {
        return 0;
    }
    (sample.clamp(-1.0, 1.0) * 32768.0)
        .round()
        .clamp(-32768.0, 32767.0) as i16
}

/// The live stream's AAC encoder (RTMP): raw AAC-LC access units at the
/// stream's bitrate. Only a partial 1024-sample hop is kept between calls.
pub struct StreamEncoder {
    encoder: NativeEncoder,
    channels: usize,
    pending: Vec<i16>,
}

/// AAC-LC bitrates the stream settings may ask for, in kb/s.
const STREAM_KBPS: std::ops::RangeInclusive<u32> = 32..=320;

impl StreamEncoder {
    pub fn new(spec: &isocast_engine::LiveAudioSpec) -> Result<Self> {
        if !STREAM_KBPS.contains(&spec.bitrate_kbps) {
            bail!(
                "stream.audio_bitrate_kbps: {} is outside AAC-LC's {}-{} kb/s",
                spec.bitrate_kbps,
                STREAM_KBPS.start(),
                STREAM_KBPS.end()
            );
        }
        let encoder = native(spec.channels, spec.bitrate_kbps * 1000)?;
        let channels = spec.channels as usize;
        Ok(Self {
            encoder,
            channels,
            pending: Vec::with_capacity(BLOCK as usize * channels),
        })
    }
}

impl isocast_engine::AudioEncoder for StreamEncoder {
    fn encode(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>> {
        if !samples.len().is_multiple_of(self.channels) {
            bail!("audio.samples: interleaved input must contain whole channel frames");
        }
        let hop = BLOCK as usize * self.channels;
        let mut packets = Vec::with_capacity((self.pending.len() + samples.len()) / hop);
        let mut rest = samples;
        while !rest.is_empty() {
            let take = (hop - self.pending.len()).min(rest.len());
            self.pending.extend(rest[..take].iter().copied().map(pcm));
            rest = &rest[take..];
            if self.pending.len() == hop {
                let packet = self.encoder.0.encode_i16_interleaved(&self.pending)?;
                self.pending.clear();
                if packet.data.is_empty() || packet.data.len() > 8192 {
                    bail!("audio.packet: libxaac did not produce one bounded raw AAC access unit");
                }
                packets.push(packet.data);
            }
        }
        Ok(packets)
    }

    fn delay(&self) -> u32 {
        PRIMING
    }
}

impl AudioTrack {
    pub fn new(channels: u32) -> Result<Self> {
        let bitrate = if channels == 1 { 128_000 } else { 192_000 };
        let encoder = native(channels, bitrate)?;
        Ok(Self {
            encoder,
            config: AudioConfig {
                channels: channels as u16,
                priming: PRIMING,
            },
            input_frames: 0,
            pending: Vec::with_capacity(BLOCK as usize * channels as usize),
            output_frames: 0,
        })
    }

    pub fn packet_budget(&self, frames: u64) -> u64 {
        if frames == 0 {
            0
        } else {
            (frames + u64::from(PRIMING)).div_ceil(BLOCK)
        }
    }

    pub fn validate(&self, samples: &[f32], at: u64) -> Result<u64> {
        let channels = usize::from(self.config.channels);
        if !samples.len().is_multiple_of(channels) {
            bail!("audio.samples: interleaved input must contain whole channel frames");
        }
        let frames = samples.len() / channels;
        if frames > MAX_BATCH {
            bail!(
                "audio.samples: at most {MAX_BATCH} frames per append; split larger chunks upstream"
            );
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            bail!("audio.samples: NaN and infinity are not valid PCM");
        }
        if at < self.input_frames {
            bail!("audio.pts: overlaps accepted audio; clip upstream");
        }
        at.checked_add(frames as u64)
            .context("audio.samples: endpoint overflow")
    }

    /// The caller validates the whole batch before consuming any of it. Only
    /// a partial AAC hop is retained; completed packets must be muxed at once.
    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<Packet>> {
        let channels = usize::from(self.config.channels);
        let hop = BLOCK as usize * channels;
        let mut packets = Vec::new();
        for chunk in samples.chunks(channels) {
            self.pending.extend(chunk.iter().copied().map(pcm));
            self.input_frames += 1;
            if self.pending.len() == hop {
                let packet = self.encoder.0.encode_i16_interleaved(&self.pending)?;
                self.pending.clear();
                packets.push(self.packet(packet, BLOCK as u32)?);
            }
        }
        Ok(packets)
    }

    pub fn silence(&mut self, frames: usize) -> Result<Vec<Packet>> {
        if frames > MAX_BATCH {
            bail!("audio.silence: batch exceeds {MAX_BATCH} frames");
        }
        self.push(&vec![0.0; frames * usize::from(self.config.channels)])
    }

    pub fn finish(&mut self) -> Result<Vec<Packet>> {
        if self.input_frames == 0 {
            return Ok(Vec::new());
        }
        let mut packets = Vec::new();
        if !self.pending.is_empty() {
            self.pending
                .resize(BLOCK as usize * usize::from(self.config.channels), 0);
            let packet = self.encoder.0.encode_i16_interleaved(&self.pending)?;
            self.pending.clear();
            packets.push(self.packet(packet, BLOCK as u32)?);
        }
        // libxaac has no separate drain API. Flush its analysis delay with
        // zero hops (at most two after the partial input hop). Those zeros
        // do not advance input_frames. Trim only the last packet in the mux.
        let end = self.input_frames + u64::from(PRIMING);
        let zeros = vec![0i16; BLOCK as usize * usize::from(self.config.channels)];
        while self.output_frames < end {
            let samples = (end - self.output_frames).min(BLOCK) as u32;
            let packet = self.encoder.0.encode_i16_interleaved(&zeros)?;
            packets.push(self.packet(packet, samples)?);
        }
        Ok(packets)
    }

    fn packet(&mut self, encoded: xaac_rs::EncodedPacket, samples: u32) -> Result<Packet> {
        // Pinned libxaac never populates its legacy i_bytes_consumed field.
        // Classic AAC processes the complete fixed hop per successful call;
        // do not mistake that unused zero field for a dropped hop.
        if encoded.data.is_empty()
            || encoded.data.len() > 8192
            || (encoded.data.len() >= 2
                && encoded.data[0] == 0xff
                && encoded.data[1] & 0xf6 == 0xf0)
        {
            bail!("audio.packet: libxaac did not produce one bounded raw AAC access unit");
        }
        let packet = Packet {
            data: encoded.data,
            time: self.output_frames,
            samples,
        };
        self.output_frames += u64::from(samples);
        Ok(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_hops_flush_once_and_preserve_priming() {
        let mut track = AudioTrack::new(2).unwrap();
        assert!(track.push(&vec![0.0; 480 * 2]).unwrap().is_empty());
        let packets = track.push(&vec![0.0; 700 * 2]).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!((packets[0].time, packets[0].samples), (0, 1024));
        let packets = track.finish().unwrap();
        assert_eq!(packets.len(), 2);
        assert_eq!((packets[0].time, packets[0].samples), (1024, 1024));
        assert_eq!((packets[1].time, packets[1].samples), (2048, 732));
        assert_eq!(track.output_frames, track.input_frames + u64::from(PRIMING));
    }

    #[test]
    fn stream_encoder_keeps_its_bitrate_and_partial_hops() {
        use isocast_engine::{AudioEncoder, LiveAudioSpec};
        let mut encoder = StreamEncoder::new(&LiveAudioSpec {
            channels: 2,
            bitrate_kbps: 160,
        })
        .unwrap();
        assert_eq!(encoder.delay(), PRIMING);
        // Ten seconds of a 440 Hz tone, in uneven batches as the mixer
        // delivers them.
        let tone: Vec<f32> = (0..RATE as usize * 10)
            .flat_map(|n| {
                let v = (n as f32 * 440.0 * std::f32::consts::TAU / RATE as f32).sin() * 0.5;
                [v, v]
            })
            .collect();
        let mut packets = Vec::new();
        for batch in tone.chunks(2 * 480) {
            packets.extend(encoder.encode(batch).unwrap());
        }
        // Whole hops only: 480000 frames make 468 hops, 768 frames kept.
        assert_eq!(packets.len(), 468);
        assert_eq!(encoder.pending.len(), 768 * 2);
        let bytes: usize = packets.iter().map(Vec::len).sum();
        let kbps = bytes as f64 * 8.0 / (468.0 * 1024.0 / f64::from(RATE)) / 1000.0;
        assert!((100.0..=176.0).contains(&kbps), "{kbps:.0} kb/s for 160");
        // No ADTS headers: raw access units, as FLV carries them.
        assert!(
            packets
                .iter()
                .all(|p| !(p[0] == 0xff && p[1] & 0xf6 == 0xf0))
        );
    }

    #[test]
    fn stream_encoder_refuses_bad_settings_and_conceals_bad_samples() {
        use isocast_engine::{AudioEncoder, LiveAudioSpec};
        let spec = |channels, bitrate_kbps| LiveAudioSpec {
            channels,
            bitrate_kbps,
        };
        let error = StreamEncoder::new(&spec(2, 8)).err().unwrap();
        assert!(
            error.to_string().contains("stream.audio_bitrate_kbps"),
            "{error}"
        );
        assert!(StreamEncoder::new(&spec(6, 160)).is_err());
        let mut encoder = StreamEncoder::new(&spec(2, 128)).unwrap();
        assert!(encoder.encode(&[0.0]).is_err());
        let mut bad = vec![0.0f32; 2048];
        bad[10] = f32::NAN;
        bad[11] = f32::INFINITY;
        assert_eq!(encoder.encode(&bad).unwrap().len(), 1);
    }

    #[test]
    fn rejects_invalid_batch_before_consuming_pcm() {
        let track = AudioTrack::new(2).unwrap();
        assert!(track.validate(&[0.0], 0).is_err());
        assert!(track.validate(&[f32::NAN, 0.0], 0).is_err());
        assert!(track.validate(&vec![0.0; (MAX_BATCH + 1) * 2], 0).is_err());
        assert_eq!(track.input_frames, 0);
    }
}
