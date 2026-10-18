//! Admission across the outputs sharing a process's hardware video
//! encoders: protected outputs (main program/stream, then vertical) publish
//! when they stall, and ISO files yield first. The protected side lives in
//! the platform's writers and stream encoder ([`EncoderPressure`]); ISO
//! admission happens here in the engine, before an ISO frame is drawn for
//! its file ([`IsoCredit`]), so a refused tick costs no GPU work (#29).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
static STALL_EVENTS: AtomicU64 = AtomicU64::new(0);

pub fn snapshot() -> (usize, u32, u64) {
    (
        PRIORITY_PRESSURE.load(Ordering::Relaxed),
        if PRIORITY_OUTPUTS.load(Ordering::Relaxed) == 0 {
            100
        } else {
            ISO_SHARE.load(Ordering::Relaxed) / 10
        },
        STALL_EVENTS.load(Ordering::Relaxed),
    )
}

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
    pub fn admit(&mut self, ceiling: u32) -> bool {
        // Stop adding low-priority work immediately while a protected
        // queue catches up. The reduced steady share avoids repeatedly
        // refilling every encoder at full rate when that pause ends.
        let protected = PRIORITY_OUTPUTS.load(Ordering::Relaxed) > 0;
        if protected && PRIORITY_PRESSURE.load(Ordering::Relaxed) > 0 {
            return false;
        }
        let native = if protected {
            ISO_SHARE.load(Ordering::Relaxed)
        } else {
            1000
        };
        self.0 += native.min(ceiling);
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
    refused: u64,
    governor_pin: Arc<AtomicBool>,
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
            refused: 0,
            governor_pin: crate::governor::secondary_pin(canvas),
        }
    }

    pub fn held(&self) -> bool {
        !self.canvas.is_main() && vertical_held()
    }

    /// Cumulative intentional refusals owned by this encoder. Attribute
    /// these at admission, not by reading a possibly restored ceiling later.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    pub fn admit(&mut self) -> bool {
        self.admit_with_native(true)
    }

    /// ProRes has its own encoder resource: apply only the explicit governor
    /// ceiling, not the H.26x native secondary share. Main remains protected.
    pub fn admit_governor(&mut self) -> bool {
        self.admit_with_native(false)
    }

    fn admit_with_native(&mut self, shared_native: bool) -> bool {
        if self.canvas.is_main() {
            return true;
        }
        if shared_native && self.held() {
            self.refused += 1;
            return false;
        }
        let native = if !shared_native || MAIN_OUTPUTS.load(Ordering::Relaxed) == 0 {
            1000
        } else {
            VERTICAL_SHARE.load(Ordering::Relaxed)
        };
        self.credit += native.min(crate::governor::secondary_ceiling(&self.governor_pin));
        if self.credit < 1000 {
            self.refused += 1;
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
            STALL_EVENTS.fetch_add(1, Ordering::Relaxed);
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
