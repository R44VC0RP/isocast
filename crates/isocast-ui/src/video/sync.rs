//! Lip-sync for a WebRTC viewer (RFC 3550 §6.4.1 and RFC 7273's model):
//! each packet's RTP time maps to the sender's wallclock through the newest
//! RTCP Sender Report of its stream (its NTP time and the RTP time of the
//! same instant). Picture and sound then share one sender timeline. A
//! moment `s` on it plays here at `s + delay`, where `delay` covers the
//! slower stream (the engine's mix holds sound back to protect recordings;
//! the picture waits for it) plus a jitter margin. The delay follows the
//! smallest recent transit of each stream: it rises at once when a packet
//! would otherwise be late, and otherwise moves slowly (no audible jumps).
//!
//! Pure arithmetic, no clocks of its own: times are seconds on this
//! machine's monotonic clock since an arbitrary start ([`Playout::seconds`]).

use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Jitter margin above the slower stream's smallest transit.
pub const MARGIN: f64 = 0.040;
/// How long a transit sample counts toward a stream's minimum.
const WINDOW: f64 = 3.0;
/// The least a late packet gets between arriving and playing.
const ON_TIME: f64 = 0.005;
/// The most one late packet may raise the delay above what the streams'
/// smallest transits ask for; a packet later than that (after a stall)
/// is too late to wait for, and the delay above it falls at once.
pub const MAX_RISE: f64 = 0.200;
/// How fast the delay may fall, in seconds per second.
const FALL: f64 = 0.01;
/// A stream whose last packet is older than this no longer holds the
/// other back (the engine stopped sending sound, say).
const STALE: f64 = 1.0;

/// The newest Sender Report of a stream: an instant as RTP time and as the
/// sender's wallclock.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Report {
    /// The RTP timestamp (32 bits) of the instant.
    pub rtp: u32,
    /// The sender's wallclock at it, in seconds since 1970.
    pub wallclock: f64,
}

impl Report {
    /// From an SR's NTP time.
    pub fn new(rtp: u64, ntp: SystemTime) -> Self {
        Self {
            rtp: rtp as u32,
            wallclock: ntp
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs_f64(),
        }
    }

    /// The sender's wallclock of RTP time `rtp` at `rate` Hz (wrapping
    /// 32-bit RTP arithmetic: within half the wrap of the report).
    pub fn wallclock_of(&self, rtp: u64, rate: u32) -> f64 {
        let ticks = (rtp as u32).wrapping_sub(self.rtp) as i32;
        self.wallclock + f64::from(ticks) / f64::from(rate)
    }
}

/// How long a stream's slowest recent arrival counts: longer than the
/// gaps between a Windows client's sound bursts (one every ~10 s on
/// exon-desk), so the delay keeps covering them in between.
const JITTER_WINDOW: f64 = 15.0;

/// One stream's recent transits (arrival here minus sender time): the
/// smallest over [`WINDOW`] and the largest over [`JITTER_WINDOW`] (not
/// counting stalls beyond [`MAX_RISE`]).
#[derive(Debug, Default)]
struct Transit {
    samples: VecDeque<(f64, f64)>,
    highs: VecDeque<(f64, f64)>,
}

impl Transit {
    fn add(&mut self, arrival: f64, transit: f64) {
        // Keep a running minimum: samples above a newer one can't be it.
        while self.samples.back().is_some_and(|(_, t)| *t >= transit) {
            self.samples.pop_back();
        }
        self.samples.push_back((arrival, transit));
        while self
            .samples
            .front()
            .is_some_and(|(at, _)| arrival - at > WINDOW)
        {
            self.samples.pop_front();
        }
        // ...and a running maximum, without stalls.
        let floor = self.samples.front().map_or(transit, |(_, t)| *t);
        if transit <= floor + MAX_RISE {
            while self.highs.back().is_some_and(|(_, t)| *t <= transit) {
                self.highs.pop_back();
            }
            self.highs.push_back((arrival, transit));
        }
        while self
            .highs
            .front()
            .is_some_and(|(at, _)| arrival - at > JITTER_WINDOW)
        {
            self.highs.pop_front();
        }
    }

    /// The largest recent transit (stalls aside), unless gone quiet.
    fn maximum(&self, now: f64, last: Option<f64>) -> Option<f64> {
        if last.is_none_or(|last| now - last > STALE) {
            return None;
        }
        self.highs.front().map(|(_, t)| *t)
    }

    /// The smallest recent transit, unless the stream has gone quiet.
    fn minimum(&self, now: f64, last: Option<f64>) -> Option<f64> {
        if last.is_none_or(|last| now - last > STALE) {
            return None;
        }
        self.samples.front().map(|(_, t)| *t)
    }
}

/// Which stream a packet belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
}

/// The shared playout clock of a viewer's picture and sound.
#[derive(Debug)]
pub struct Playout {
    start: Instant,
    video: Transit,
    audio: Transit,
    video_last: Option<f64>,
    audio_last: Option<f64>,
    delay: Option<f64>,
    updated: f64,
    /// Added to the margin: how far ahead the sound output asks.
    extra: f64,
}

impl Default for Playout {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl Playout {
    pub fn new(start: Instant) -> Self {
        Self {
            start,
            video: Transit::default(),
            audio: Transit::default(),
            video_last: None,
            audio_last: None,
            delay: None,
            updated: 0.0,
            extra: 0.0,
        }
    }

    /// `at` in this clock's seconds.
    pub fn seconds(&self, at: Instant) -> f64 {
        at.saturating_duration_since(self.start).as_secs_f64()
    }

    /// The instant of `seconds` on this clock.
    pub fn instant(&self, seconds: f64) -> Instant {
        self.start + Duration::from_secs_f64(seconds.max(0.0))
    }

    /// A packet of `kind` drawn at sender time `sender` arrived at
    /// `arrival` (this clock): when to play it here, or none when it's
    /// more than [`MAX_RISE`] too late (a stall: show it now, or drop
    /// sound, rather than delay everything after it).
    pub fn schedule(&mut self, kind: Kind, sender: f64, arrival: f64) -> Option<f64> {
        let transit = arrival - sender;
        match kind {
            Kind::Video => {
                self.video.add(arrival, transit);
                self.video_last = Some(arrival);
            }
            Kind::Audio => {
                self.audio.add(arrival, transit);
                self.audio_last = Some(arrival);
            }
        }
        let slowest = [
            self.video.minimum(arrival, self.video_last),
            self.audio.minimum(arrival, self.audio_last),
        ]
        .into_iter()
        .flatten()
        .fold(f64::MIN, f64::max);
        // The slower stream's fastest packets plus the margin; a packet
        // later than that raises it enough to play this one on time.
        // Sound must also reach the output that far ahead of playing.
        let needed = transit
            + ON_TIME
            + match kind {
                Kind::Audio => self.extra,
                Kind::Video => 0.0,
            };
        // Sound must also cover its own slowest recent arrivals (with the
        // output's read-ahead), or the output runs short when a burst is
        // late: on a Windows client sound comes in bursts up to ~60 ms.
        let sound_late = self
            .audio
            .maximum(arrival, self.audio_last)
            .map_or(f64::MIN, |high| high + ON_TIME);
        let base = (slowest + MARGIN).max(sound_late) + self.extra;
        if needed > base + MAX_RISE {
            return None;
        }
        let target = base.max(needed);
        // At once only when this packet would otherwise be late; else
        // gently either way, so the picture and sound already scheduled
        // keep their places. Never more than MAX_RISE above the base.
        let step = FALL * (arrival - self.updated).max(0.0);
        let delay = match self.delay {
            Some(delay) if needed > delay => target,
            Some(delay) if target < delay => (delay - step).max(target),
            Some(delay) => (delay + step).min(target),
            None => target,
        }
        .min(base + MAX_RISE);
        self.delay = Some(delay);
        self.updated = arrival;
        Some(sender + delay)
    }

    /// How far the sound's slowest recent arrival is behind its fastest,
    /// in seconds (its jitter), if sound flows.
    pub fn sound_jitter(&self) -> Option<f64> {
        let high = self.audio.maximum(self.updated, self.audio_last)?;
        let low = self.audio.minimum(self.updated, self.audio_last)?;
        Some(high - low)
    }

    /// The sound stopped playing here: only the picture's transit counts
    /// from now on (at once, not falling slowly).
    pub fn forget_sound(&mut self) {
        self.audio = Transit::default();
        self.audio_last = None;
        self.extra = 0.0;
        self.delay = None;
    }

    /// Also cover `seconds` more (the sound output reads that far ahead).
    pub fn set_extra(&mut self, seconds: f64) {
        self.extra = seconds.clamp(0.0, 0.5);
    }

    /// How long pictures wait beyond their own smallest transit (for the
    /// sound, plus the margin), and the sound's transit beyond the
    /// picture's: what lip-sync costs. None until both streams flow.
    pub fn waits(&self) -> Option<(f64, f64)> {
        let video = self.video.minimum(self.updated, self.video_last)?;
        let audio = self.audio.minimum(self.updated, self.audio_last)?;
        Some((self.delay? - video, audio - video))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_maps_through_the_sender_report_across_a_wrap() {
        let report = Report {
            rtp: u32::MAX - 899,
            wallclock: 100.0,
        };
        // 1800 ticks later at 90 kHz, past the 32-bit wrap: 20 ms.
        assert!((report.wallclock_of(900, 90_000) - 100.02).abs() < 1e-9);
        // Earlier than the report.
        assert!((report.wallclock_of(u64::from(u32::MAX - 4499), 90_000) - 99.96).abs() < 1e-9);
    }

    #[test]
    fn the_picture_waits_for_the_slower_sound() {
        let mut playout = Playout::new(Instant::now());
        // Video arrives 25 ms after it was drawn, sound 214 ms after.
        let mut video_at = 0.0;
        let mut audio_at = 0.0;
        for i in 0..100 {
            let s = 10.0 + f64::from(i) * 0.01;
            video_at = playout.schedule(Kind::Video, s, s - 10.0 + 0.025).unwrap();
            audio_at = playout.schedule(Kind::Audio, s, s - 10.0 + 0.214).unwrap();
        }
        // The same moment plays at the same time: sound's transit plus the
        // margin after it was made.
        let s = 10.0 + 0.99;
        assert!((audio_at - video_at).abs() < 1e-9);
        assert!((video_at - (s - 10.0 + 0.214 + MARGIN)).abs() < 1e-6);
        let (wait, skew) = playout.waits().unwrap();
        assert!((wait - (0.189 + MARGIN)).abs() < 1e-6 && (skew - 0.189).abs() < 1e-6);
    }

    #[test]
    fn delay_rises_at_once_and_falls_slowly() {
        let mut playout = Playout::new(Instant::now());
        let delay = |p: &mut Playout, s: f64, a: f64| p.schedule(Kind::Video, s, a).unwrap() - s;
        assert!((delay(&mut playout, 0.0, 0.05) - (0.05 + MARGIN)).abs() < 1e-9);
        // A late packet: up at once, enough to play it on time.
        assert!((delay(&mut playout, 0.1, 0.25) - (0.15 + ON_TIME)).abs() < 1e-9);
        // Back to 50 ms transits: it falls, at most FALL a second.
        let before = delay(&mut playout, 3.2, 3.25);
        let after = delay(&mut playout, 4.2, 4.25);
        assert!(after < before && before - after <= FALL * 1.0 + 1e-9);
    }

    /// The engine's two tracks as the client sees them: each with its own
    /// RTP base, rate and 32-bit wrap, a Sender Report a second, network
    /// jitter, and one picture 150 ms late. The SR mapping must recover the
    /// capture time exactly, nothing may be scheduled before it arrives,
    /// and a picture and the sound captured with it play together.
    #[test]
    fn synthetic_sender_reports_with_jitter_and_a_late_packet() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut jitter = move |range: f64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % 10_000) as f64 / 10_000.0 * range
        };
        let wall0 = 1_792_740_000.0;
        // Bases near the wrap, so both tracks wrap during the run.
        let (video_base, audio_base) = (u32::MAX - 90_000 * 3, u32::MAX - 48_000 * 7);
        let rtp = |base: u32, rate: f64, t: f64| u64::from(base.wrapping_add((t * rate) as u32));
        let report = |base: u32, rate: f64, t: f64| Report {
            rtp: base.wrapping_add((t * rate) as u32),
            wallclock: wall0 + t,
        };
        let mut playout = Playout::new(Instant::now());
        let mut worst_pair = 0.0f64;
        let (mut video_report, mut audio_report) = (None, None);
        for tick in 0..300 {
            // 30 fps pictures; sound packets at the same instants.
            let t = f64::from(tick) / 30.0;
            if tick % 30 == 0 {
                video_report = Some(report(video_base, 90_000.0, t));
                audio_report = Some(report(audio_base, 48_000.0, t));
            }
            let video_rtp = rtp(video_base, 90_000.0, t);
            let audio_rtp = rtp(audio_base, 48_000.0, t);
            let video_sender = video_report.unwrap().wallclock_of(video_rtp, 90_000);
            let audio_sender = audio_report.unwrap().wallclock_of(audio_rtp, 48_000);
            assert!((video_sender - (wall0 + t)).abs() < 1e-4, "video at {t}");
            assert!((audio_sender - (wall0 + t)).abs() < 1e-4, "audio at {t}");
            // Arrival on this machine's clock (its own epoch): 20 ms ± 10
            // for pictures, 25 ms ± 10 for sound, one picture 150 ms late.
            let late = if tick == 150 { 0.150 } else { 0.0 };
            let video_arrival = t + 0.020 + jitter(0.010) + late;
            let audio_arrival = t + 0.025 + jitter(0.010);
            let video_at = playout
                .schedule(Kind::Video, video_sender, video_arrival)
                .unwrap();
            let audio_at = playout
                .schedule(Kind::Audio, audio_sender, audio_arrival)
                .unwrap();
            assert!(
                video_at >= video_arrival + ON_TIME - 1e-6,
                "picture late at {t}"
            );
            assert!(
                audio_at >= audio_arrival + ON_TIME - 1e-6,
                "sound late at {t}"
            );
            if tick != 150 {
                worst_pair = worst_pair.max((video_at - audio_at).abs());
            }
        }
        // The pair shares one delay; between the two calls it can only
        // fall (at most FALL per second of arrival difference).
        assert!(worst_pair < 0.001, "picture and sound {worst_pair} s apart");
    }

    /// A one-second stall: its packets are too late to wait for (none),
    /// the delay doesn't take on the stall, and the next packets play at
    /// the usual delay.
    #[test]
    fn a_stall_does_not_add_its_length_to_the_delay() {
        let mut playout = Playout::new(Instant::now());
        for i in 0..90 {
            let s = f64::from(i) / 30.0;
            playout.schedule(Kind::Video, s, s + 0.02).unwrap();
        }
        // Three seconds in, a second of pictures arrives at once.
        for i in 90..120 {
            let s = f64::from(i) / 30.0;
            let late = playout.schedule(Kind::Video, s, 4.0 + 0.02);
            assert!(late.is_none() || late.unwrap() - s < 0.02 + MARGIN + MAX_RISE + 1e-9);
        }
        let s = 4.0;
        let at = playout.schedule(Kind::Video, s, s + 0.02).unwrap();
        assert!(at - s < 0.02 + MARGIN + MAX_RISE + 1e-9, "delay {}", at - s);
        let at = playout.schedule(Kind::Video, 4.1, 4.12).unwrap();
        assert!(at - 4.1 <= 0.02 + MARGIN + MAX_RISE);
    }

    /// Windows-client sound: 10 ms packets 20 ms in transit, with a burst
    /// 60 ms late every 10 s. After the first, every packet's time leaves
    /// at least the read-ahead between its arrival and its playing (no
    /// short buffers); the jitter is held between bursts.
    #[test]
    fn sound_bursts_are_covered_after_the_first() {
        let mut playout = Playout::new(Instant::now());
        let lead = 0.050;
        playout.set_extra(lead);
        let mut short_after_first = 0;
        for i in 0..6000 {
            let s = f64::from(i) * 0.010;
            let late = if i % 1000 >= 990 {
                0.060 - 0.006 * f64::from(i % 1000 - 990)
            } else {
                0.0
            };
            let arrival = s + 0.020 + late;
            let at = playout.schedule(Kind::Audio, s, arrival).unwrap();
            if i > 1000 && at - arrival < lead {
                short_after_first += 1;
            }
        }
        assert_eq!(short_after_first, 0);
        let jitter = playout.sound_jitter().unwrap();
        assert!((jitter - 0.060).abs() < 1e-6, "jitter {jitter}");
    }

    #[test]
    fn quiet_sound_stops_holding_the_picture_back() {
        let mut playout = Playout::new(Instant::now());
        playout.schedule(Kind::Audio, 0.0, 0.3).unwrap();
        for i in 0..400 {
            let s = 0.01 * f64::from(i);
            playout.schedule(Kind::Video, s, s + 0.02).unwrap();
        }
        // Sound stopped at 0.3 s; four seconds on, only video's transit
        // counts (falling toward it).
        assert!(playout.schedule(Kind::Video, 4.0, 4.02).unwrap() - 4.0 < 0.3 + MARGIN);
    }
}
