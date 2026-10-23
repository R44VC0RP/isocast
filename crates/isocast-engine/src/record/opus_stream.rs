//! One Opus encoder for all program viewers. Neither a slow viewer nor the
//! encoder can block the mixer: both sides have small, bounded queues.

use std::sync::{Arc, Weak, mpsc};
use std::time::Duration;

use anyhow::Result;
use parking_lot::Mutex;

use super::{Packet, ProgramTapSource};
use crate::{Platform, Time};

const FRAMES: usize = 480;
const QUEUE: usize = 8;

pub struct OpusPacket {
    pub pts: Time,
    pub bytes: Arc<[u8]>,
}

pub struct OpusFeed {
    pub packets: mpsc::Receiver<Arc<OpusPacket>>,
    shared: Arc<Shared>,
    id: u64,
}

struct Viewer {
    id: u64,
    tx: mpsc::SyncSender<Arc<OpusPacket>>,
}

pub(super) struct Shared {
    viewers: Mutex<Option<Vec<Viewer>>>,
    next: std::sync::atomic::AtomicU64,
}

impl Drop for OpusFeed {
    fn drop(&mut self) {
        if let Some(viewers) = self.shared.viewers.lock().as_mut() {
            viewers.retain(|viewer| viewer.id != self.id);
        }
    }
}

pub(super) fn watch<P: Platform>(
    stream: &Mutex<Option<Weak<Shared>>>,
    source: ProgramTapSource<P>,
) -> Result<OpusFeed> {
    let mut stream = stream.lock();
    let (tx, packets) = mpsc::sync_channel(QUEUE);
    if let Some(shared) = stream.as_ref().and_then(Weak::upgrade) {
        let mut viewers = shared.viewers.lock();
        if let Some(viewers) = viewers.as_mut() {
            let id = shared
                .next
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            viewers.push(Viewer { id, tx });
            return Ok(OpusFeed {
                packets,
                shared: shared.clone(),
                id,
            });
        }
    }
    let mut encoder = opus2::Encoder::new(
        48_000,
        opus2::Channels::Stereo,
        opus2::Application::LowDelay,
    )?;
    encoder.set_bitrate(opus2::Bitrate::Bits(128_000))?;
    let tap = source.tap_audio(QUEUE)?;
    let shared = Arc::new(Shared {
        viewers: Mutex::new(Some(vec![Viewer { id: 0, tx }])),
        next: std::sync::atomic::AtomicU64::new(1),
    });
    let worker = shared.clone();
    std::thread::Builder::new().name("program-opus".into()).spawn(move || {
        struct Close(Arc<Shared>);
        impl Drop for Close {
            fn drop(&mut self) { self.0.viewers.lock().take(); }
        }
        let _close = Close(worker.clone());
        let mut pending = Vec::<f32>::with_capacity(FRAMES * 2);
        let mut beginning = Time::from_nanos(0);
        let mut expected = None;
        let mut output = [0u8; 4000];
        while !source.stopped() {
            {
                let mut viewers = worker.viewers.lock();
                if viewers.as_ref().is_none_or(Vec::is_empty) {
                    viewers.take();
                    break;
                }
            }
            let packet = match tap.packets.recv_timeout(Duration::from_millis(50)) {
                Ok(packet) => packet,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let Packet::Audio { samples, pts, .. } = packet else { continue };
            // A lost input chunk creates a timestamp gap, never an ever-growing delay.
            if expected.is_some_and(|end: Time| (end.as_nanos() - pts.as_nanos()).abs() > 2_000_000) {
                pending.clear();
            }
            if pending.is_empty() { beginning = pts; }
            expected = Some(Time::from_nanos(pts.as_nanos() + (samples.len() / 2) as i64 * 1_000_000_000 / 48_000));
            for sample in samples.iter() {
                pending.push(*sample);
                if pending.len() != FRAMES * 2 { continue; }
                let length = match encoder.encode_float(&pending, &mut output) {
                    Ok(length) => length,
                    Err(error) => {
                        isocast_diagnostics::diagnostic!("warn"; "program Opus encoder stopped: {error}");
                        return;
                    }
                };
                let packet = Arc::new(OpusPacket { pts: beginning, bytes: output[..length].into() });
                if let Some(viewers) = worker.viewers.lock().as_mut() {
                    viewers.retain(|viewer| match viewer.tx.try_send(packet.clone()) {
                        Ok(()) | Err(mpsc::TrySendError::Full(_)) => true,
                        Err(mpsc::TrySendError::Disconnected(_)) => false,
                    });
                }
                pending.clear();
                beginning = Time::from_nanos(beginning.as_nanos() + 10_000_000);
            }
        }
    })?;
    *stream = Some(Arc::downgrade(&shared));
    Ok(OpusFeed {
        packets,
        shared,
        id: 0,
    })
}
