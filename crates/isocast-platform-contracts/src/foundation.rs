//! Foundation contracts every [`Platform`] back end keeps, checked through
//! its public API only, with the same code for every back end:
//!
//! - `clock`: `now` is monotonic, its clones (on any thread) share one
//!   origin, it counts real nanoseconds, `sleep_until` never wakes before
//!   its deadline and returns at once for a past one, and frame/sample
//!   ticks map exactly onto it over two hours. Opens nothing.
//! - `pools`: a 64x48 BGRA pool is bounded (under 128 frames), draws give
//!   a row stride and buffer that hold the picture and call back exactly
//!   once per frame, a frame stays out of the pool until every clone is
//!   dropped, pressure returns `None` without drawing, one freed slot lets
//!   exactly one frame through, pools and frames move across threads, and
//!   frames outlive their pool. Opens nothing. Pixels are not read back:
//!   frames have no public pixel API (the GPU contract reads them).
//! - `gpu`: visible pixels, alias effects and lifetime through the GPU API.
//!   This does not measure whether an importer performs internal copies. A CPU-drawn
//!   frame's import reads back exactly; an uncached import keeps its frame
//!   (unchanged, out of the pool) after the frame is dropped; repeated
//!   imports don't drain the pool; a cleared render target is seen by a
//!   later import (same memory) after a real completion callback; cached
//!   targets and imports are checked when the back end has them (`None`
//!   is reported, not failed); with the importer and every texture gone,
//!   the pool gives all its frames again. A fresh uncached import stays
//!   out of the pool with only unsubmitted commands holding it (no caller
//!   frame, texture or importer); after submission and attempted pool reuse,
//!   those commands read back the original pixels and complete. Post-submit
//!   availability is observed, not proof of unfinished GPU work retaining
//!   the frame. Readback is test-only: through a copy if the texture allows
//!   `COPY_SRC`, otherwise a `textureLoad` shader pass into our own texture.
//!
//! Every check returns an error with context instead of panicking (panics
//! are caught and reported too), and measurements as JSON on success. Which
//! back ends run which contract is the caller's choice, from their declared
//! capabilities.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::pin;
use std::sync::mpsc;
use std::task::{Context as TaskContext, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use isocast_engine::gpu::{FrameImporter, Gpu, GpuFrame, GpuPixels};
use isocast_engine::{FramePool, Platform, Time};
use serde_json::{Value, json};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 48;
/// A pool that hands out this many frames without `None` is unbounded.
const MAX_ALLOCATIONS: usize = 128;
/// Work bounds inside a contract; the runner bounds the process too.
const CPU_BUDGET: Duration = Duration::from_secs(10);
const GPU_BUDGET: Duration = Duration::from_secs(20);
/// Longest wait for one GPU submission or callback.
const FENCE_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest wait for released frames to come back to a pool.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
/// Draws and takes tried while a pool is exhausted.
const PRESSURE_ATTEMPTS: usize = 4;
/// A sanity bound, not a latency target: a past deadline must not be
/// waited for, and a short one not overslept by a wrong clock domain.
const SLEEP_SANITY: Duration = Duration::from_secs(1);
const TWO_HOURS_NANOS: i64 = 2 * 3_600 * 1_000_000_000;

const SEED_DRAWN: u32 = 11;
const SEED_OVERWRITE: u32 = 97;
const SEED_CACHED: u32 = 173;
const SEED_WORK: u32 = 229;
/// Clear colours as RGBA unorm8 values (exact in a float clear).
const CLEAR_TARGET: [u8; 4] = [64, 128, 192, 255];
const CLEAR_CACHED: [u8; 4] = [200, 32, 96, 255];

// ---------------------------------------------------------------- clock

/// The host clock contract. Opens no devices or windows.
pub fn clock<P: Platform>(platform: &P) -> Result<Value> {
    contained("clock", || {
        let budget = Budget::new("clock", CPU_BUDGET);
        let origin = platform.now();
        let monotonic = monotonic(platform, &budget)?;
        let shared = shared_origin(platform, &budget)?;
        let rate = counts_nanoseconds(platform)?;
        let sleeps = sleeps(platform)?;
        budget.check()?;
        let ticks = ticks(platform)?;
        budget.check()?;
        Ok(json!({
            "first_now_ns": origin.as_nanos(),
            "monotonic": monotonic,
            "shared_origin": shared,
            "rate": rate,
            "sleep_until": sleeps,
            "ticks": ticks,
            "ticks_scope": "engine Time arithmetic at the platform's origin, not a two-hour hardware drift measurement",
            "elapsed_ms": ms(budget.started.elapsed()),
        }))
    })
}

fn monotonic<P: Platform>(platform: &P, budget: &Budget) -> Result<Value> {
    let window = Duration::from_millis(100);
    let started = Instant::now();
    let first = platform.now();
    let mut previous = first;
    let (mut samples, mut advances) = (0u64, 0u64);
    let (mut smallest, mut largest) = (i64::MAX, 0i64);
    while started.elapsed() < window && samples < 1_000_000 {
        let now = platform.now();
        samples += 1;
        ensure!(
            now >= previous,
            "now() went backwards by {} ns at sample {samples} ({} ns, then {} ns)",
            previous.as_nanos() - now.as_nanos(),
            previous.as_nanos(),
            now.as_nanos()
        );
        let step = now.as_nanos() - previous.as_nanos();
        if step > 0 {
            advances += 1;
            smallest = smallest.min(step);
            largest = largest.max(step);
        }
        previous = now;
    }
    budget.check()?;
    ensure!(
        advances > 0,
        "now() never advanced in {samples} samples over {:?}",
        started.elapsed()
    );
    Ok(json!({
        "samples": samples,
        "advances": advances,
        "smallest_step_ns": smallest,
        "largest_step_ns": largest,
        "span_ns": previous.as_nanos() - first.as_nanos(),
    }))
}

/// Clones read one clock: interleaved reads on one thread never go back,
/// and a read on another thread lies between reads that happen before and
/// after it (a channel orders them).
fn shared_origin<P: Platform>(platform: &P, budget: &Budget) -> Result<Value> {
    let clone = platform.clone();
    let clone_of_clone = clone.clone();
    const INTERLEAVED: usize = 1_000;
    for round in 0..INTERLEAVED {
        let reads = [
            platform.now(),
            clone.now(),
            clone_of_clone.now(),
            platform.now(),
        ];
        ensure!(
            reads.windows(2).all(|pair| pair[0] <= pair[1]),
            "clones of the platform disagree at round {round}: original, clone, clone of \
             clone, original read {:?} ns",
            reads.map(Time::as_nanos)
        );
    }

    const ROUNDS: usize = 64;
    let worker = platform.clone();
    let (to_worker, worker_rx) = mpsc::channel::<()>();
    let (to_main, main_rx) = mpsc::channel::<Time>();
    let (worker_thread, largest_round_trip) = thread::scope(move |scope| -> Result<_> {
        let mut largest_round_trip = 0i64;
        let handle = scope.spawn(move || {
            while worker_rx.recv().is_ok() {
                if to_main.send(worker.now()).is_err() {
                    break;
                }
            }
            format!("{:?}", thread::current().id())
        });
        for round in 0..ROUNDS {
            budget.check()?;
            let before = platform.now();
            to_worker
                .send(())
                .context("the clock thread stopped early")?;
            let seen = main_rx
                .recv_timeout(FENCE_TIMEOUT)
                .context("the clock thread didn't answer within 5 s")?;
            let after = platform.now();
            ensure!(
                before <= seen && seen <= after,
                "a clone on another thread read {} ns, outside {}..={} ns read on this \
                 thread before and after it (round {round}): the clones don't share an origin",
                seen.as_nanos(),
                before.as_nanos(),
                after.as_nanos()
            );
            largest_round_trip = largest_round_trip.max(after.as_nanos() - before.as_nanos());
        }
        drop(to_worker);
        let worker_thread = handle.join().map_err(|payload| {
            anyhow!("the clock thread panicked: {}", panic_message(&*payload))
        })?;
        Ok((worker_thread, largest_round_trip))
    })?;
    Ok(json!({
        "interleaved_rounds": INTERLEAVED,
        "cross_thread_rounds": ROUNDS,
        "worker_thread": worker_thread,
        "largest_round_trip_ns": largest_round_trip,
    }))
}

/// `now` counts real nanoseconds: the engine stamps media in nanoseconds
/// on it and the default `sleep_until` sleeps the difference with the
/// standard library. Compared with `Instant` across a 100 ms sleep, with
/// 2 ms + 5% slack for the two clock sources.
fn counts_nanoseconds<P: Platform>(platform: &P) -> Result<Value> {
    let a = Instant::now();
    let p0 = platform.now();
    let b = Instant::now();
    thread::sleep(Duration::from_millis(100));
    let c = Instant::now();
    let p1 = platform.now();
    let d = Instant::now();
    let platform_ns = (p1.as_nanos() - p0.as_nanos()) as i128;
    let inner = c.duration_since(b).as_nanos() as i128;
    let outer = d.duration_since(a).as_nanos() as i128;
    let slack = 2_000_000 + outer / 20;
    ensure!(
        platform_ns + slack >= inner && platform_ns <= outer + slack,
        "now() advanced {platform_ns} ns while Instant advanced between {inner} and {outer} ns: \
         it doesn't count nanoseconds"
    );
    Ok(json!({
        "platform_ns": platform_ns as i64,
        "instant_inner_ns": inner as i64,
        "instant_outer_ns": outer as i64,
        "slack_ns": slack as i64,
    }))
}

fn sleeps<P: Platform>(platform: &P) -> Result<Value> {
    let mut past = Vec::new();
    for (label, behind) in [("1 s ago", Duration::from_secs(1)), ("now", Duration::ZERO)] {
        let at = platform.now() - behind;
        let started = Instant::now();
        platform.sleep_until(at);
        let waited = started.elapsed();
        ensure!(
            waited < SLEEP_SANITY,
            "sleep_until a deadline {label} waited {waited:?}: past deadlines return at once"
        );
        past.push(
            json!({"deadline": label, "deadline_ns": at.as_nanos(), "waited_ms": ms(waited)}),
        );
    }
    let mut future = Vec::new();
    for ahead_ms in [1u64, 5, 20, 50] {
        let at = platform.now() + Duration::from_millis(ahead_ms);
        let started = Instant::now();
        platform.sleep_until(at);
        let after = platform.now();
        let waited = started.elapsed();
        ensure!(
            after >= at,
            "sleep_until woke {} ns before its deadline {ahead_ms} ms ahead",
            at.as_nanos() - after.as_nanos()
        );
        let overshoot = after.saturating_since(at);
        ensure!(
            overshoot < SLEEP_SANITY,
            "sleep_until {ahead_ms} ms ahead overslept by {overshoot:?}: a wrong clock domain"
        );
        future.push(json!({
            "ahead_ms": ahead_ms,
            "deadline_ns": at.as_nanos(),
            "woke_ns": after.as_nanos(),
            "overshoot_us": overshoot.as_secs_f64() * 1e6,
            "waited_ms": ms(waited),
        }));
    }
    Ok(json!({"past": past, "future": future}))
}

/// 60 fps frames and 48 kHz samples over two hours from the platform's
/// current time: each tick lands at exactly `n * 1e9 / rate` ns (floored)
/// with no accumulated error, maps back to `n` both ways, increases
/// strictly, and both rates meet at the same instants.
fn ticks<P: Platform>(platform: &P) -> Result<Value> {
    let start = platform.now();
    ensure!(
        start.as_nanos().checked_add(TWO_HOURS_NANOS).is_some(),
        "now() is {} ns: two hours later overflows the clock",
        start.as_nanos()
    );
    let mut rates = Vec::new();
    for rate in [60u32, 48_000] {
        let total = 2 * 3_600 * u64::from(rate);
        let end = start.add_ticks(total, rate);
        ensure!(
            end.as_nanos() - start.as_nanos() == TWO_HOURS_NANOS,
            "{total} ticks at {rate} Hz span {} ns, not exactly two hours",
            end.as_nanos() - start.as_nanos()
        );
        // Every tick of the first and last two seconds, a prime stride
        // between (every tick at 60 fps).
        let edge = 2 * u64::from(rate);
        let stride = if rate == 60 { 1 } else { 7_919 };
        let counts = (0..edge)
            .chain((edge..total - edge).step_by(stride))
            .chain(total - edge..=total);
        let mut previous: Option<Time> = None;
        let mut checked = 0u64;
        for n in counts {
            let at = start.add_ticks(n, rate);
            let expected = (i128::from(n) * 1_000_000_000 / i128::from(rate)) as i64;
            ensure!(
                at.as_nanos() - start.as_nanos() == expected,
                "tick {n} at {rate} Hz is {} ns after the start, expected {expected}",
                at.as_nanos() - start.as_nanos()
            );
            ensure!(
                at.ticks_since(start, rate) == n as i64
                    && start.ticks_since(at, rate) == -(n as i64),
                "tick {n} at {rate} Hz maps back to {} (and {} backwards)",
                at.ticks_since(start, rate),
                start.ticks_since(at, rate)
            );
            if let Some(previous) = previous {
                ensure!(
                    at > previous,
                    "tick {n} at {rate} Hz isn't after the one before"
                );
            }
            previous = Some(at);
            checked += 1;
        }
        rates.push(json!({"rate": rate, "ticks": total, "checked": checked}));
    }
    for frame in [0u64, 1, 59, 60, 3_600 * 60, 2 * 3_600 * 60] {
        ensure!(
            start.add_ticks(frame, 60) == start.add_ticks(frame * 800, 48_000),
            "frame {frame} at 60 fps and sample {} at 48 kHz aren't the same instant",
            frame * 800
        );
    }
    Ok(json!({"start_ns": start.as_nanos(), "span_ns": TWO_HOURS_NANOS, "rates": rates}))
}

// ---------------------------------------------------------------- pools

/// The BGRA frame pool contract. Opens no devices or windows.
pub fn pools<P: Platform>(platform: &P) -> Result<Value> {
    contained("pools", || {
        let budget = Budget::new("pools", CPU_BUDGET);
        let mut pool = platform
            .frame_pool(WIDTH, HEIGHT)
            .context("frame_pool(64, 48)")?;
        let mut log = DrawLog::default();

        let first = draw_checked(&mut pool, &mut log, SEED_DRAWN)?
            .context("a new pool's first draw returned None")?;
        let (buffer_len, stride) = log.layout.context("draw gave no buffer")?;
        check_frame(platform, &first, "a drawn frame")?;
        let taken = pool
            .take()
            .context("take")?
            .context("take returned None with one frame out of a new pool")?;
        check_frame(platform, &taken, "a taken frame")?;
        let clone = taken.clone();
        check_frame(platform, &clone, "a clone of a frame")?;
        drop(clone);

        let mut held = vec![first, taken];
        held.extend(fill(&budget, "filling the pool", || {
            draw_checked(&mut pool, &mut log, SEED_OVERWRITE)
        })?);
        let capacity = held.len();

        // Under pressure: `None`, without drawing, however often asked.
        for attempt in 0..PRESSURE_ATTEMPTS {
            ensure!(
                draw_checked(&mut pool, &mut log, SEED_OVERWRITE)?.is_none(),
                "draw {attempt} under pressure returned a frame with all {capacity} held"
            );
            ensure!(
                pool.take().context("take under pressure")?.is_none(),
                "take {attempt} under pressure returned a frame with all {capacity} held"
            );
        }

        // The last frame stays out of the pool until all its clones go,
        // one dropped on another thread.
        let last = held.pop().context("no frames held")?;
        let kept = last.clone();
        let sent = last.clone();
        drop(last);
        ensure!(
            draw_checked(&mut pool, &mut log, SEED_OVERWRITE)?.is_none(),
            "dropping a frame with two clones alive freed its slot"
        );
        let sender = platform.clone();
        let sent_size = thread::scope(|scope| {
            scope
                .spawn(move || {
                    let size = sender.frame_size(&sent);
                    drop(sent);
                    size
                })
                .join()
        })
        .map_err(|payload| anyhow!("the clone thread panicked: {}", panic_message(&*payload)))?;
        ensure!(
            sent_size == (WIDTH, HEIGHT),
            "frame_size on another thread is {sent_size:?}"
        );
        ensure!(
            draw_checked(&mut pool, &mut log, SEED_OVERWRITE)?.is_none(),
            "dropping all but one clone of a frame freed its slot"
        );
        drop(kept);
        let reused = draw_checked(&mut pool, &mut log, SEED_DRAWN)?
            .context("dropping a frame's last clone didn't free its slot")?;
        check_frame(platform, &reused, "a frame in a freed slot")?;
        held.push(reused);
        ensure!(
            draw_checked(&mut pool, &mut log, SEED_OVERWRITE)?.is_none(),
            "one freed slot let more than one frame through"
        );

        // The pool and its frames move to another thread and back.
        let mover = platform.clone();
        let (mut pool, mut held, moved) = thread::scope(|scope| {
            scope
                .spawn(move || -> Result<_> {
                    let mut log = DrawLog::default();
                    drop(held.pop().context("no frames held")?);
                    let frame = draw_checked(&mut pool, &mut log, SEED_DRAWN)?
                        .context("a freed slot gave no frame on another thread")?;
                    check_frame(&mover, &frame, "a frame drawn on another thread")?;
                    held.push(frame);
                    ensure!(
                        draw_checked(&mut pool, &mut log, SEED_OVERWRITE)?.is_none(),
                        "a full pool gave a frame on another thread"
                    );
                    Ok((pool, held, format!("{:?}", thread::current().id())))
                })
                .join()
        })
        .map_err(|payload| anyhow!("the pool thread panicked: {}", panic_message(&*payload)))??;
        ensure!(held.len() == capacity, "frames went missing across threads");

        // With every retainer gone, the whole pool is reusable.
        held.clear();
        let mut survivors = fill(&budget, "refilling the pool", || {
            draw_checked(&mut pool, &mut log, SEED_DRAWN)
        })?;
        ensure!(
            survivors.len() == capacity,
            "with every frame released the pool gave {} frames, not its {capacity}",
            survivors.len()
        );

        // Frames outlive their pool, here and on another thread.
        survivors.truncate(2);
        let survivor_clone = survivors[0].clone();
        drop(pool);
        check_frame(
            platform,
            &survivor_clone,
            "a clone after its pool was dropped",
        )?;
        for frame in &survivors {
            check_frame(platform, frame, "a frame after its pool was dropped")?;
        }
        let checker = platform.clone();
        thread::scope(|scope| {
            scope
                .spawn(move || -> Result<()> {
                    for frame in &survivors {
                        check_frame(&checker, frame, "a frame on another thread after its pool")?;
                    }
                    drop(survivors);
                    Ok(())
                })
                .join()
        })
        .map_err(|payload| {
            anyhow!("the survivor thread panicked: {}", panic_message(&*payload))
        })??;
        drop(survivor_clone);
        budget.check()?;

        Ok(json!({
            "width": WIDTH,
            "height": HEIGHT,
            "capacity": capacity,
            "stride": stride,
            "buffer_len": buffer_len,
            "draw_callbacks": log.calls,
            "pressure_attempts": PRESSURE_ATTEMPTS,
            "moved_to_thread": moved,
            "pixel_scope": "no pixel readback in the pool contract (frames have no public \
                            pixel API); the gpu contract reads drawn pixels back",
            "elapsed_ms": ms(budget.started.elapsed()),
        }))
    })
}

// ---------------------------------------------------------------- gpu

/// The GPU import contract: opens the shared GPU, the platform's importer
/// and a 64x48 BGRA pool. Readback here is test-only.
pub fn gpu<P: Platform>(platform: &P) -> Result<Value> {
    contained("gpu", || {
        let budget = Budget::new("gpu", GPU_BUDGET);
        let gpu = Gpu::new().context("Gpu::new")?;
        let opened = budget.started.elapsed();
        let mut importer = platform.frame_importer(&gpu).context("frame_importer")?;
        let mut pool = platform
            .frame_pool(WIDTH, HEIGHT)
            .context("frame_pool(64, 48)")?;
        let capacity = fill(&budget, "measuring the pool", || pool.take())?.len();
        ensure!(capacity >= 1, "a new pool gave no frame");
        let mut log = DrawLog::default();

        // A CPU-drawn frame imports exactly (its stride included).
        let drawn = draw_checked(&mut pool, &mut log, SEED_DRAWN)?
            .context("draw returned None from an idle pool")?;
        let imported = importer
            .import(&drawn)
            .context("import of a CPU-drawn BGRA frame")?;
        let (import_kind, texture) = bgra_plane(&imported, "import")?;
        let (pixels, import_readback) = read_back(&gpu, &texture)?;
        compare(&pixels, "import of a CPU-drawn frame", |x, y| {
            pattern_pixel(x, y, SEED_DRAWN)
        })?;

        // An uncached import keeps its frame, unchanged and out of the
        // pool, after the frame itself is dropped.
        drop(drawn);
        let overwritten = fill(&budget, "filling the pool while an import is alive", || {
            draw_checked(&mut pool, &mut log, SEED_OVERWRITE)
        })?;
        let free_while_imported = overwritten.len();
        ensure!(
            free_while_imported < capacity,
            "with only an imported texture holding a frame the pool gave all {capacity} \
             frames: the texture doesn't keep its frame alive"
        );
        let (pixels, _) = read_back(&gpu, &texture)?;
        compare(
            &pixels,
            "an imported texture after its frame was dropped and the pool refilled",
            |x, y| pattern_pixel(x, y, SEED_DRAWN),
        )?;
        drop(overwritten);
        drop(texture);
        drop(imported);
        poll_idle(&gpu)?;
        // Reported: an importer may let go at its next use or its drop.
        let free_after_texture_drop =
            fill(&budget, "after dropping an import", || pool.take())?.len();

        // Imports that are dropped don't drain the pool while the importer
        // lives.
        let cycles = 2 * capacity + 2;
        for cycle in 0..cycles {
            budget.check()?;
            let frame = pool.take().context("take")?.with_context(|| {
                format!(
                    "the pool ran dry at import cycle {cycle} of {cycles} (capacity \
                     {capacity}): dropped imports keep their frames"
                )
            })?;
            let shown = importer
                .import(&frame)
                .with_context(|| format!("import in cycle {cycle}"))?;
            drop(frame);
            drop(shown);
            poll_idle(&gpu)?;
        }

        // A render target cleared on the GPU, after a real completion
        // callback, is what a later import of the frame sees.
        let frame = pool
            .take()
            .context("take")?
            .context("take returned None for a target")?;
        let target = importer.target(&frame).context("target of a pool frame")?;
        check_texture(&target, "target", wgpu::TextureUsages::RENDER_ATTACHMENT)?;
        let target_usage = format!("{:?}", target.usage());
        let target_fence = clear(&gpu, &target, CLEAR_TARGET)?;
        drop(target);
        read_frame(
            &gpu,
            &mut importer,
            &frame,
            "a cleared target",
            CLEAR_TARGET,
        )?;

        // A cached target, if the back end has them; the caller holds the
        // frame through completion, as documented.
        let cached = importer.target_cached(&frame).context("target_cached")?;
        let mut cached_target = match &cached {
            None => json!({
                "available": false,
                "detail": "target_cached returned None: this back end has no cached targets",
            }),
            Some(cached) => {
                check_texture(
                    cached,
                    "target_cached",
                    wgpu::TextureUsages::RENDER_ATTACHMENT,
                )?;
                let fence = clear(&gpu, cached, CLEAR_CACHED)?;
                read_frame(
                    &gpu,
                    &mut importer,
                    &frame,
                    "a cleared cached target",
                    CLEAR_CACHED,
                )?;
                json!({"available": true, "usage": format!("{:?}", cached.usage()), "fence": fence})
            }
        };
        drop(frame);
        if cached.is_some() {
            poll_idle(&gpu)?;
            // Reported, not asserted: it doesn't retain the pool buffer,
            // but whether a pool reuses a surface a cached texture still
            // names is the back end's policy.
            let free = fill(&budget, "with a cached target alive", || pool.take())?.len();
            cached_target["free_while_alive"] = json!(free);
        }
        drop(cached);

        // A cached import, if the back end has them, holding the frame
        // until the GPU is done.
        let drawn = draw_checked(&mut pool, &mut log, SEED_CACHED)?
            .context("draw returned None for a cached import")?;
        let cached_import = match importer.import_cached(&drawn).context("import_cached")? {
            None => json!({
                "available": false,
                "detail": "import_cached returned None: this back end has no cached imports",
            }),
            Some(shown) => {
                let (kind, texture) = bgra_plane(&shown, "import_cached")?;
                let (pixels, readback) = read_back(&gpu, &texture)?;
                compare(&pixels, "a cached import of a CPU-drawn frame", |x, y| {
                    pattern_pixel(x, y, SEED_CACHED)
                })?;
                json!({"available": true, "kind": kind, "readback": readback})
            }
        };
        drop(drawn);

        // End to end: with the importer and every texture gone, the whole
        // pool is reusable.
        drop(importer);
        let released = wait_for_capacity(
            &gpu,
            &mut pool,
            capacity,
            &budget,
            "after dropping the importer and every texture",
        )?;
        let work_only_import = work_only_import(platform, &gpu, &mut pool, capacity, &budget)?;

        Ok(json!({
            "width": WIDTH,
            "height": HEIGHT,
            "gpu_open_ms": ms(opened),
            "capacity": capacity,
            "import": {
                "kind": import_kind,
                "readback": import_readback,
                "free_while_imported": free_while_imported,
                "free_after_texture_drop": free_after_texture_drop,
                "release_cycles": cycles,
            },
            "target": {"usage": target_usage, "fence": target_fence},
            "cached_target": cached_target,
            "cached_import": cached_import,
            "full_pool_after_importer_drop_ms": ms(released),
            "work_only_import": work_only_import,
            "pixel_scope": "test-only readback: COPY_SRC copy when allowed, else a \
                            textureLoad pass into our own texture; never production readback",
            "copy_scope": "visible pixels, alias effects and lifetimes; absence of internal copies is not measured",
            "elapsed_ms": ms(budget.started.elapsed()),
        }))
    })
}

/// No caller handles or importer/cache may mask command-owned retention.
/// Before submission the work cannot be done; afterwards it may finish
/// before we can inspect the pool, even if its callback hasn't run yet.
fn work_only_import<P: Platform>(
    platform: &P,
    gpu: &Gpu,
    pool: &mut P::FramePool,
    capacity: usize,
    budget: &Budget,
) -> Result<Value> {
    let mut rounds = Vec::new();
    for round in 0..3 {
        let available = fill(budget, "before command-only round", || pool.take())?.len();
        let mut result = work_only_round(platform, gpu, pool, capacity, budget, SEED_WORK + round)?;
        result["available_before_round"] = json!(available);
        rounds.push(result);
    }
    // A real, source-free render pass also recycles an application encoder;
    // an empty queue submission does not exercise that distinct retirement path.
    let independent = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("contract source-free retirement"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let retirement = clear(gpu, &independent, CLEAR_TARGET)?;
    drop(independent);
    poll_idle(gpu)?;
    let after_render = fill(budget, "after source-free render retirement", || {
        pool.take()
    })?
    .len();
    eprintln!(
        "command-only retention diagnostic: {}",
        json!({"rounds": rounds, "free_after_source_free_render": after_render})
    );
    let released = wait_for_capacity(
        gpu,
        pool,
        capacity,
        budget,
        "after source-free render retirement",
    )?;
    Ok(
        json!({"rounds": rounds, "retirement_fence": retirement, "free_after_retirement": after_render,
        "full_pool_after_retirement_ms": ms(released),
        "scope": "unsubmitted commands pin their source; submitted pixels survive attempted reuse; retirement is measured after independent render work"}),
    )
}

fn work_only_round<P: Platform>(
    platform: &P,
    gpu: &Gpu,
    pool: &mut P::FramePool,
    capacity: usize,
    budget: &Budget,
    seed: u32,
) -> Result<Value> {
    let mut importer = platform
        .frame_importer(gpu)
        .context("fresh frame_importer")?;
    let mut log = DrawLog::default();
    let frame = draw_checked(pool, &mut log, seed)?
        .context("draw returned None for command-only retention")?;
    let shown = importer
        .import(&frame)
        .context("uncached import for command-only retention")?;
    let (kind, texture) = bgra_plane(&shown, "command-only import")?;
    let (commands, readback) = encode_readback(gpu, &texture)?;
    // encode_readback has dropped its views, bind group and staging texture.
    // The readback buffer is independent memory, not a source-frame retainer.
    drop(frame);
    drop(shown);
    drop(texture);
    drop(importer);

    let overwritten = fill(
        budget,
        "with only unsubmitted commands holding the import",
        || draw_checked(pool, &mut log, SEED_OVERWRITE),
    )?;
    let free_before_submit = overwritten.len();
    ensure!(
        free_before_submit < capacity,
        "with only unsubmitted commands holding an import the pool gave all {capacity} \
         frames: encoded work doesn't keep its source frame alive"
    );
    drop(overwritten);

    let (done, finished) = mpsc::channel();
    commands.on_submitted_work_done(move || {
        let _ = done.send((Instant::now(), format!("{:?}", thread::current().id())));
    });
    let submitted = Instant::now();
    // submit consumes the public CommandBuffer; any remaining source
    // retention is internal to wgpu. Try to overwrite every available slot
    // before any explicit poll or map. Full availability is legal if work ended.
    let index = start_readback(gpu, commands)?;
    let overwritten = fill(
        budget,
        "after submission, before polling the readback",
        || draw_checked(pool, &mut log, SEED_OVERWRITE),
    )?;
    let free_after_submit_before_poll = overwritten.len();
    ensure!(
        free_after_submit_before_poll <= capacity,
        "after submission the pool gave {free_after_submit_before_poll} frames, \
         more than its idle capacity {capacity}"
    );
    let (pixels, method) = map_readback(gpu, readback, index)?;
    let read_at = Instant::now();
    let (fired, callback_thread) = finished
        .recv_timeout(FENCE_TIMEOUT)
        .context("the command-only import's completion callback never ran")?;
    compare(
        &pixels,
        "a command-only import after attempted pool reuse",
        |x, y| pattern_pixel(x, y, seed),
    )?;
    drop(pixels);
    drop(overwritten);
    let free_after_completion =
        fill(budget, "after completed command-only round", || pool.take())?.len();
    Ok(json!({
        "kind": kind,
        "readback": method,
        "seed": seed,
        "overwrite_seed": SEED_OVERWRITE,
        "capacity": capacity,
        "free_before_submit": free_before_submit,
        "free_after_submit_before_explicit_poll": free_after_submit_before_poll,
        "source_handles_and_importer_dropped_before_submit": true,
        "pixels_compared_exact": WIDTH * HEIGHT,
        "completion": {
            "callback_received": true,
            "submit_to_callback_ms": ms(fired.saturating_duration_since(submitted)),
            "submit_to_readback_ms": ms(read_at.duration_since(submitted)),
            "callback_thread": callback_thread,
        },
        "free_after_completion": free_after_completion,
        "scope": "command-only retention before submission; exact submitted readback under \
                  attempted pool reuse. Post-submit availability does not establish whether \
                  GPU work was still unfinished; callback delivery may lag GPU completion",
    }))
}

/// Import `frame` (uncached) and check every pixel is `rgba`.
fn read_frame<I: FrameImporter>(
    gpu: &Gpu,
    importer: &mut I,
    frame: &I::Frame,
    what: &str,
    rgba: [u8; 4],
) -> Result<()> {
    let shown = importer
        .import(frame)
        .with_context(|| format!("import of {what}"))?;
    let (_, texture) = bgra_plane(&shown, what)?;
    let (pixels, _) = read_back(gpu, &texture)?;
    compare(&pixels, what, |_, _| bgra(rgba))?;
    Ok(())
}

/// The one BGRA texture of an import of a 64x48 pool frame.
fn bgra_plane(frame: &GpuFrame, what: &str) -> Result<(&'static str, wgpu::Texture)> {
    ensure!(
        (frame.width, frame.height) == (WIDTH, HEIGHT),
        "{what}: imported as {}x{}, expected {WIDTH}x{HEIGHT}",
        frame.width,
        frame.height
    );
    let (kind, texture) = match &frame.pixels {
        GpuPixels::Rgb(texture) => ("rgb", texture.clone()),
        GpuPixels::Premultiplied(texture) => ("premultiplied", texture.clone()),
        GpuPixels::YCbCr { .. } => bail!("{what}: a BGRA frame was imported as Y'CbCr planes"),
    };
    check_texture(&texture, what, wgpu::TextureUsages::TEXTURE_BINDING)?;
    Ok((kind, texture))
}

fn check_texture(texture: &wgpu::Texture, what: &str, usage: wgpu::TextureUsages) -> Result<()> {
    let size = texture.size();
    ensure!(
        (size.width, size.height, size.depth_or_array_layers) == (WIDTH, HEIGHT, 1),
        "{what}: texture is {}x{}x{}, expected {WIDTH}x{HEIGHT}x1",
        size.width,
        size.height,
        size.depth_or_array_layers
    );
    ensure!(
        texture.format() == wgpu::TextureFormat::Bgra8Unorm,
        "{what}: texture format is {:?}, expected Bgra8Unorm",
        texture.format()
    );
    ensure!(
        texture.usage().contains(usage),
        "{what}: texture usages {:?} lack {usage:?}",
        texture.usage()
    );
    Ok(())
}

/// Clear `target` to `rgba` and wait for the submission's own completion
/// callback (after a `PollType::Wait` for it).
fn clear(gpu: &Gpu, target: &wgpu::Texture, rgba: [u8; 4]) -> Result<Value> {
    let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let view = target.create_view(&Default::default());
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    let unit = |value: u8| f64::from(value) / 255.0;
    drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("contract clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: unit(rgba[0]),
                    g: unit(rgba[1]),
                    b: unit(rgba[2]),
                    a: unit(rgba[3]),
                }),
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    }));
    let commands = encoder.finish();
    let (done, finished) = mpsc::channel();
    commands.on_submitted_work_done(move || {
        let _ = done.send((Instant::now(), format!("{:?}", thread::current().id())));
    });
    let submitted = Instant::now();
    let index = gpu.queue.submit([commands]);
    if let Some(error) = block_on(scope.pop()) {
        bail!("clearing a target failed validation: {error}");
    }
    gpu.device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(index),
            timeout: Some(FENCE_TIMEOUT),
        })
        .context("waiting for the clear to finish")?;
    let polled = Instant::now();
    // Another thread polling the shared device may run the callback; wait
    // for it either way.
    let (fired_by_poll, (fired, callback_thread)) = match finished.try_recv() {
        Ok(fired) => (true, fired),
        Err(_) => (
            false,
            finished
                .recv_timeout(FENCE_TIMEOUT)
                .context("the clear's completion callback never ran")?,
        ),
    };
    Ok(json!({
        "submit_to_callback_ms": ms(fired.saturating_duration_since(submitted)),
        "submit_to_poll_return_ms": ms(polled.duration_since(submitted)),
        "callback_ran_before_poll_returned": fired_by_poll,
        "callback_thread": callback_thread,
        "polling_thread": format!("{:?}", thread::current().id()),
    }))
}

const LOAD_SHADER: &str = r#"
@group(0) @binding(0) var source: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let corner = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(source, vec2<i32>(position.xy), 0);
}
"#;

/// A Bgra8Unorm texture's pixels, tightly packed BGRA rows, and how they
/// were read. Test-only.
fn read_back(gpu: &Gpu, texture: &wgpu::Texture) -> Result<(Vec<u8>, &'static str)> {
    let (commands, readback) = encode_readback(gpu, texture)?;
    let index = start_readback(gpu, commands)?;
    map_readback(gpu, readback, index)
}

/// Only our destination buffer and its layout, never the source texture.
struct Readback {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    row: u32,
    method: &'static str,
}

/// Record the entire readback without submitting any GPU work. Local
/// texture/view/bind-group handles are gone when this returns; recorded
/// commands must retain everything needed to read the source later.
fn encode_readback(gpu: &Gpu, texture: &wgpu::Texture) -> Result<(wgpu::CommandBuffer, Readback)> {
    let device = &gpu.device;
    let usage = texture.usage();
    let size = texture.size();
    let (width, height) = (size.width, size.height);
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let mut encoder = device.create_command_encoder(&Default::default());
    let staging;
    let (source, method) = if usage.contains(wgpu::TextureUsages::COPY_SRC) {
        (texture, "copy")
    } else if usage.contains(wgpu::TextureUsages::TEXTURE_BINDING) {
        staging = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("contract readback"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        load_into(gpu, &mut encoder, texture, &staging);
        (&staging, "texture_load_pass_then_copy")
    } else {
        bail!("texture usages {usage:?} allow neither COPY_SRC nor TEXTURE_BINDING: unobservable");
    };
    let row = (width * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("contract readback"),
        size: u64::from(row) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        source.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(height),
            },
        },
        size,
    );
    let commands = encoder.finish();
    if let Some(error) = block_on(scope.pop()) {
        bail!("encoding a texture readback failed validation: {error}");
    }
    Ok((
        commands,
        Readback {
            buffer,
            width,
            height,
            row,
            method,
        },
    ))
}

fn start_readback(gpu: &Gpu, commands: wgpu::CommandBuffer) -> Result<wgpu::SubmissionIndex> {
    let scope = gpu.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let index = gpu.queue.submit([commands]);
    if let Some(error) = block_on(scope.pop()) {
        bail!("submitting a texture readback failed validation: {error}");
    }
    Ok(index)
}

/// Consume the destination buffer so it is unmapped and dropped before
/// checking that the source's pool is fully reusable.
fn map_readback(
    gpu: &Gpu,
    readback: Readback,
    index: wgpu::SubmissionIndex,
) -> Result<(Vec<u8>, &'static str)> {
    let Readback {
        buffer,
        width,
        height,
        row,
        method,
    } = readback;
    let device = &gpu.device;
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let slice = buffer.slice(..);
    let (mapped_tx, mapped_rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = mapped_tx.send(result);
    });
    if let Some(error) = block_on(scope.pop()) {
        bail!("reading a texture back failed validation: {error}");
    }
    device
        .poll(wgpu::PollType::Wait {
            submission_index: Some(index),
            timeout: Some(FENCE_TIMEOUT),
        })
        .context("waiting for the readback")?;
    mapped_rx
        .recv_timeout(FENCE_TIMEOUT)
        .context("the readback buffer was never mapped")?
        .context("mapping the readback buffer")?;
    let mapped = slice
        .get_mapped_range()
        .context("reading the mapped buffer")?;
    let line = width as usize * 4;
    let mut pixels = Vec::with_capacity(line * height as usize);
    for y in 0..height as usize {
        let start = y * row as usize;
        pixels.extend_from_slice(&mapped[start..start + line]);
    }
    drop(mapped);
    buffer.unmap();
    Ok((pixels, method))
}

/// Encode copying `source` texel for texel into `target` (both Bgra8Unorm)
/// with `textureLoad`, for textures that can't be copied from.
fn load_into(
    gpu: &Gpu,
    encoder: &mut wgpu::CommandEncoder,
    source: &wgpu::Texture,
    target: &wgpu::Texture,
) {
    let device = &gpu.device;
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("contract readback"),
        source: wgpu::ShaderSource::Wgsl(LOAD_SHADER.into()),
    });
    let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("contract readback"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        }],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("contract readback"),
        bind_group_layouts: &[Some(&layout)],
        ..Default::default()
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("contract readback"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: Some("vs"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Bgra8Unorm,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    });
    let source_view = source.create_view(&Default::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("contract readback"),
        layout: &layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(&source_view),
        }],
    });
    let target_view = target.create_view(&Default::default());
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("contract readback"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &target_view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                store: wgpu::StoreOp::Store,
            },
        })],
        ..Default::default()
    });
    pass.set_pipeline(&pipeline);
    pass.set_bind_group(0, &bind_group, &[]);
    pass.draw(0..3, 0..1);
}

fn poll_idle(gpu: &Gpu) -> Result<()> {
    gpu.device
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(FENCE_TIMEOUT),
        })
        .context("waiting for the GPU to go idle")?;
    Ok(())
}

/// Wait (polling the GPU) until the pool gives all `capacity` frames.
fn wait_for_capacity<F: FramePool>(
    gpu: &Gpu,
    pool: &mut F,
    capacity: usize,
    budget: &Budget,
    what: &str,
) -> Result<Duration> {
    let started = Instant::now();
    loop {
        poll_idle(gpu)?;
        let free = fill(budget, what, || pool.take())?.len();
        if free == capacity {
            return Ok(started.elapsed());
        }
        ensure!(
            free < capacity,
            "{what}: the pool gave {free} frames, more than the {capacity} it gave idle"
        );
        ensure!(
            started.elapsed() < RELEASE_TIMEOUT,
            "{what}: only {free} of {capacity} frames came back within {RELEASE_TIMEOUT:?}: \
             something still holds the rest"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// Every 64x48 pixel of `pixels` (tight BGRA rows) against `expected`.
fn compare(pixels: &[u8], what: &str, expected: impl Fn(u32, u32) -> [u8; 4]) -> Result<()> {
    let total = WIDTH as usize * HEIGHT as usize;
    ensure!(
        pixels.len() == total * 4,
        "{what}: read {} bytes, expected {}",
        pixels.len(),
        total * 4
    );
    let mut wrong = 0usize;
    let mut first = None;
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let at = (y as usize * WIDTH as usize + x as usize) * 4;
            let got = &pixels[at..at + 4];
            let want = expected(x, y);
            if got != want {
                wrong += 1;
                first.get_or_insert((x, y, [got[0], got[1], got[2], got[3]], want));
            }
        }
    }
    if let Some((x, y, got, want)) = first {
        bail!(
            "{what}: {wrong} of {total} pixels differ; first at ({x}, {y}): BGRA {got:?}, \
             expected {want:?}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- shared

/// What the draw callbacks saw.
#[derive(Default)]
struct DrawLog {
    calls: usize,
    layout: Option<(usize, usize)>,
    problem: Option<String>,
}

/// `pool.draw`, writing the `seed` pattern after checking the buffer holds
/// a 64x48 BGRA picture at its stride, and that the callback ran once for
/// a frame and never for `None`.
fn draw_checked<F: FramePool>(
    pool: &mut F,
    log: &mut DrawLog,
    seed: u32,
) -> Result<Option<F::Frame>> {
    let before = log.calls;
    let frame = pool
        .draw(&mut |pixels, stride| {
            log.calls += 1;
            log.layout = Some((pixels.len(), stride));
            match layout_problem(pixels.len(), stride) {
                Some(problem) => {
                    log.problem.get_or_insert(problem);
                }
                None => write_pattern(pixels, stride, seed),
            }
        })
        .context("draw")?;
    if let Some(problem) = log.problem.take() {
        bail!("draw: {problem}");
    }
    let calls = log.calls - before;
    match &frame {
        Some(_) => ensure!(
            calls == 1,
            "draw returned a frame after calling back {calls} times, not once"
        ),
        None => ensure!(
            calls == 0,
            "draw returned None after calling back {calls} times: nothing was drawn"
        ),
    }
    Ok(frame)
}

fn layout_problem(len: usize, stride: usize) -> Option<String> {
    let line = WIDTH as usize * 4;
    if stride < line {
        return Some(format!(
            "stride {stride} is under a {WIDTH}-pixel BGRA row ({line})"
        ));
    }
    let needed = stride
        .saturating_mul(HEIGHT as usize - 1)
        .saturating_add(line);
    (len < needed).then(|| {
        format!("buffer of {len} bytes can't hold {HEIGHT} rows at stride {stride} ({needed})")
    })
}

fn write_pattern(pixels: &mut [u8], stride: usize, seed: u32) {
    for y in 0..HEIGHT {
        let row = &mut pixels[y as usize * stride..][..WIDTH as usize * 4];
        for (x, pixel) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            pixel.copy_from_slice(&pattern_pixel(x as u32, y, seed));
        }
    }
}

/// A distinct opaque BGRA value per pixel and row (a wrong stride or row
/// order shows), different for each seed.
fn pattern_pixel(x: u32, y: u32, seed: u32) -> [u8; 4] {
    [
        x.wrapping_mul(4).wrapping_add(seed) as u8,
        y.wrapping_mul(5).wrapping_add(seed.wrapping_mul(3)) as u8,
        (x.wrapping_add(y.wrapping_mul(7)) ^ seed) as u8,
        255,
    ]
}

/// An RGBA value as BGRA bytes.
fn bgra(rgba: [u8; 4]) -> [u8; 4] {
    [rgba[2], rgba[1], rgba[0], rgba[3]]
}

fn check_frame<P: Platform>(platform: &P, frame: &P::Frame, what: &str) -> Result<()> {
    let size = platform.frame_size(frame);
    ensure!(
        size == (WIDTH, HEIGHT),
        "{what}: frame_size is {}x{}, expected {WIDTH}x{HEIGHT}",
        size.0,
        size.1
    );
    ensure!(
        !platform.frame_is_ycbcr(frame),
        "{what}: frame_is_ycbcr is true for a BGRA pool frame"
    );
    Ok(())
}

/// Frames from `next` until it returns `None`, at most [`MAX_ALLOCATIONS`]
/// attempts.
fn fill<T>(
    budget: &Budget,
    what: &str,
    mut next: impl FnMut() -> Result<Option<T>>,
) -> Result<Vec<T>> {
    let mut held = Vec::new();
    for _ in 0..MAX_ALLOCATIONS {
        budget.check()?;
        match next().with_context(|| format!("{what}, frame {}", held.len()))? {
            Some(frame) => held.push(frame),
            None => return Ok(held),
        }
    }
    bail!(
        "{what}: {MAX_ALLOCATIONS} attempts all returned frames; a pool must be bounded and \
         return None under pressure"
    )
}

struct Budget {
    what: &'static str,
    started: Instant,
    limit: Duration,
}

impl Budget {
    fn new(what: &'static str, limit: Duration) -> Self {
        Self {
            what,
            started: Instant::now(),
            limit,
        }
    }

    fn check(&self) -> Result<()> {
        ensure!(
            self.started.elapsed() <= self.limit,
            "the {} contract ran past its {:?} work budget",
            self.what,
            self.limit
        );
        Ok(())
    }
}

/// Run a contract, turning a panic into an error.
fn contained(name: &str, check: impl FnOnce() -> Result<Value>) -> Result<Value> {
    match catch_unwind(AssertUnwindSafe(check)) {
        Ok(result) => result.with_context(|| format!("{name} contract")),
        Err(payload) => Err(anyhow!(
            "{name} contract panicked: {}",
            panic_message(&*payload)
        )),
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-text panic".to_owned())
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

/// The engine's own busy-poll for wgpu's futures (error scopes resolve at
/// once on native back ends).
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = TaskContext::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        thread::yield_now();
    }
}
