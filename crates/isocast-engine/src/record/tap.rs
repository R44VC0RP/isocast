//! A live output's raw packets. Video has its own budget: a slow encoder
//! must lose old stream pictures, not pin the compositor's entire pool or
//! consume the audio stall reserve.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, TrySendError};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use super::Packet;

/// Raw pictures retained by a live tap, independent of its audio capacity.
/// The platform's auxiliary reserve also covers the dequeued picture and
/// native submissions, shared with every other auxiliary consumer.
pub(crate) const VIDEO_DEPTH: usize = 8;

pub(super) enum Output<F> {
    Recording(SyncSender<Packet<F>>),
    Live(Sender<F>),
}

impl<F> From<SyncSender<Packet<F>>> for Output<F> {
    fn from(tx: SyncSender<Packet<F>>) -> Self {
        Self::Recording(tx)
    }
}

impl<F> Output<F> {
    pub(super) fn is_recording(&self) -> bool {
        matches!(self, Self::Recording(_))
    }

    pub(super) fn video(
        &self,
        frame: &F,
        pts: crate::Time,
        retain: fn(&F, crate::FrameUse) -> Option<F>,
    ) -> Result<(), TrySendError<Packet<F>>> {
        let usage = if self.is_recording() {
            crate::FrameUse::Recording
        } else {
            crate::FrameUse::Auxiliary
        };
        let mut kept = retain(frame, usage);
        if kept.is_none()
            && let Self::Live(tx) = self
        {
            tx.evict_video();
            kept = retain(frame, usage);
        }
        match kept {
            Some(frame) => self.try_send(Packet::Video { frame, pts }),
            None => Err(TrySendError::Full(Packet::Skipped { pts })),
        }
    }

    pub(super) fn try_send(&self, packet: Packet<F>) -> Result<(), TrySendError<Packet<F>>> {
        match self {
            Self::Recording(tx) => tx.try_send(packet),
            Self::Live(tx) => tx.send(packet),
        }
    }
}

struct State<F> {
    packets: VecDeque<Packet<F>>,
    video: usize,
    audio: usize,
    senders: usize,
    receiving: bool,
}

struct Queue<F> {
    state: Mutex<State<F>>,
    ready: Condvar,
    audio_depth: usize,
    missed_video: Arc<AtomicU64>,
}

/// The bounded packets of a program tap, oldest first. Audio keeps its
/// own stall reserve; video evicts its oldest picture when its budget fills.
pub struct Packets<F>(Arc<Queue<F>>);

pub(super) struct Sender<F>(Arc<Queue<F>>);

pub(super) fn channel<F>(
    audio_depth: usize,
    missed_video: Arc<AtomicU64>,
) -> (Sender<F>, Packets<F>) {
    let queue = Arc::new(Queue {
        state: Mutex::new(State {
            packets: VecDeque::with_capacity(audio_depth.saturating_add(VIDEO_DEPTH)),
            video: 0,
            audio: 0,
            senders: 1,
            receiving: true,
        }),
        ready: Condvar::new(),
        audio_depth,
        missed_video,
    });
    (Sender(queue.clone()), Packets(queue))
}

impl<F> Clone for Sender<F> {
    fn clone(&self) -> Self {
        self.0.state.lock().senders += 1;
        Self(self.0.clone())
    }
}

impl<F> Sender<F> {
    fn evict_video(&self) {
        let mut state = self.0.state.lock();
        let evicted = state
            .packets
            .iter()
            .position(|p| matches!(p, Packet::Video { .. }))
            .and_then(|index| state.packets.remove(index));
        if evicted.is_some() {
            state.video -= 1;
            self.0.missed_video.fetch_add(1, Ordering::Relaxed);
        }
        drop(state);
        drop(evicted);
    }

    fn send(&self, packet: Packet<F>) -> Result<(), TrySendError<Packet<F>>> {
        let mut state = self.0.state.lock();
        if !state.receiving {
            return Err(TrySendError::Disconnected(packet));
        }
        let mut evicted = None;
        match &packet {
            Packet::Video { .. } => {
                if state.video == VIDEO_DEPTH {
                    let index = state
                        .packets
                        .iter()
                        .position(|p| matches!(p, Packet::Video { .. }))
                        .expect("the counted video packet is queued");
                    evicted = state.packets.remove(index);
                    state.video -= 1;
                    self.0.missed_video.fetch_add(1, Ordering::Relaxed);
                }
                state.video += 1;
            }
            Packet::Audio { .. } => {
                if state.audio == self.0.audio_depth {
                    return Err(TrySendError::Full(packet));
                }
                state.audio += 1;
            }
            // Only ISO files get skipped ticks, never a program tap.
            Packet::Skipped { .. } | Packet::AudioGap { .. } => return Ok(()),
        }
        state.packets.push_back(packet);
        drop(state);
        self.0.ready.notify_one();
        // CoreVideo releases can enter the kernel; never keep the queue
        // locked while releasing the evicted picture.
        drop(evicted);
        Ok(())
    }
}

impl<F> Drop for Sender<F> {
    fn drop(&mut self) {
        self.0.state.lock().senders -= 1;
        self.0.ready.notify_all();
    }
}

impl<F> Packets<F> {
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Packet<F>, RecvTimeoutError> {
        let mut state = self.0.state.lock();
        self.0.ready.wait_while_for(
            &mut state,
            |s| s.packets.is_empty() && s.senders != 0,
            timeout,
        );
        if let Some(packet) = state.packets.pop_front() {
            match &packet {
                Packet::Video { .. } => state.video -= 1,
                Packet::Audio { .. } => state.audio -= 1,
                Packet::Skipped { .. } | Packet::AudioGap { .. } => {}
            }
            Ok(packet)
        } else if state.senders == 0 {
            Err(RecvTimeoutError::Disconnected)
        } else {
            Err(RecvTimeoutError::Timeout)
        }
    }
}

impl<F> Drop for Packets<F> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock();
        state.receiving = false;
        let packets = std::mem::take(&mut state.packets);
        drop(state);
        drop(packets);
    }
}
