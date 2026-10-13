//! What both filters need without the standard library (an Isocast
//! component gets no WASI, so it is built `no_std`): an allocator, a panic
//! that traps, the bit of maths they use, and reading params.

#![allow(dead_code)]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ffi::c_void;

use crate::isocast::plugin::types::{Param, Value};

/// Blocks of 16 bytes to 1 GB in powers of two, each size with a free
/// list, carved from memory grown as needed. Small and fast; a component
/// runs on one thread.
pub struct Allocator(UnsafeCell<Heap>);

struct Heap {
    free: [usize; CLASSES],
    next: usize,
    end: usize,
}

const CLASSES: usize = 27;
const PAGE: usize = 65_536;

// SAFETY: a component instance runs on one thread.
unsafe impl Sync for Allocator {}

impl Allocator {
    pub const fn new() -> Self {
        Self(UnsafeCell::new(Heap {
            free: [0; CLASSES],
            next: 0,
            end: 0,
        }))
    }
}

/// The size class holding `size` bytes: blocks of `16 << class`.
fn class(size: usize) -> usize {
    let size = size.max(16);
    (usize::BITS - (size - 1).leading_zeros()) as usize - 4
}

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: one thread, and no allocation re-enters.
        let heap = unsafe { &mut *self.0.get() };
        let class = class(layout.size().max(layout.align()));
        if class >= CLASSES {
            return core::ptr::null_mut();
        }
        let head = heap.free[class];
        if head != 0 {
            // SAFETY: a free block holds the next free block's address.
            heap.free[class] = unsafe { *(head as *const usize) };
            return head as *mut u8;
        }
        if heap.end == 0 {
            heap.end = core::arch::wasm32::memory_size(0) * PAGE;
            heap.next = heap.end;
        }
        // Blocks sit at multiples of their size, so they are aligned for
        // anything that fits.
        let size = 16 << class;
        let start = (heap.next + size - 1) & !(size - 1);
        if start + size > heap.end {
            let pages = (start + size - heap.end).div_ceil(PAGE);
            if core::arch::wasm32::memory_grow(0, pages) == usize::MAX {
                return core::ptr::null_mut();
            }
            heap.end += pages * PAGE;
        }
        heap.next = start + size;
        start as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: one thread; `ptr` came from `alloc` with this layout.
        let heap = unsafe { &mut *self.0.get() };
        let class = class(layout.size().max(layout.align()));
        unsafe { *(ptr as *mut usize) = heap.free[class] };
        heap.free[class] = ptr as usize;
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if class(layout.size().max(layout.align())) == class(new_size.max(layout.align())) {
            return ptr;
        }
        // SAFETY: as GlobalAlloc's own default.
        unsafe {
            let new = self.alloc(Layout::from_size_align_unchecked(new_size, layout.align()));
            if !new.is_null() {
                core::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
            new
        }
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator::new();

/// A filter's state, kept between calls (one thread, no re-entry).
pub struct Global<T>(UnsafeCell<T>);

// SAFETY: a component instance runs on one thread.
unsafe impl<T> Sync for Global<T> {}

impl<T> Global<T> {
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    /// The state. Each export takes it once and lets go before returning.
    #[allow(clippy::mut_from_ref)]
    pub fn get(&self) -> &mut T {
        // SAFETY: one thread, and exports don't call each other.
        unsafe { &mut *self.0.get() }
    }
}

/// The allocator the component model's calls use to pass lists and
/// strings in (the C library has it otherwise).
///
/// # Safety
///
/// As `realloc`: `old_ptr` is null with `old_len` 0, or an allocation of
/// `old_len` bytes aligned to `align`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cabi_realloc(
    old_ptr: *mut u8,
    old_len: usize,
    align: usize,
    new_len: usize,
) -> *mut u8 {
    if new_len == 0 {
        return align as *mut u8;
    }
    // SAFETY: as the caller promises.
    let new = unsafe {
        if old_len == 0 {
            alloc::alloc::alloc(Layout::from_size_align_unchecked(new_len, align))
        } else {
            let old = Layout::from_size_align_unchecked(old_len, align);
            alloc::alloc::realloc(old_ptr, old, new_len)
        }
    };
    if new.is_null() {
        core::arch::wasm32::unreachable();
    }
    new
}

/// Byte comparison, which comparing strings needs and which comes from the
/// C library otherwise. Volatile reads keep the compiler from turning the
/// loop back into a call to itself.
///
/// # Safety
///
/// `a` and `b` must be readable for `n` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn memcmp(a: *const c_void, b: *const c_void, n: usize) -> i32 {
    let (a, b) = (a.cast::<u8>(), b.cast::<u8>());
    for i in 0..n {
        // SAFETY: within the n readable bytes.
        let (x, y) = unsafe { (a.add(i).read_volatile(), b.add(i).read_volatile()) };
        if x != y {
            return i32::from(x) - i32::from(y);
        }
    }
    0
}

/// A panic traps: Isocast bypasses the filter for that block and starts a
/// fresh instance.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

/// e^x, accurate to about 1e-16 relative.
pub fn exp(x: f64) -> f64 {
    if x > 709.0 {
        return f64::INFINITY;
    }
    if x < -745.0 {
        return 0.0;
    }
    // x = k ln 2 + r with |r| <= ln 2 / 2.
    let k = (x * core::f64::consts::LOG2_E + if x < 0.0 { -0.5 } else { 0.5 }) as i64;
    let r = x - k as f64 * core::f64::consts::LN_2;
    let mut term = 1.0;
    let mut sum = 1.0;
    for n in 1..=18 {
        term *= r / n as f64;
        sum += term;
    }
    scale(sum, k)
}

/// x × 2^k.
fn scale(x: f64, k: i64) -> f64 {
    let (mut x, mut k) = (x, k);
    while k > 1000 {
        x *= f64::from_bits(((1000 + 1023) as u64) << 52);
        k -= 1000;
    }
    while k < -1000 {
        x *= f64::from_bits(((-1000i64 + 1023) as u64) << 52);
        k += 1000;
    }
    x * f64::from_bits(((k + 1023) as u64) << 52)
}

/// The natural logarithm of x (> 0), accurate to about 1e-16 relative.
pub fn ln(x: f64) -> f64 {
    if x <= 0.0 {
        return f64::NEG_INFINITY;
    }
    // x = m × 2^k with m in [√½, √2): ln m = 2 atanh((m − 1) / (m + 1)).
    let bits = x.to_bits();
    let mut k = ((bits >> 52) & 0x7ff) as i64 - 1023;
    let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | (1023u64 << 52));
    if k == -1023 {
        // Subnormal: normalise first.
        let two_54 = f64::from_bits((1023u64 + 54) << 52);
        return ln(x * two_54) - 54.0 * core::f64::consts::LN_2;
    }
    if m > core::f64::consts::SQRT_2 {
        m /= 2.0;
        k += 1;
    }
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let mut term = s;
    let mut sum = 0.0;
    for n in 0..20 {
        sum += term / (2 * n + 1) as f64;
        term *= s2;
    }
    2.0 * sum + k as f64 * core::f64::consts::LN_2
}

/// A level in dB as an amplitude factor, 10^(dB/20), as Isocast's own
/// filters work it out (in f32, the power rounded once).
pub fn amplitude(db: f32) -> f32 {
    let x = db / 20.0;
    exp(f64::from(x) * core::f64::consts::LN_10) as f32
}

/// An amplitude in dB, no lower than −120 (silence has a level).
pub fn decibels(level: f32) -> f32 {
    let log10 = (ln(f64::from(level.max(1e-6))) / core::f64::consts::LN_10) as f32;
    (20.0 * log10).max(-120.0)
}

/// The number param `name` (an f32 or i32 in the manifest).
pub fn number(params: &[Param], name: &str) -> Option<f64> {
    params.iter().find(|p| p.name == name).and_then(|p| match p.value {
        Value::Number(n) => Some(n),
        Value::Integer(n) => Some(n as f64),
        _ => None,
    })
}

/// The text param `name` (a string, enum or file in the manifest).
pub fn text<'a>(params: &'a [Param], name: &str) -> Option<&'a str> {
    params.iter().find(|p| p.name == name).and_then(|p| match &p.value {
        Value::Text(text) => Some(text.as_str()),
        _ => None,
    })
}
