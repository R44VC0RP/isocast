//! Provider order is explicit and observable: NVENC, hardware MFT, software
//! MFT last. This does not select the Windows platform in the daemon yet.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use isocast_engine::{
    EncodedVideo, LiveVideoSpec, ParameterSets, Time, VideoEncoder, VideoTrackSpec,
};
use isocast_protocol::{Codec, EncoderQuality, QualityPreset};

use crate::{EncodeStats, Frame, GraphicsDevice, MfEncoder, MfMode, NvencEncoder, PixelFormat};
use crate::{h264, hevc};

/// SDR Rec.709 encoding. Main10 preserves a ten-bit input, or expands an
/// eight-bit input on GPU; it does not turn SDR/eight-bit capture into HDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEncoding {
    H264,
    Hevc,
    Hevc10,
}

impl VideoEncoding {
    pub fn bit_depth(self) -> u8 {
        if self == Self::Hevc10 { 10 } else { 8 }
    }

    pub(crate) fn validate(self, input: PixelFormat) -> Result<()> {
        if input == PixelFormat::Rgba10 && self != Self::Hevc10 {
            bail!("video.input: ten-bit RGB requires HEVC Main10; no silent bit-depth reduction");
        }
        Ok(())
    }
}

pub(crate) enum Parameters {
    H264(ParameterSets),
    Hevc { depth: u8, sets: hevc::Parameters },
}

impl Parameters {
    pub(crate) fn new(encoding: VideoEncoding) -> Self {
        match encoding {
            VideoEncoding::H264 => Self::H264(ParameterSets::default()),
            _ => Self::Hevc {
                depth: encoding.bit_depth(),
                sets: hevc::Parameters::default(),
            },
        }
    }

    pub(crate) fn headers(&mut self, bytes: &[u8]) -> Result<()> {
        let nals = h264::annex_b(bytes)?;
        match self {
            Self::H264(sets) => *sets = h264::parameters(&nals),
            Self::Hevc { sets, .. } => {
                let next = hevc::parameters(&nals)?;
                if !sets.vps.is_empty()
                    && !sets.sps.is_empty()
                    && !sets.pps.is_empty()
                    && *sets != next
                {
                    bail!("HEVC parameter sets changed; open a new hvc1 track");
                }
                *sets = next;
            }
        }
        self.config()?;
        Ok(())
    }

    pub(crate) fn config(&self) -> Result<Vec<u8>> {
        match self {
            Self::H264(sets) => h264::config(sets),
            Self::Hevc { depth, sets } => {
                if sets.bit_depth()? != *depth {
                    bail!("encoder returned HEVC with a different bit depth than requested");
                }
                hevc::config(sets)
            }
        }
    }

    pub(crate) fn packet(&mut self, bytes: &[u8], pts: Time) -> Result<Option<EncodedVideo>> {
        match self {
            Self::H264(sets) => h264::packet(bytes, pts, sets),
            Self::Hevc { sets, .. } => hevc::packet(bytes, pts, sets),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncoderBackend {
    Nvenc,
    MediaFoundationHardware(String),
    /// This provider downloads NV12/P010 for a CPU encoder. Never call it a
    /// zero-copy or hardware encoding path in diagnostics.
    MediaFoundationSoftware(String),
}

/// Windows-native recording controls, not VideoToolbox quality or a bitrate
/// estimate. NVENC uses this base QP with spatial AQ; MF uses its own 0–100
/// quality property. Equal labels do not promise equal quality across codecs
/// or providers, and even `incredible` is not lossless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordingQuality {
    pub nvenc_qp: u32,
    pub mf_quality: u32,
}

impl RecordingQuality {
    /// Requested native policy. An opened MF encoder can select a nearby
    /// supported value; use `WindowsVideoEncoder::recording_quality` for it.
    pub fn new(quality: EncoderQuality) -> Result<Self> {
        let value = match quality {
            EncoderQuality::Preset(preset) => match preset {
                QualityPreset::Low => 30,
                QualityPreset::Medium => 50,
                QualityPreset::High => 70,
                QualityPreset::VeryHigh => 85,
                QualityPreset::Incredible => 100,
            },
            EncoderQuality::Value(value) if (1..=100).contains(&value) => u32::from(value),
            EncoderQuality::Value(_) => bail!("encoder.quality: valid numeric range is 1..=100"),
        };
        // Interpolate the explicit native ladder so numeric qualities remain
        // monotonic and agree with named presets at their documented positions.
        let knots = [(1, 51), (30, 30), (50, 26), (70, 22), (85, 18), (100, 14)];
        let mut qp = 14;
        for pair in knots.windows(2) {
            let [(low, worst), (high, best)] = [pair[0], pair[1]];
            if value <= high {
                let span = high - low;
                qp = worst - ((value - low) * (worst - best) + span / 2) / span;
                break;
            }
        }
        Ok(Self {
            nvenc_qp: qp,
            mf_quality: value,
        })
    }
}

enum Encoder {
    Nvenc(NvencEncoder),
    Mf(MfEncoder),
}

pub struct WindowsVideoEncoder {
    encoder: Encoder,
    backend: EncoderBackend,
    fallback_reasons: Vec<String>,
    encoding: VideoEncoding,
    quality: Option<RecordingQuality>,
    delivered: u64,
    held: Option<EncodedVideo>,
    force_idr: bool,
    terminal: bool,
}

impl WindowsVideoEncoder {
    pub fn new(device: &GraphicsDevice, spec: &LiveVideoSpec) -> Result<Self> {
        Self::with_format(device, spec, VideoEncoding::H264, PixelFormat::Bgra8)
    }

    pub fn with_format(
        device: &GraphicsDevice,
        spec: &LiveVideoSpec,
        encoding: VideoEncoding,
        input: PixelFormat,
    ) -> Result<Self> {
        encoding.validate(input)?;
        let mut reasons = Vec::new();
        match NvencEncoder::with_format(device, spec, encoding, input) {
            Ok(encoder) => {
                return Ok(Self {
                    encoder: Encoder::Nvenc(encoder),
                    backend: EncoderBackend::Nvenc,
                    fallback_reasons: reasons,
                    encoding,
                    quality: None,
                    delivered: 0,
                    held: None,
                    force_idr: false,
                    terminal: false,
                });
            }
            Err(error) => reasons.push(format!("NVENC: {error:#}")),
        }
        for mode in [MfMode::Hardware, MfMode::Software] {
            match MfEncoder::with_format(device, spec, mode, encoding, input) {
                Ok(encoder) => {
                    let name = encoder.info().name.clone();
                    let backend = match mode {
                        MfMode::Hardware => EncoderBackend::MediaFoundationHardware(name),
                        MfMode::Software => EncoderBackend::MediaFoundationSoftware(name),
                    };
                    return Ok(Self {
                        encoder: Encoder::Mf(encoder),
                        backend,
                        fallback_reasons: reasons,
                        encoding,
                        quality: None,
                        delivered: 0,
                        held: None,
                        force_idr: false,
                        terminal: false,
                    });
                }
                Err(error) => reasons.push(format!("MF {mode:?}: {error:#}")),
            }
        }
        bail!(
            "encoder: no {encoding:?} provider could open: {}",
            reasons.join("; ")
        )
    }

    /// Open a recording encoder without a guessed average or maximum bitrate.
    /// Provider fallback must preserve quality mode, codec and bit depth.
    pub fn recording(
        device: &GraphicsDevice,
        spec: &VideoTrackSpec,
        input: PixelFormat,
    ) -> Result<Self> {
        let depth = spec.encoder.effective_bit_depth();
        let encoding = match (spec.encoder.codec, depth) {
            (Codec::H264, 8) => VideoEncoding::H264,
            (Codec::Hevc, 8) => VideoEncoding::Hevc,
            (Codec::Hevc, 10) => VideoEncoding::Hevc10,
            _ => bail!(
                "encoder.codec/bit_depth: Windows recording supports h264/8, hevc/8 or hevc/10"
            ),
        };
        encoding.validate(input)?;
        let quality = RecordingQuality::new(spec.encoder.quality)?;
        let settings = LiveVideoSpec {
            width: spec.width,
            height: spec.height,
            fps: spec.fps,
            bitrate_kbps: 0, // Deliberately unused by quality-mode constructors.
            constant_bitrate: false,
            keyframe_interval: spec
                .fps
                .checked_mul(2)
                .context("video.fps: GOP length overflow")?,
            low_latency: true,
        };
        let mut reasons = Vec::new();
        match NvencEncoder::with_quantizer(device, &settings, encoding, input, quality.nvenc_qp) {
            Ok(encoder) => {
                return Ok(Self {
                    encoder: Encoder::Nvenc(encoder),
                    backend: EncoderBackend::Nvenc,
                    fallback_reasons: reasons,
                    encoding,
                    quality: Some(quality),
                    delivered: 0,
                    held: None,
                    force_idr: true,
                    terminal: false,
                });
            }
            Err(error) => reasons.push(format!("NVENC recording QP: {error:#}")),
        }
        for mode in [MfMode::Hardware, MfMode::Software] {
            match MfEncoder::with_quality(
                device,
                &settings,
                mode,
                encoding,
                input,
                quality.mf_quality,
            ) {
                Ok(encoder) => {
                    let quality = RecordingQuality {
                        mf_quality: encoder.info().quality.context(
                            "MF recording quality: provider did not report selected quality",
                        )?,
                        ..quality
                    };
                    let name = encoder.info().name.clone();
                    let backend = match mode {
                        MfMode::Hardware => EncoderBackend::MediaFoundationHardware(name),
                        MfMode::Software => EncoderBackend::MediaFoundationSoftware(name),
                    };
                    return Ok(Self {
                        encoder: Encoder::Mf(encoder),
                        backend,
                        fallback_reasons: reasons,
                        encoding,
                        quality: Some(quality),
                        delivered: 0,
                        held: None,
                        force_idr: true,
                        terminal: false,
                    });
                }
                Err(error) => reasons.push(format!("MF {mode:?} recording quality: {error:#}")),
            }
        }
        bail!(
            "encoder: no {encoding:?} recording-quality provider could open: {}",
            reasons.join("; ")
        )
    }

    /// Native recording controls: `mf_quality` is the actual selected value
    /// for MF, not necessarily `RecordingQuality::new`'s requested value.
    /// NVENC uses `nvenc_qp`; the unused provider's field retains its policy
    /// value. Raw `MfEncoder::info()` exposes requested and actual MF quality.
    pub fn recording_quality(&self) -> Option<RecordingQuality> {
        self.quality
    }

    pub fn backend(&self) -> &EncoderBackend {
        &self.backend
    }
    pub fn encoding(&self) -> VideoEncoding {
        self.encoding
    }
    pub fn fallback_reasons(&self) -> &[String] {
        &self.fallback_reasons
    }
    pub fn decoder_config(&self) -> Result<Vec<u8>> {
        match &self.encoder {
            Encoder::Nvenc(encoder) => encoder.decoder_config(),
            Encoder::Mf(encoder) => encoder.decoder_config(),
        }
    }
    pub fn try_encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<bool> {
        let keyframe = keyframe || self.force_idr;
        let accepted = match &mut self.encoder {
            Encoder::Nvenc(encoder) => encoder.try_encode(frame, pts, keyframe)?,
            Encoder::Mf(encoder) => encoder.try_encode(frame, pts, keyframe)?,
        };
        if accepted {
            self.force_idr = false;
        }
        Ok(accepted)
    }
    pub fn finish(&mut self) -> Result<Vec<EncodedVideo>> {
        self.terminal = true;
        let mut packets = match &mut self.encoder {
            Encoder::Nvenc(encoder) => encoder.finish(),
            Encoder::Mf(encoder) => encoder.finish(),
        }?;
        if let Some(packet) = self.held.take() {
            packets.insert(0, packet);
        }
        self.delivered += packets.len() as u64;
        Ok(packets)
    }
    pub fn stats(&self) -> Result<EncodeStats> {
        let mut stats = match &self.encoder {
            Encoder::Nvenc(encoder) => Ok(encoder.stats()),
            Encoder::Mf(encoder) => encoder.stats(),
        }?;
        stats.ready_packets += usize::from(self.held.is_some());
        Ok(stats)
    }

    /// Persist output for every input accepted before this call, without EOS,
    /// DRAIN, FLUSH or a session reset. Each packet is acknowledged separately;
    /// a callback error retains that packet for retry/receive. The callback must
    /// return Ok only after taking responsibility for its bytes. Native waits
    /// and the callback itself are not forcibly interrupted by this timeout.
    pub fn barrier(
        &mut self,
        timeout: Duration,
        mut persist: impl FnMut(&EncodedVideo) -> Result<()>,
    ) -> Result<()> {
        let target = self.stats()?.accepted;
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("video.barrier.timeout: too large")?;
        while self.delivered < target {
            if self.held.is_none() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                self.held = match &mut self.encoder {
                    Encoder::Nvenc(encoder) => encoder.receive_timeout(remaining)?,
                    Encoder::Mf(encoder) => encoder.receive_timeout(remaining)?,
                };
            }
            if let Some(packet) = &self.held {
                persist(packet)?;
                self.held = None;
                self.delivered += 1;
            } else {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    bail!(
                        "video.barrier: timed out with {} of {target} accepted pictures delivered; session is not finished",
                        self.delivered
                    );
                }
                std::thread::sleep(remaining.min(Duration::from_millis(1)));
            }
        }
        Ok(())
    }

    /// Reuse a drained recording session for a new file. Input PTS must keep
    /// increasing across files; rebase only at the mux. Force an IDR on the
    /// next accepted picture without changing codec configuration or quality.
    pub fn begin_segment(&mut self) -> Result<()> {
        if self.terminal {
            bail!("video.segment: finish is terminal; open a new recording encoder");
        }
        if self.quality.is_none() {
            bail!("video.segment: requires a recording-quality encoder");
        }
        let stats = self.stats()?;
        if stats.accepted != self.delivered || self.held.is_some() {
            bail!(
                "video.segment: persist all accepted output with barrier before opening a new segment"
            );
        }
        self.force_idr = true;
        Ok(())
    }
}

impl VideoEncoder for WindowsVideoEncoder {
    type Frame = Frame;
    fn prioritize_video(&mut self, canvas: isocast_protocol::CanvasId) {
        match &mut self.encoder {
            Encoder::Nvenc(encoder) => encoder.prioritize_video(canvas),
            Encoder::Mf(encoder) => encoder.prioritize_video(canvas),
        }
    }
    fn encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<()> {
        let before = if self.force_idr {
            Some(self.stats()?.accepted)
        } else {
            None
        };
        let keyframe = keyframe || self.force_idr;
        match &mut self.encoder {
            Encoder::Nvenc(encoder) => encoder.encode(frame, pts, keyframe)?,
            Encoder::Mf(encoder) => encoder.encode(frame, pts, keyframe)?,
        };
        if let Some(before) = before
            && self.stats()?.accepted > before
        {
            self.force_idr = false;
        }
        Ok(())
    }
    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        self.receive_timeout(Duration::ZERO)
    }
    fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<EncodedVideo>> {
        let packet = if let Some(packet) = self.held.take() {
            Some(packet)
        } else {
            match &mut self.encoder {
                Encoder::Nvenc(encoder) => encoder.receive_timeout(timeout),
                Encoder::Mf(encoder) => encoder.receive_timeout(timeout),
            }?
        };
        self.delivered += u64::from(packet.is_some());
        Ok(packet)
    }
    fn dropped(&self) -> u64 {
        match &self.encoder {
            Encoder::Nvenc(encoder) => encoder.dropped(),
            Encoder::Mf(encoder) => encoder.dropped(),
        }
    }
    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) -> Result<()> {
        match &mut self.encoder {
            Encoder::Nvenc(encoder) => encoder.set_rate(fps, bitrate_kbps),
            Encoder::Mf(encoder) => encoder.set_rate(fps, bitrate_kbps),
        }
    }
}
