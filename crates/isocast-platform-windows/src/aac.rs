//! The Microsoft, synchronous AAC-LC MFT: PCM16 in, raw 48 kHz AAC out.

use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{AudioEncoder, LiveAudioSpec};
use windows::Win32::Foundation::E_NOTIMPL;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

use crate::mf::Runtime;
use crate::native::Apartment;

const BLOCK: u64 = 1024;
// An explicit silent pre-roll protects the first real sample from the MFT's
// unprimed analysis window and makes even a one-sample take encodable.
const PRIMING: u64 = BLOCK;
const RATE: u64 = 48_000;
const DURATION: i64 = 213_333;
const MAX_BATCH: usize = 16_384;
const READY_LIMIT: usize = 32;
const DEBT_LIMIT: u64 = 30; // Two slots reserved for terminal zero padding.

/// One raw AAC access unit, without an ADTS header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AacPacket {
    pub data: Vec<u8>,
    /// Stream-relative decode time in 48 kHz sample frames (per channel).
    pub dts_samples: u64,
    /// Normally 1024; the last published packet may have a shorter mux duration.
    /// The decoder still emits 1024 samples; the mux must trim its padded tail.
    pub samples: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AacFinish {
    pub input_frames: u64,
    pub priming: u32,
    /// Padding in the last published packet, excluding wholly padded packets.
    pub padding: u32,
    /// Total published packets, including those already received.
    pub packets: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AacStats {
    /// Real input accepted, in sample frames per channel.
    pub accepted_frames: u64,
    /// Frames submitted to the MFT, including terminal zero padding.
    pub submitted_frames: u64,
    /// Validated native packets, including discarded wholly padded packets.
    pub produced_packets: u64,
    pub ready_packets: usize,
    /// Real frames not yet represented by native output; excludes ready packets.
    pub buffered_frames: u64,
    /// Locally held, unsubmitted PCM frames; less than 1024 while healthy.
    pub partial_frames: usize,
}

type Job = Box<dyn FnOnce(&mut Session) + Send>;

/// COM lives exclusively on a private MTA worker. Calls are synchronous RPCs
/// through a one-deep queue, not a deadline around potentially hung COM calls.
/// Recording callers must finish and receive explicitly; Drop aborts and joins.
/// Do not mix the byte-oriented AudioEncoder API with packet consumption.
pub struct MfAacEncoder {
    jobs: Option<mpsc::SyncSender<Job>>,
    worker: Option<JoinHandle<()>>,
    spec: LiveAudioSpec,
    config: [u8; 2],
    /// Only the byte-oriented live path pads; packet consumers never see it.
    padding: Option<Padding>,
}

impl MfAacEncoder {
    pub fn new(spec: &LiveAudioSpec) -> Result<Self> {
        Self::with_thread_start(spec, || {})
    }

    /// Live variant for the byte-oriented AudioEncoder path: each packet is
    /// prefixed with AAC fill elements so the stream averages the nominal
    /// bitrate. The Microsoft encoder is not constant-bitrate on simple audio.
    pub fn streaming(spec: &LiveAudioSpec, thread_start: fn()) -> Result<Self> {
        let mut encoder = Self::with_thread_start(spec, thread_start)?;
        encoder.padding = Some(Padding::default());
        Ok(encoder)
    }

    /// Run the platform's thread-local scheduling hook once on this worker,
    /// so its existing MMCSS registration can be reused without duplicating it.
    pub fn with_thread_start(spec: &LiveAudioSpec, thread_start: fn()) -> Result<Self> {
        if !matches!(spec.channels, 1 | 2) {
            bail!("audio.channels: Microsoft AAC supports 1 or 2 channels");
        }
        if !matches!(spec.bitrate_kbps, 96 | 128 | 160 | 192) {
            bail!("audio.bitrate_kbps: Microsoft AAC supports 96, 128, 160 or 192");
        }
        let settings = *spec;
        let (tx, rx) = mpsc::sync_channel::<Job>(1);
        let (started, startup) = mpsc::sync_channel(1);
        let worker = std::thread::Builder::new()
            .name("Media Foundation AAC".into())
            .spawn(move || {
                let run =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
                        let _apartment = Apartment::new()?;
                        thread_start();
                        let _runtime = Runtime::new()?;
                        let (mut session, config) = Session::open(&settings)?;
                        if started.send(Ok(config)).is_err() {
                            return Ok(());
                        }
                        while let Ok(job) = rx.recv() {
                            job(&mut session);
                        }
                        Ok(())
                    }));
                let error = match run {
                    Ok(Ok(())) => return,
                    Ok(Err(error)) => format!("{error:#}"),
                    Err(_) => "MF AAC worker panicked".into(),
                };
                let _ = started.send(Err(error));
            })?;
        let config = match startup.recv() {
            Ok(Ok(config)) => config,
            result => {
                drop(tx);
                let _ = worker.join();
                bail!(
                    "audio.mf: {}",
                    match result {
                        Ok(Err(error)) => error,
                        _ => "worker stopped during startup".into(),
                    }
                );
            }
        };
        Ok(Self {
            jobs: Some(tx),
            worker: Some(worker),
            spec: *spec,
            config,
            padding: None,
        })
    }

    pub fn channels(&self) -> u32 {
        self.spec.channels
    }
    pub fn bitrate_kbps(&self) -> u32 {
        self.spec.bitrate_kbps
    }
    pub fn decoder_config(&self) -> &[u8] {
        &self.config
    }

    /// Deliberate silent pre-roll, verified against decoded first/last samples.
    /// This is not the MFT's two-to-three-block processing latency.
    pub fn delay(&self) -> u32 {
        PRIMING as u32
    }

    /// False means only pre-admission queue pressure, with no input consumed.
    /// A native error after admission is terminal, not a request to retry input.
    /// Input is finite, interleaved 48 kHz PCM; at most 16384 frames per call.
    /// Invalid input does not poison the encoder or consume any samples.
    pub fn try_encode(&mut self, samples: &[f32]) -> Result<bool> {
        let channels = self.channels() as usize;
        if !samples.len().is_multiple_of(channels) {
            bail!("audio.samples: interleaved input must contain whole channel frames");
        }
        if samples.len() / channels > MAX_BATCH {
            bail!("audio.samples: at most {MAX_BATCH} sample frames per call");
        }
        if samples.iter().any(|sample| !sample.is_finite()) {
            bail!("audio.samples: NaN and infinity are not valid PCM");
        }
        let pcm: Vec<i16> = samples
            .iter()
            .map(|sample| {
                (sample.clamp(-1.0, 1.0) * 32768.0)
                    .round()
                    .clamp(-32768.0, 32767.0) as i16
            })
            .collect();
        self.call(move |session| session.encode(pcm))
    }

    /// Pop valid queued output even after a fatal error. Once that prefix has
    /// been consumed, the fatal error is sticky. An empty healthy queue is pumped.
    pub fn receive(&mut self) -> Result<Option<AacPacket>> {
        self.call(|session| {
            if let Some(packet) = session.ready.pop_front() {
                return Ok(Some(packet));
            }
            let result = session.drain_ready();
            if let Some(packet) = session.ready.pop_front() {
                return Ok(Some(packet));
            }
            result?;
            Ok(None)
        })
    }

    /// Poll normal output without EOS, DRAIN or FLUSH. The MFT can still retain
    /// accepted input afterwards; inspect stats rather than treating this as EOS.
    pub fn drain_ready(&mut self) -> Result<()> {
        self.call(Session::drain_ready)
    }

    /// Terminal and idempotent; never removes queued packets, including on error.
    pub fn finish(&mut self) -> Result<AacFinish> {
        self.call(Session::finish)
    }

    /// Available after failure as well as during normal operation.
    pub fn stats(&self) -> Result<AacStats> {
        self.call(|session| Ok(session.stats()))
    }

    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Session) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.jobs
            .as_ref()
            .context("MF AAC encoder is closed")?
            .send(Box::new(move |session| {
                let _ = tx.send(f(session));
            }))
            .map_err(|_| anyhow!("MF AAC worker stopped"))?;
        rx.recv()
            .context("MF AAC worker stopped during a request")?
    }
}

impl AudioEncoder for MfAacEncoder {
    fn encode(&mut self, samples: &[f32]) -> Result<Vec<Vec<u8>>> {
        if !self.try_encode(samples)? {
            bail!("audio.samples: MF AAC queue is full; input was not accepted");
        }
        // Do not remove valid output before a possible native failure: packet
        // callers can still retrieve the entire valid prefix on the error path.
        self.drain_ready()?;
        let packets: Vec<Vec<u8>> =
            self.call(|session| Ok(session.ready.drain(..).map(|packet| packet.data).collect()))?;
        let Some(padding) = &mut self.padding else {
            return Ok(packets);
        };
        let kbps = self.spec.bitrate_kbps;
        packets
            .into_iter()
            .map(|packet| padding.pad(kbps, packet))
            .collect()
    }

    fn delay(&self) -> u32 {
        MfAacEncoder::delay(self)
    }
}

impl Drop for MfAacEncoder {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Cumulative byte budget for live streams. The count is of native packets,
/// not elapsed time, so a parked encoder never banks credit. A packet over its
/// share is never truncated; later packets pay it back by padding less.
#[derive(Default)]
struct Padding {
    packets: u64,
    emitted: u64,
}

impl Padding {
    fn pad(&mut self, kbps: u32, mut packet: Vec<u8>) -> Result<Vec<u8>> {
        let rate = u128::from(BLOCK) * u128::from(kbps) * 1000;
        let frame = u64::try_from(rate.div_ceil(u128::from(RATE * 8)))?;
        self.packets = self
            .packets
            .checked_add(1)
            .context("audio.mf: packet count overflow")?;
        let budget = u64::try_from(rate * u128::from(self.packets) / u128::from(RATE * 8))
            .context("audio.mf: byte budget overflow")?;
        let size = u64::try_from(packet.len())?;
        let wanted = budget.saturating_sub(self.emitted).saturating_sub(size);
        // A fill prefix is at least 7 bytes; a smaller shortfall carries over.
        // Earlier packets leave under 7 bytes unpaid, so more than a frame
        // plus that carry means the accounting is broken.
        if wanted > frame + 7 {
            bail!("audio.mf: padding request of {wanted} bytes exceeds one {frame}-byte frame");
        }
        let padded = if wanted < 7 {
            size
        } else {
            let mut prefix = fill_prefix(usize::try_from(wanted)?)?;
            prefix.append(&mut packet);
            packet = prefix;
            size + wanted
        };
        self.emitted = self
            .emitted
            .checked_add(padded)
            .context("audio.mf: byte count overflow")?;
        Ok(packet)
    }
}

struct Session {
    transform: IMFTransform,
    output_size: u32,
    channels: usize,
    partial: Vec<i16>,
    ready: VecDeque<AacPacket>,
    accepted: u64,
    submitted: u64,
    produced: u64,
    published: u64,
    primed: bool,
    terminal: bool,
    finished: Option<AacFinish>,
    failed: Option<String>,
}

impl Session {
    fn open(spec: &LiveAudioSpec) -> Result<(Self, [u8; 2])> {
        let transform: IMFTransform =
            unsafe { CoCreateInstance(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER) }
                .context("activate Microsoft AACMFTEncoder")?;
        let mut inputs = 0;
        let mut outputs = 0;
        unsafe { transform.GetStreamCount(&mut inputs, &mut outputs) }?;
        if (inputs, outputs) != (1, 1) {
            bail!("MF AAC requires one input and one output");
        }
        let mut input_ids = [0];
        let mut output_ids = [0];
        match unsafe { transform.GetStreamIDs(&mut input_ids, &mut output_ids) } {
            Ok(()) if input_ids != [0] || output_ids != [0] => {
                bail!("MF AAC requires stream IDs 0")
            }
            Err(error) if error.code() != E_NOTIMPL => return Err(error.into()),
            _ => {}
        }
        match unsafe { transform.GetAttributes() } {
            Ok(attributes) => match unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) } {
                Ok(0) => {}
                Ok(_) => bail!("Microsoft AAC unexpectedly requires asynchronous processing"),
                Err(error) if error.code() == MF_E_ATTRIBUTENOTFOUND => {}
                Err(error) => return Err(error.into()),
            },
            Err(error) if error.code() == E_NOTIMPL => {}
            Err(error) => return Err(error.into()),
        }
        unsafe {
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, spec.channels)?;
            output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE as u32)?;
            output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, spec.bitrate_kbps * 125)?;
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            output.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
            transform.SetOutputType(0, &output, 0)?;
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, spec.channels)?;
            input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE as u32)?;
            input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            input.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, spec.channels * 2)?;
            input.SetUINT32(
                &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
                RATE as u32 * spec.channels * 2,
            )?;
            transform.SetInputType(0, &input, 0)?;
        }
        let config = decoder_config(&transform, spec)?;
        let info = unsafe { transform.GetOutputStreamInfo(0) }?;
        if info.dwFlags != 0 || info.cbAlignment != 0 || info.cbSize == 0 || info.cbSize > 65_536 {
            bail!("MF AAC returned unsupported output buffer requirements");
        }
        let session = Self {
            transform,
            output_size: info.cbSize,
            channels: spec.channels as usize,
            partial: Vec::new(),
            ready: VecDeque::new(),
            accepted: 0,
            submitted: 0,
            produced: 0,
            published: 0,
            primed: false,
            terminal: false,
            finished: None,
            failed: None,
        };
        unsafe {
            session
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            session
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok((session, config))
    }

    fn healthy(&self) -> Result<()> {
        if let Some(error) = &self.failed {
            bail!("{error}");
        }
        Ok(())
    }

    fn media<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.healthy()?;
        let result = f(self);
        if let Err(error) = &result {
            self.failed = Some(format!("{error:#}"));
        }
        result
    }

    fn encode(&mut self, pcm: Vec<i16>) -> Result<bool> {
        self.healthy()?;
        if self.terminal {
            bail!("MF AAC encoder has begun finishing");
        }
        let frames = u64::try_from(pcm.len() / self.channels)?;
        let accepted = self
            .accepted
            .checked_add(frames)
            .context("audio.samples: frame count overflow")?;
        // Validate the eventual padded endpoint too, before accepting anything.
        timestamp(terminal_frames(accepted)?)?;
        let new_blocks = (self.partial.len() + pcm.len()) / self.channels / BLOCK as usize
            + usize::from(!self.primed && frames != 0);
        let debt = self.ready.len() as u64 + self.submitted / BLOCK - self.produced;
        if debt + new_blocks as u64 > DEBT_LIMIT {
            return Ok(false);
        }
        self.media(move |session| {
            session.accepted = accepted;
            if !session.primed && frames != 0 {
                session
                    .partial
                    .resize(PRIMING as usize * session.channels, 0);
                session.submit_partial()?;
                session.primed = true;
            }
            session.partial.extend(pcm);
            session.submit_partial()?;
            Ok(true)
        })
    }

    fn submit_partial(&mut self) -> Result<()> {
        let block = BLOCK as usize * self.channels;
        while self.partial.len() >= block {
            let bytes = u32::try_from(block.checked_mul(2).context("MF AAC input size overflow")?)?;
            let buffer = unsafe { MFCreateMemoryBuffer(bytes) }?;
            with_buffer(&buffer, |data, capacity, _| {
                if data.is_null() || capacity < bytes {
                    bail!("MF AAC input buffer is too small");
                }
                // All supported Windows targets are little endian; PCM is signed 16-bit.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.partial.as_ptr().cast::<u8>(),
                        data,
                        bytes as usize,
                    );
                }
                Ok(())
            })?;
            let sample = unsafe { MFCreateSample() }?;
            let next = self
                .submitted
                .checked_add(BLOCK)
                .context("MF AAC submitted frame overflow")?;
            unsafe {
                buffer.SetCurrentLength(bytes)?;
                sample.AddBuffer(&buffer)?;
                sample.SetSampleTime(timestamp(self.submitted)?)?;
                sample.SetSampleDuration(DURATION)?;
                // Only full, nonempty blocks enter this codec. Empty/zero-duration
                // input can divide by zero inside Microsoft's implementation.
                self.transform
                    .ProcessInput(0, &sample, 0)
                    .context("MF AAC ProcessInput")?;
            }
            self.submitted = next;
            self.partial.drain(..block);
            self.pump()?;
        }
        Ok(())
    }

    fn drain_ready(&mut self) -> Result<()> {
        self.healthy()?;
        if self.finished.is_some() {
            return Ok(());
        }
        self.media(Self::pump)
    }

    fn pump(&mut self) -> Result<()> {
        // Each successful output consumes a submitted block, bounding this loop
        // by the admission debt. A stream change or extra output is fatal.
        while self.output()? {}
        Ok(())
    }

    fn output(&mut self) -> Result<bool> {
        let sample = unsafe { MFCreateSample() }?;
        let buffer = unsafe { MFCreateMemoryBuffer(self.output_size) }?;
        unsafe { sample.AddBuffer(&buffer) }?;
        let mut output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(Some(sample)),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0;
        let result = unsafe {
            self.transform
                .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
        };
        // windows-rs deliberately leaves these COM fields manually dropped.
        let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
        drop(unsafe { ManuallyDrop::take(&mut output.pEvents) });
        match result {
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
            Err(error) => return Err(error).context("MF AAC ProcessOutput"),
            Ok(()) => {}
        }
        if status != 0 || output.dwStatus & !(MFT_OUTPUT_DATA_BUFFER_INCOMPLETE.0 as u32) != 0 {
            bail!("MF AAC changed its output stream or sample status");
        }
        if self.produced >= self.submitted / BLOCK {
            bail!("MF AAC produced output without input");
        }
        let dts = self
            .produced
            .checked_mul(BLOCK)
            .context("MF AAC output timestamp overflow")?;
        let sample = sample.context("MF AAC returned no output sample")?;
        let expected = timestamp(dts)?;
        unsafe {
            if sample.GetSampleTime()? != expected || sample.GetSampleDuration()? != DURATION {
                bail!("MF AAC changed the expected 1024-sample output timeline at {dts}");
            }
            match sample.GetUINT64(&MFSampleExtension_DecodeTimestamp) {
                Ok(value) if value != expected as u64 => bail!("MF AAC reordered output at {dts}"),
                Err(error) if error.code() != MF_E_ATTRIBUTENOTFOUND => return Err(error.into()),
                _ => {}
            }
        }
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }?;
        let data = with_buffer(&buffer, |data, capacity, length| {
            if data.is_null() || length == 0 || length > capacity || length > self.output_size {
                bail!("MF AAC returned invalid packet size {length}");
            }
            Ok(unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec())
        })?;
        let end = if self.accepted == 0 {
            0
        } else {
            PRIMING + self.accepted
        };
        let samples = end.saturating_sub(dts).min(BLOCK) as u32;
        if !self.terminal && samples != BLOCK as u32 {
            bail!("MF AAC output exceeds real input");
        }
        if samples != 0 && self.ready.len() >= READY_LIMIT {
            bail!("MF AAC output exceeded its queue budget");
        }
        self.produced += 1;
        if samples != 0 {
            self.ready.push_back(AacPacket {
                data,
                dts_samples: dts,
                samples,
            });
            self.published += 1;
        }
        Ok(true)
    }

    fn finish(&mut self) -> Result<AacFinish> {
        if let Some(finished) = self.finished {
            return Ok(finished);
        }
        self.terminal = true;
        self.media(|session| {
            if session.accepted != 0 {
                // The pre-roll plus at least one real (possibly padded) block
                // meets the native two-block minimum, including tiny takes.
                let target = terminal_frames(session.accepted)?;
                let remaining = usize::try_from(target - session.submitted)? * session.channels;
                session.partial.resize(remaining, 0);
                session.submit_partial()?;
            }
            unsafe {
                session.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
                session.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
            }
            session.pump()?;
            if session.produced != session.submitted / BLOCK
                || session.published != if session.accepted == 0 { 0 } else { (PRIMING + session.accepted).div_ceil(BLOCK) } {
                bail!("MF AAC drain lost audio: {} input frames, {} submitted, {} native packets, {} published",
                    session.accepted, session.submitted, session.produced, session.published);
            }
            let finished = AacFinish {
                input_frames: session.accepted, priming: PRIMING as u32,
                padding: ((BLOCK - session.accepted % BLOCK) % BLOCK) as u32,
                packets: session.published,
            };
            session.finished = Some(finished);
            Ok(finished)
        })
    }

    fn stats(&self) -> AacStats {
        AacStats {
            accepted_frames: self.accepted,
            submitted_frames: self.submitted,
            produced_packets: self.produced,
            ready_packets: self.ready.len(),
            buffered_frames: self
                .accepted
                .saturating_sub((self.produced * BLOCK).saturating_sub(PRIMING)),
            partial_frames: self.partial.len() / self.channels,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // On the owning MTA, before the transform, runtime and apartment release.
        // No implicit EOS: livestream abort may discard input; recording must finish.
        let _ = unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) };
        let _ = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)
        };
    }
}

fn terminal_frames(frames: u64) -> Result<u64> {
    if frames == 0 {
        return Ok(0);
    }
    frames
        .checked_add(PRIMING)
        .context("audio.samples: primed frame count overflow")?
        .div_ceil(BLOCK)
        .max(2)
        .checked_mul(BLOCK)
        .context("audio.samples: padded frame count overflow")
}

/// Exactly `bytes` (at least 7) of eight ISO/IEC 14496-3 fill elements, to
/// precede the first element of a raw_data_block. Each is a 3-bit ID_FIL, a
/// 4-bit count (15 adds an 8-bit esc_count, so `cnt = 14 + esc_count`) and `cnt`
/// bytes of extension_payload: EXT_FILL, a zero fill nibble, then 0xA5 bytes.
/// Eight elements are 56 header bits, so the block's own bytes stay aligned
/// for its end-of-block byte_align() and any byte_alignment() inside it.
fn fill_prefix(bytes: usize) -> Result<Vec<u8>> {
    let mut left = bytes
        .checked_sub(7)
        .context("audio.mf: fill prefix needs at least 7 bytes")?;
    let mut out = Vec::with_capacity(bytes);
    let (mut acc, mut held) = (0u32, 0u32);
    let mut put = |value: u32, bits: u32| {
        acc = acc << bits | value;
        held += bits;
        while held >= 8 {
            out.push((acc >> (held - 8)) as u8);
            held -= 8;
        }
        acc &= (1 << held) - 1;
    };
    for _ in 0..8 {
        // A count costs its payload bytes, plus the escape byte from 15 up;
        // 15 bytes in one element is unrepresentable, so leave one over.
        let mut used = left.min(270);
        if used == 15 {
            used = 14;
        }
        left -= used;
        let count = if used >= 16 { used - 1 } else { used } as u32;
        put(0b110, 3);
        if count >= 15 {
            put(15, 4);
            put(count - 14, 8);
        } else {
            put(count, 4);
        }
        if count > 0 {
            put(0, 8);
            for _ in 1..count {
                put(0xA5, 8);
            }
        }
    }
    if left != 0 || held != 0 || out.len() != bytes {
        bail!("audio.mf: cannot build a {bytes}-byte fill prefix");
    }
    Ok(out)
}

fn timestamp(frames: u64) -> Result<i64> {
    i64::try_from(u128::from(frames) * 10_000_000 / u128::from(RATE))
        .context("audio.samples: Media Foundation timestamp overflow")
}

fn decoder_config(transform: &IMFTransform, spec: &LiveAudioSpec) -> Result<[u8; 2]> {
    unsafe {
        let output = transform.GetOutputCurrentType(0)?;
        if output.GetGUID(&MF_MT_MAJOR_TYPE)? != MFMediaType_Audio
            || output.GetGUID(&MF_MT_SUBTYPE)? != MFAudioFormat_AAC
        {
            bail!("MF AAC returned a non-AAC media type");
        }
        for (key, expected) in [
            (MF_MT_AUDIO_NUM_CHANNELS, spec.channels),
            (MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE as u32),
            (MF_MT_AUDIO_BITS_PER_SAMPLE, 16),
            (MF_MT_AUDIO_AVG_BYTES_PER_SECOND, spec.bitrate_kbps * 125),
            (MF_MT_AAC_PAYLOAD_TYPE, 0),
            (MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29),
        ] {
            if output.GetUINT32(&key)? != expected {
                bail!("MF AAC changed its negotiated configuration");
            }
        }
        let config = [0x11, if spec.channels == 1 { 0x88 } else { 0x90 }];
        // MF_MT_USER_DATA is HEAACWAVEINFO (12 bytes), then AudioSpecificConfig.
        // Only raw AAC-LC, 48 kHz, mono/stereo with no extension is qualified.
        let mut data = [0u8; 14];
        if output.GetBlobSize(&MF_MT_USER_DATA)? != data.len() as u32 {
            bail!("MF AAC returned an unsupported decoder configuration length");
        }
        output.GetBlob(&MF_MT_USER_DATA, &mut data, None)?;
        if data[..12] != [0, 0, 0x29, 0, 0, 0, 0, 0, 0, 0, 0, 0] || data[12..] != config {
            bail!("MF AAC returned an unsupported HEAAC header or AudioSpecificConfig");
        }
        Ok(config)
    }
}

/// Unlock even if copying/validating the locked view unwinds; report normal
/// Unlock failures rather than silently treating those packets as valid.
fn with_buffer<T>(
    buffer: &IMFMediaBuffer,
    f: impl FnOnce(*mut u8, u32, u32) -> Result<T>,
) -> Result<T> {
    struct Unlock<'a>(&'a IMFMediaBuffer);
    impl Drop for Unlock<'_> {
        fn drop(&mut self) {
            let _ = unsafe { self.0.Unlock() };
        }
    }
    let mut data = std::ptr::null_mut();
    let mut capacity = 0;
    let mut length = 0;
    unsafe { buffer.Lock(&mut data, Some(&mut capacity), Some(&mut length)) }?;
    let guard = Unlock(buffer);
    let result = f(data, capacity, length);
    let unlocked = unsafe { buffer.Unlock() };
    std::mem::forget(guard);
    unlocked?;
    result
}
