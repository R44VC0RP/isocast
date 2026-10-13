//! The queue between the encoders and the connection, and what it drops
//! when the connection can't keep up.
//!
//! Sending must never fall far behind live: once the queue spans more than
//! [`DROP_AFTER`], every queued video frame that isn't a keyframe is
//! dropped, and so is new video until the next keyframe (H.264 frames
//! reference the ones before them, so nothing in between would decode).
//! Keyframes and audio stay. If even those pile up past [`HARD_LIMIT`], the
//! oldest go.
//!
//! Frames only count as dropped once the first connection has started
//! sending: what the encoder made while it connected was never due to go.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use crate::Time;

/// How far behind sending may fall before video is dropped.
pub(crate) const DROP_AFTER: Duration = Duration::from_millis(1000);
/// How much the queue may hold at all, keyframes and audio included.
const HARD_LIMIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    Video { keyframe: bool },
    Audio,
}

/// One encoded frame, ready to send.
#[derive(Debug, Clone)]
pub(crate) struct Media {
    /// Its decode time on the master clock.
    pub ts: Time,
    pub kind: Kind,
    /// H.264 NAL units with 4-byte lengths, or one raw AAC frame.
    pub data: Vec<u8>,
    /// For a keyframe, the AVC decoder configuration record it needs.
    pub config: Option<Arc<Vec<u8>>>,
}

impl Media {
    fn is_inter_frame(&self) -> bool {
        self.kind == Kind::Video { keyframe: false }
    }

    fn is_video(&self) -> bool {
        matches!(self.kind, Kind::Video { .. })
    }
}

#[derive(Debug, Default)]
pub(crate) struct SendQueue {
    items: VecDeque<Media>,
    /// Drop video until the next keyframe.
    skip_video: bool,
    /// A new connection waits for its first keyframe: everything before
    /// it goes.
    starting: bool,
    /// Where a new connection starts: the frame the encoder was asked to
    /// make its first keyframe. Until the encoder has taken the request,
    /// nothing is queued, so a keyframe made before the connection (still
    /// in the encoder when it connected) can't start the stream a frame or
    /// two before the requested one.
    start: Option<Time>,
    /// The newest video queued was a keyframe.
    keyframe_last: bool,
    /// A connection has started sending (its first keyframe was queued).
    sending: bool,
    /// Video frames dropped so far.
    pub(crate) dropped_frames: u64,
    pub(crate) audio_dropouts: u64,
    /// The worst congestion since it was last taken.
    peak: f64,
}

impl SendQueue {
    pub(crate) fn push(&mut self, item: Media) {
        if self.starting {
            let started = self.start.is_some_and(|start| item.ts >= start);
            match item.kind {
                Kind::Video { keyframe: true } if started => {
                    self.starting = false;
                    self.sending = true;
                }
                Kind::Video { .. } => {
                    self.dropped(1);
                    return;
                }
                Kind::Audio => {
                    self.dropped_audio(1);
                    return;
                }
            }
        }
        if let Kind::Video { keyframe } = item.kind {
            if self.skip_video && !keyframe {
                self.dropped(1);
                return;
            }
            self.skip_video &= !keyframe;
            self.keyframe_last = keyframe;
        }
        self.items.push_back(item);
        if self.span() > DROP_AFTER {
            let before = self.items.len();
            self.items.retain(|item| !item.is_inter_frame());
            self.dropped((before - self.items.len()) as u64);
            // Frames after the newest keyframe queued still decode.
            self.skip_video = !self.keyframe_last;
        }
        while self.span() > HARD_LIMIT {
            let oldest = self.items.pop_front().expect("a span needs items");
            if oldest.is_video() {
                self.dropped(1);
            } else {
                self.dropped_audio(1);
            }
        }
        self.peak = self.peak.max(self.congestion());
    }

    pub(crate) fn pop(&mut self) -> Option<Media> {
        self.items.pop_front()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The time between the oldest and newest frames queued.
    fn span(&self) -> Duration {
        match (self.items.front(), self.items.back()) {
            (Some(first), Some(last)) => last.ts.saturating_since(first.ts),
            _ => Duration::ZERO,
        }
    }

    /// How close sending is to dropping video: 0 to 1.
    pub(crate) fn congestion(&self) -> f64 {
        (self.span().as_secs_f64() / DROP_AFTER.as_secs_f64()).min(1.0)
    }

    /// The worst congestion since the last call.
    pub(crate) fn take_peak(&mut self) -> f64 {
        let now = self.congestion();
        std::mem::replace(&mut self.peak, now)
    }

    /// Start again for a new connection: drop what is queued, and begin
    /// with the keyframe the encoder is asked for (see [`Self::start_at`]).
    pub(crate) fn restart(&mut self) {
        self.dropped(self.items.iter().filter(|item| item.is_video()).count() as u64);
        self.dropped_audio(self.items.iter().filter(|item| !item.is_video()).count() as u64);
        self.items.clear();
        self.starting = true;
        self.start = None;
        self.skip_video = false;
    }

    /// The new connection's first keyframe was asked of the frame at `ts`:
    /// it starts at the first keyframe from then on.
    pub(crate) fn start_at(&mut self, ts: Time) {
        if self.starting {
            self.start = Some(ts);
        }
    }

    /// Count `frames` video frames as dropped, once sending has started.
    pub(crate) fn dropped(&mut self, frames: u64) {
        if self.sending {
            self.dropped_frames += frames;
        }
    }

    fn dropped_audio(&mut self, chunks: u64) {
        if self.sending {
            self.audio_dropouts += chunks;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(ms: i64, keyframe: bool) -> Media {
        Media {
            ts: Time::from_nanos(ms * 1_000_000),
            kind: Kind::Video { keyframe },
            data: Vec::new(),
            config: None,
        }
    }

    fn audio(ms: i64) -> Media {
        Media {
            kind: Kind::Audio,
            ..video(ms, false)
        }
    }

    fn kinds(queue: &mut SendQueue) -> Vec<(i64, &'static str)> {
        std::iter::from_fn(|| queue.pop())
            .map(|m| {
                let kind = match m.kind {
                    Kind::Video { keyframe: true } => "K",
                    Kind::Video { keyframe: false } => "P",
                    Kind::Audio => "a",
                };
                (m.ts.as_nanos() / 1_000_000, kind)
            })
            .collect()
    }

    #[test]
    fn a_backlog_drops_inter_frames_then_video_until_a_keyframe() {
        let mut queue = SendQueue::default();
        queue.restart();
        queue.start_at(Time::from_nanos(0));
        // Nothing is sent: 30 ms video with keyframes at 0 and 1020 ms, and
        // audio every 20 ms.
        for ms in (0..=1100).step_by(10) {
            if ms % 30 == 0 {
                queue.push(video(ms, ms == 0 || ms == 1020));
            }
            if ms % 20 == 0 {
                queue.push(audio(ms));
            }
        }
        // Past a second behind, every inter frame went and later ones
        // keep going; keyframes and audio stay.
        let left = kinds(&mut queue);
        let frames: Vec<_> = left.iter().filter(|(_, k)| *k != "a").collect();
        assert_eq!(frames, [&(0, "K"), &(1020, "K")]);
        assert_eq!(left.iter().filter(|(_, k)| *k == "a").count(), 56);
        assert_eq!(queue.dropped_frames, 35);
        // Once caught up, inter frames wait for the next keyframe.
        queue.push(video(1110, false));
        queue.push(video(2040, true));
        queue.push(video(2070, false));
        assert_eq!(kinds(&mut queue), [(2040, "K"), (2070, "P")]);
        assert_eq!(queue.dropped_frames, 36);
    }

    #[test]
    fn a_new_connection_starts_at_a_keyframe() {
        let mut queue = SendQueue::default();
        // Made while the first connection was being made: not drops.
        queue.push(video(0, true));
        queue.push(video(33, false));
        queue.restart();
        queue.push(audio(40));
        queue.push(video(66, false));
        // A keyframe made before the encoder took the connection's request
        // doesn't start it, nor does one before the requested frame.
        queue.push(video(80, true));
        queue.start_at(Time::from_nanos(100_000_000));
        queue.push(video(90, true));
        queue.push(video(100, true));
        queue.push(audio(110));
        assert_eq!(kinds(&mut queue), [(100, "K"), (110, "a")]);
        assert_eq!(queue.dropped_frames, 0);
        // A reconnect loses what was queued and what comes before its
        // keyframe.
        queue.push(video(133, false));
        queue.restart();
        queue.push(video(166, false));
        queue.start_at(Time::from_nanos(200_000_000));
        queue.push(video(200, true));
        assert_eq!(kinds(&mut queue), [(200, "K")]);
        assert_eq!(queue.dropped_frames, 2);
    }

    #[test]
    fn the_queue_never_holds_more_than_its_limit() {
        let mut queue = SendQueue::default();
        queue.restart();
        queue.start_at(Time::from_nanos(0));
        queue.push(video(0, true));
        for ms in (0..10_000).step_by(20) {
            queue.push(audio(ms));
        }
        assert!(queue.span() <= HARD_LIMIT);
        assert_eq!(queue.congestion(), 1.0);
        assert_eq!(queue.dropped_frames, 1);
        assert_eq!(queue.audio_dropouts, 249);
    }
}
