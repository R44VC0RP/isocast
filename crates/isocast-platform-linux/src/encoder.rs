//! Synchronous, bounded software H.264. No external process or GPL codec.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_engine::{EncodedVideo, LiveVideoSpec, ParameterSets, Time, VideoEncoder};
use openh264::encoder::{
    BitRate, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, QpRange,
    RateControlMode, VuiConfig,
};
use openh264::formats::YUVSource;

use crate::frames::Frame;

pub struct Encoder {
    native: openh264::encoder::Encoder,
    spec: LiveVideoSpec,
    pixels: I420,
    parameters: ParameterSets,
    ready: Option<EncodedVideo>,
    last: Option<Time>,
    dropped: u64,
}

impl Encoder {
    pub fn new(spec: &LiveVideoSpec) -> Result<Self> {
        if spec.constant_bitrate {
            bail!("video.constant_bitrate: Linux software H.264 supports average bitrate only");
        }
        Self::configured(spec, None)
    }

    pub(crate) fn recording(width: u32, height: u32, fps: u32, quality: u8) -> Result<Self> {
        if !(1..=100).contains(&quality) {
            bail!("video.encoder.quality: valid range is 1..=100");
        }
        // OpenH264 quality mode with a fixed quantizer range, not a guessed
        // recording bitrate. The generous rate ceiling cannot enable skips.
        // This is a software-specific ladder, not VideoToolbox quality parity.
        let qp = 38 - (u32::from(quality) * 22 / 100) as u8;
        Self::configured(
            &LiveVideoSpec {
                width,
                height,
                fps,
                bitrate_kbps: 100_000,
                constant_bitrate: false,
                keyframe_interval: fps.saturating_mul(2),
                low_latency: true,
            },
            Some(qp),
        )
    }

    fn configured(spec: &LiveVideoSpec, qp: Option<u8>) -> Result<Self> {
        if spec.width < 16
            || spec.height < 16
            || !spec.width.is_multiple_of(2)
            || !spec.height.is_multiple_of(2)
            || spec.width.max(spec.height) > 3840
            || spec.width.min(spec.height) > 2160
        {
            bail!(
                "video.width/height: OpenH264 requires even dimensions from 16 up to 3840x2160 (or portrait)"
            );
        }
        if !(1..=60).contains(&spec.fps) {
            bail!("video.fps: Linux software H.264 supports 1..=60");
        }
        if !(1..=100_000).contains(&spec.bitrate_kbps) || spec.keyframe_interval == 0 {
            bail!("video: bitrate_kbps must be 1..=100000 and keyframe_interval must be positive");
        }
        let mut config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(spec.bitrate_kbps * 1000))
            .max_frame_rate(FrameRate::from_hz(spec.fps as f32))
            .intra_frame_period(IntraFramePeriod::from_num_frames(spec.keyframe_interval))
            .profile(Profile::High)
            .vui(VuiConfig::bt709())
            .skip_frames(false)
            // Program plus ISOs must not each spawn one thread per CPU.
            .num_threads(1)
            .rate_control_mode(RateControlMode::Bitrate);
        if let Some(qp) = qp {
            config = config
                .rate_control_mode(RateControlMode::Quality)
                .qp(QpRange::new(qp, qp))
                .adaptive_quantization(false);
        }
        let mut native = openh264::encoder::Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            config,
        )?;
        let pixels = I420::new(spec.width as usize, spec.height as usize);
        // OpenH264 initializes lazily. Probe a black picture now, before the
        // output path is reserved, so invalid native settings fail at creation
        // and audio never waits for a video description. Discard this picture;
        // the first caller picture is forced to IDR and has its caller's PTS.
        let packet = packet(&native.encode(&pixels)?, Time::from_nanos(0))?;
        let parameters = packet
            .parameter_sets
            .context("OpenH264 did not provide SPS/PPS at initialization")?;
        avcc(&parameters)?;
        Ok(Self {
            native,
            spec: *spec,
            pixels,
            parameters,
            ready: None,
            last: None,
            dropped: 0,
        })
    }

    pub(crate) fn decoder_config(&self) -> Result<Vec<u8>> {
        avcc(&self.parameters)
    }
}

impl VideoEncoder for Encoder {
    type Frame = Frame;

    fn encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<()> {
        if frame.width() != self.spec.width || frame.height() != self.spec.height {
            bail!("video.frame: dimensions must match the encoder's configured size");
        }
        if pts.as_nanos() < 0 || self.last.is_some_and(|last| pts <= last) {
            bail!("video.pts: timestamps must be nonnegative and strictly increasing");
        }
        if self.ready.is_some() {
            self.dropped += 1;
            return Ok(());
        }
        self.pixels.read_bgra(&frame.read_bgra()?)?;
        if keyframe || self.last.is_none() {
            self.native.force_intra_frame();
        }
        let stream = self.native.encode_at(
            &self.pixels,
            openh264::Timestamp::from_millis(pts.as_nanos() as u64 / 1_000_000),
        )?;
        self.last = Some(pts);
        if stream.frame_type() == FrameType::Skip {
            self.dropped += 1;
            return Ok(());
        }
        let mut encoded = packet(&stream, pts)?;
        if let Some(parameters) = &encoded.parameter_sets {
            if *parameters != self.parameters {
                bail!("video: OpenH264 changed SPS/PPS; a new encoder/file is required");
            }
        } else if encoded.keyframe {
            encoded.parameter_sets = Some(self.parameters.clone());
        }
        self.ready = Some(encoded);
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        Ok(self.ready.take())
    }

    fn receive_timeout(&mut self, _timeout: Duration) -> Result<Option<EncodedVideo>> {
        // Encoding completes synchronously; there is no worker to wait for.
        self.receive()
    }

    fn dropped(&self) -> u64 {
        self.dropped
    }

    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) -> Result<()> {
        if fps == self.spec.fps && bitrate_kbps == self.spec.bitrate_kbps {
            return Ok(());
        }
        // The safe wrapper has no in-place rate API. Do not silently replace
        // the session/insert an IDR in violation of VideoEncoder::set_rate.
        bail!(
            "video.rate: Linux software encoder cannot change rate in place; create a new encoder"
        )
    }
}

fn packet(stream: &openh264::encoder::EncodedBitStream<'_>, pts: Time) -> Result<EncodedVideo> {
    let mut data = Vec::new();
    let mut parameters = ParameterSets::default();
    let mut picture = false;
    let mut keyframe = false;
    for index in 0..stream.num_layers() {
        let layer = stream
            .layer(index)
            .context("OpenH264 returned a missing layer")?;
        for index in 0..layer.nal_count() {
            let bytes = layer
                .nal_unit(index)
                .context("OpenH264 returned a missing NAL")?;
            let nal = bytes
                .strip_prefix(&[0, 0, 0, 1])
                .or_else(|| bytes.strip_prefix(&[0, 0, 1]))
                .context("OpenH264 returned a NAL without a start code")?;
            let kind = nal.first().context("OpenH264 returned an empty NAL")? & 31;
            match kind {
                7 => parameters.sps.push(nal.to_vec()),
                8 => parameters.pps.push(nal.to_vec()),
                _ => {
                    picture |= matches!(kind, 1..=5);
                    keyframe |= kind == 5;
                    data.extend_from_slice(&u32::try_from(nal.len())?.to_be_bytes());
                    data.extend_from_slice(nal);
                }
            }
        }
    }
    if !picture {
        bail!("OpenH264 returned no coded picture");
    }
    Ok(EncodedVideo {
        data,
        pts,
        dts: pts,
        keyframe,
        parameter_sets: (!parameters.sps.is_empty() || !parameters.pps.is_empty())
            .then_some(parameters),
    })
}

fn avcc(parameters: &ParameterSets) -> Result<Vec<u8>> {
    let sps = parameters.sps.first().context("video: missing SPS")?;
    if sps.len() < 4
        || parameters.sps.len() > 31
        || parameters.pps.is_empty()
        || parameters.pps.len() > 255
    {
        bail!("video: invalid SPS/PPS configuration");
    }
    let mut config = vec![
        1,
        sps[1],
        sps[2],
        sps[3],
        0xff,
        0xe0 | parameters.sps.len() as u8,
    ];
    for nal in &parameters.sps {
        config.extend_from_slice(&u16::try_from(nal.len())?.to_be_bytes());
        config.extend_from_slice(nal);
    }
    config.push(parameters.pps.len() as u8);
    for nal in &parameters.pps {
        config.extend_from_slice(&u16::try_from(nal.len())?.to_be_bytes());
        config.extend_from_slice(nal);
    }
    // High-profile extension: 4:2:0, 8-bit luma/chroma, no SPS extensions.
    if matches!(sps[1], 100 | 110 | 122 | 144) {
        config.extend_from_slice(&[0xfd, 0xf8, 0xf8, 0]);
    }
    Ok(config)
}

struct I420 {
    width: usize,
    height: usize,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl I420 {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            y: vec![16; width * height],
            u: vec![128; width * height / 4],
            v: vec![128; width * height / 4],
        }
    }

    fn read_bgra(&mut self, bgra: &[u8]) -> Result<()> {
        if bgra.len() != self.width * self.height * 4 {
            bail!("video.frame: expected tightly packed BGRA pixels");
        }
        // Limited-range Rec.709; average each 2x2 chroma footprint. The
        // wrapper's convenience RGB conversion uses a different matrix.
        for row in (0..self.height).step_by(2) {
            for col in (0..self.width).step_by(2) {
                let (mut r_sum, mut g_sum, mut b_sum) = (0, 0, 0);
                for dy in 0..2 {
                    for dx in 0..2 {
                        let offset = (row + dy) * self.width + col + dx;
                        let b = i32::from(bgra[offset * 4]);
                        let g = i32::from(bgra[offset * 4 + 1]);
                        let r = i32::from(bgra[offset * 4 + 2]);
                        self.y[offset] = (16 + ((47 * r + 157 * g + 16 * b + 128) >> 8)) as u8;
                        r_sum += r;
                        g_sum += g;
                        b_sum += b;
                    }
                }
                let offset = row / 2 * (self.width / 2) + col / 2;
                self.u[offset] =
                    (128 + ((-26 * r_sum - 87 * g_sum + 113 * b_sum + 512) >> 10)) as u8;
                self.v[offset] =
                    (128 + ((112 * r_sum - 102 * g_sum - 10 * b_sum + 512) >> 10)) as u8;
            }
        }
        Ok(())
    }
}

impl YUVSource for I420 {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }
    fn y(&self) -> &[u8] {
        &self.y
    }
    fn u(&self) -> &[u8] {
        &self.u
    }
    fn v(&self) -> &[u8] {
        &self.v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isocast_engine::FramePool as _;

    #[test]
    fn bounded_admission_keeps_pts_and_first_idr_decodes() {
        let mut pool = crate::frames::FramePool::new(32, 32).unwrap();
        let frame = pool
            .draw(&mut |pixels, _| pixels.fill(255))
            .unwrap()
            .unwrap();
        let spec = LiveVideoSpec {
            width: 32,
            height: 32,
            fps: 30,
            bitrate_kbps: 1000,
            constant_bitrate: false,
            keyframe_interval: 60,
            low_latency: true,
        };
        let mut encoder = Encoder::new(&spec).unwrap();
        let first = Time::from_nanos(1_000_000_000);
        encoder.encode(&frame, first, false).unwrap();
        encoder
            .encode(&frame, first.add_ticks(1, 30), false)
            .unwrap();
        assert_eq!(encoder.dropped(), 1);
        let packet = encoder.receive().unwrap().unwrap();
        assert!(packet.keyframe);
        assert_eq!((packet.pts, packet.dts), (first, first));
        let mut annex_b = Vec::new();
        let parameters = packet.parameter_sets.unwrap();
        for nal in parameters.sps.iter().chain(&parameters.pps) {
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(nal);
        }
        let mut data = packet.data.as_slice();
        while !data.is_empty() {
            let size = u32::from_be_bytes(data[..4].try_into().unwrap()) as usize;
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(&data[4..4 + size]);
            data = &data[4 + size..];
        }
        let mut decoder = openh264::decoder::Decoder::new().unwrap();
        let decoded = decoder.decode(&annex_b).unwrap().unwrap();
        assert_eq!(decoded.dimensions(), (32, 32));
        assert!(decoded.y().iter().all(|value| (230..=240).contains(value)));
        assert!(encoder.receive().unwrap().is_none());
        assert!(encoder.set_rate(15, 1000).is_err());
        encoder
            .encode(&frame, first.add_ticks(2, 30), true)
            .unwrap();
        assert!(encoder.receive().unwrap().unwrap().keyframe);
    }

    #[test]
    fn rec709_black_white_and_neutral_chroma() {
        let mut pixels = I420::new(2, 2);
        pixels.read_bgra(&[0; 16]).unwrap();
        assert_eq!(pixels.y, [16; 4]);
        assert_eq!(pixels.u, [128]);
        assert_eq!(pixels.v, [128]);
        pixels.read_bgra(&[255; 16]).unwrap();
        assert_eq!(pixels.y, [235; 4]);
        assert_eq!(pixels.u, [128]);
        assert_eq!(pixels.v, [128]);
        assert!(pixels.read_bgra(&[0; 15]).is_err());
    }
}
