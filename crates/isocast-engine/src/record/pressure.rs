//! Admission across the outputs sharing a process's hardware video
//! encoders: protected outputs (main program/stream, then vertical) publish
//! when they stall, and ISO files yield first. The protected side lives in
//! the platform's writers and stream encoder ([`EncoderPressure`]); ISO
//! admission happens here in the engine, before an ISO frame is drawn for
//! its file ([`IsoCredit`]), so a refused tick costs no GPU work (#29).

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use isocast_protocol::CanvasId;

/// Protected outputs (each holds an [`EncoderPressure`]).
pub static PRIORITY_OUTPUTS: AtomicUsize = AtomicUsize::new(0);
/// Protected outputs that are stalled right now.
pub static PRIORITY_PRESSURE: AtomicUsize = AtomicUsize::new(0);
/// Fraction of ISO pictures admitted, in thousandths. Reduce promptly under
/// pressure, then probe for spare capacity slowly instead of sending bursts.
static ISO_SHARE: AtomicU32 = AtomicU32::new(1000);
static MAIN_OUTPUTS: AtomicUsize = AtomicUsize::new(0);
static MAIN_PRESSURE: AtomicUsize = AtomicUsize::new(0);
static VERTICAL_SHARE: AtomicU32 = AtomicU32::new(1000);

/// ISOs yield first. Only sustained main pressure, after the ISO share has
/// fallen to a tenth, may also hold vertical work. Main-only admission is unchanged.
pub fn vertical_held() -> bool {
    MAIN_PRESSURE.load(Ordering::Relaxed) > 0 && ISO_SHARE.load(Ordering::Relaxed) <= 100
}

/// One ISO file's share of the encoders. Ask before drawing a frame for it:
/// while a protected output is stalled nothing is admitted, otherwise the
/// current ISO share of the ticks. With no protected output, everything.
#[derive(Default)]
pub struct IsoCredit(u32);

impl IsoCredit {
    /// Whether the file may take this tick's frame.
    pub fn admit(&mut self) -> bool {
        if PRIORITY_OUTPUTS.load(Ordering::Relaxed) == 0 {
            return true;
        }
        // Stop adding low-priority work immediately while a protected
        // queue catches up. The reduced steady share avoids repeatedly
        // refilling every encoder at full rate when that pause ends.
        if PRIORITY_PRESSURE.load(Ordering::Relaxed) > 0 {
            return false;
        }
        self.0 += ISO_SHARE.load(Ordering::Relaxed);
        if self.0 < 1000 {
            return false;
        }
        self.0 -= 1000;
        true
    }
}

/// Admission feedback across the native sessions sharing this process's
/// hardware encoders. Only an output's worker mutates its guard. Capture and
/// the compositor never wait; low-priority writers return a counted refusal.
pub struct EncoderPressure {
    pub canvas: CanvasId,
    stalled: bool,
    adjusted: Instant,
    credit: u32,
}

impl EncoderPressure {
    pub fn new(canvas: CanvasId) -> Self {
        if PRIORITY_OUTPUTS.fetch_add(1, Ordering::Relaxed) == 0 {
            ISO_SHARE.store(1000, Ordering::Relaxed);
        }
        if canvas.is_main() && MAIN_OUTPUTS.fetch_add(1, Ordering::Relaxed) == 0 {
            VERTICAL_SHARE.store(1000, Ordering::Relaxed);
        }
        Self {
            canvas,
            stalled: false,
            adjusted: Instant::now(),
            credit: 0,
        }
    }

    pub fn held(&self) -> bool {
        !self.canvas.is_main() && vertical_held()
    }

    pub fn admit(&mut self) -> bool {
        if self.canvas.is_main() || MAIN_OUTPUTS.load(Ordering::Relaxed) == 0 {
            return true;
        }
        if self.held() {
            return false;
        }
        self.credit += VERTICAL_SHARE.load(Ordering::Relaxed);
        if self.credit < 1000 {
            return false;
        }
        self.credit -= 1000;
        true
    }

    pub fn set(&mut self, stalled: bool) {
        let now = Instant::now();
        if stalled
            && (!self.stalled || now.duration_since(self.adjusted) >= Duration::from_millis(100))
        {
            if self.canvas.is_main() && ISO_SHARE.load(Ordering::Relaxed) <= 100 {
                let _ = VERTICAL_SHARE.try_update(Ordering::Relaxed, Ordering::Relaxed, |share| {
                    Some(share * 3 / 4)
                });
            }
            let _ = ISO_SHARE.try_update(Ordering::Relaxed, Ordering::Relaxed, |share| {
                Some(share * 3 / 4)
            });
            self.adjusted = now;
        } else if !stalled
            && PRIORITY_PRESSURE.load(Ordering::Relaxed) == 0
            && now.duration_since(self.adjusted) >= Duration::from_millis(500)
        {
            let _ = ISO_SHARE.try_update(Ordering::Relaxed, Ordering::Relaxed, |share| {
                Some((share + 10).min(1000))
            });
            if self.canvas.is_main() {
                let _ = VERTICAL_SHARE.try_update(Ordering::Relaxed, Ordering::Relaxed, |share| {
                    Some((share + 10).min(1000))
                });
            }
            self.adjusted = now;
        }
        if self.canvas.is_main()
            && !stalled
            && MAIN_PRESSURE.load(Ordering::Relaxed) == 0
            && now.duration_since(self.adjusted) >= Duration::from_millis(500)
        {
            let _ = VERTICAL_SHARE.try_update(Ordering::Relaxed, Ordering::Relaxed, |share| {
                Some((share + 10).min(1000))
            });
            self.adjusted = now;
        }
        if stalled != self.stalled {
            if stalled {
                PRIORITY_PRESSURE.fetch_add(1, Ordering::Relaxed);
                if self.canvas.is_main() {
                    MAIN_PRESSURE.fetch_add(1, Ordering::Relaxed);
                }
            } else {
                PRIORITY_PRESSURE.fetch_sub(1, Ordering::Relaxed);
                if self.canvas.is_main() {
                    MAIN_PRESSURE.fetch_sub(1, Ordering::Relaxed);
                }
            }
            self.stalled = stalled;
        }
    }
}

impl Drop for EncoderPressure {
    fn drop(&mut self) {
        self.set(false);
        PRIORITY_OUTPUTS.fetch_sub(1, Ordering::Relaxed);
        if self.canvas.is_main() {
            MAIN_OUTPUTS.fetch_sub(1, Ordering::Relaxed);
        }
    }
}
