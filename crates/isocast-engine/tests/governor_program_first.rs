//! The governor puts the program first: while an output takes it and it
//! misses ticks at a rate, previews give way a step a second down to
//! pausing, then come back a step per 30 s without losses. Its own test
//! binary: the governor's limits are process-wide, and other tests read
//! them; the tests here take turns.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use isocast_engine::governor::{self, Controller, Sample};
use isocast_protocol::CanvasId;
use isocast_protocol::governor::GovernorSettings;

static TURN: Mutex<()> = Mutex::new(());

/// A replay buffer (the main program protected, no files) with two
/// previews open, on a hardware renderer, everything else healthy.
fn replay_with_previews() -> Sample {
    Sample {
        canvas_fps: 30,
        active: true,
        main_protected: true,
        has_monitors: true,
        progressed: true,
        gpu_headroom: true,
        disk_behind: Some(false),
        ..Default::default()
    }
}

/// The preview limit the governor reports (None: at the viewers' rate),
/// and what a viewer asking for 30 fps gets.
fn previews() -> (Option<u32>, u32) {
    (governor::status().preview_fps, governor::monitor_fps(30))
}

/// Ticks of one controller on a synthetic clock, in seconds from its start.
struct Run {
    controller: Controller,
    settings: GovernorSettings,
    t0: Instant,
}

impl Run {
    fn new() -> Self {
        Self {
            controller: Controller::default(),
            settings: GovernorSettings::default(),
            t0: Instant::now(),
        }
    }

    fn tick(&mut self, seconds: f64, sample: &Sample) {
        let at = self.t0 + Duration::from_secs_f64(seconds);
        self.controller.tick_at(&self.settings, sample, at);
    }

    /// A miss at `seconds`, seen by that tick.
    fn miss(&mut self, seconds: f64, sample: &Sample) {
        governor::program_missed(CanvasId::Main, 1);
        self.tick(seconds, sample);
    }

    /// Tick each of `steps` (seconds, with a miss or not) and check what
    /// previews get after it.
    fn expect(&mut self, sample: &Sample, steps: &[(f64, bool, Option<u32>, u32)]) {
        for &(seconds, missed, limit, rate) in steps {
            if missed {
                self.miss(seconds, sample);
            } else {
                self.tick(seconds, sample);
            }
            let what = if missed { "a miss" } else { "quiet" };
            assert_eq!(previews(), (limit, rate), "{what} at {seconds} s");
        }
    }
}

#[test]
fn previews_give_way_to_a_missing_program_and_come_back() {
    let _turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let sample = replay_with_previews();
    let mut run = Run::new();
    run.expect(&sample, &[(0.0, false, None, 30)]);

    // One miss is a hiccup; a second within 5 s is a rate: a step, then a
    // step a second while it lasts.
    run.expect(
        &sample,
        &[
            (1.0, true, None, 30),
            (1.5, true, Some(15), 15),
            (2.0, true, Some(15), 15),
            (2.6, true, Some(5), 5),
            (3.7, true, Some(0), 0),
        ],
    );
    let status = governor::status();
    assert!(status.previews_paused, "{status:?}");
    assert_eq!(
        status.previews_paused_why(),
        Some("until the program keeps up")
    );

    // Back a step per 30 s without losses, counted from the first quiet
    // tick once fewer than two misses are left in the window (8 s).
    run.expect(
        &sample,
        &[
            (8.0, false, Some(0), 0),
            (37.9, false, Some(0), 0),
            (38.0, false, Some(5), 5),
        ],
    );
    assert!(!governor::status().previews_paused);

    // Missing again right after a restore: back down, and the next restore
    // waits 120 s from the first new miss (39 s), not 30.
    run.expect(
        &sample,
        &[
            (39.0, true, Some(5), 5),
            (39.5, true, Some(0), 0),
            (75.0, false, Some(0), 0),
            (158.9, false, Some(0), 0),
            (159.1, false, Some(5), 5),
            (189.0, false, Some(5), 5),
            (189.2, false, Some(15), 15),
            (219.0, false, Some(15), 15),
            (219.2, false, None, 30),
        ],
    );
}

#[test]
fn isolated_misses_and_an_output_opening_cost_previews_nothing() {
    let _turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let sample = replay_with_previews();

    // A hiccup every 6 s never adds up to a rate (the controller ticks
    // every 100 ms in the daemon; a quiet tick after each).
    let mut run = Run::new();
    run.tick(0.0, &sample);
    for n in 1..=10 {
        let at = f64::from(n) * 6.0;
        run.expect(
            &sample,
            &[(at, true, None, 30), (at + 0.5, false, None, 30)],
        );
    }
    drop(run);

    // Misses while an output opens, before the program delivered its first
    // frame, don't count; once it delivers, a rate does.
    let mut run = Run::new();
    let opening = Sample {
        progressed: false,
        ..replay_with_previews()
    };
    run.expect(
        &opening,
        &[
            (0.0, true, None, 30),
            (0.2, true, None, 30),
            (0.4, true, None, 30),
            (1.4, true, None, 30),
            (1.6, false, None, 30),
        ],
    );
    run.expect(
        &sample,
        &[
            (2.0, true, None, 30),
            (2.5, true, None, 30),
            (3.0, true, Some(15), 15),
        ],
    );
}

#[test]
fn without_an_output_or_with_a_pinned_rate_misses_change_nothing() {
    let _turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // With no output, previews are all the program feeds.
    let idle = Sample {
        active: false,
        main_protected: false,
        ..replay_with_previews()
    };
    let mut run = Run::new();
    for n in 0..6 {
        run.expect(&idle, &[(f64::from(n) * 0.5, true, None, 30)]);
    }
    drop(run);

    // Kept settings are kept: a pinned preview rate never gives way.
    let mut run = Run::new();
    run.settings = GovernorSettings {
        pins: vec!["preview.fps".into()],
        ..GovernorSettings::default()
    };
    let sample = replay_with_previews();
    for n in 0..6 {
        run.expect(&sample, &[(f64::from(n) * 0.5, true, None, 30)]);
    }
}
