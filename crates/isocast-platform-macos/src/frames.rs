use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use isocast_engine::FrameUse;

use anyhow::{Result, bail};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
    CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVBufferPropagatedAttachmentsKey, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferColorPrimariesKey, kCVImageBufferTransferFunction_ITU_R_709_2,
    kCVImageBufferTransferFunctionKey, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVImageBufferYCbCrMatrixKey, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferPoolAllocationThresholdKey, kCVPixelBufferWidthKey, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange, kCVReturnSuccess,
    kCVReturnWouldExceedAllocationThreshold,
};

// Distinct pictures per domain, not delivery counts. An auxiliary consumer
// cannot spend a recording permit, even when both retain the same picture.
// Eight producer slots cover history, the last picture and GPU submissions;
// files keep 32 pictures, auxiliaries share another 16. Protected compositor
// inputs reserve one cached picture plus their GPU window, for every canvas.
// Thus a stalled passthrough ISO cannot stop the program reading its source.
// The physical bound is the SUM (the distinct union is usually much smaller).
const LIMITS: [usize; 4] = [
    8,
    32,
    16,
    (isocast_engine::gpu::MAX_FRAMES_IN_FLIGHT + 1) * (isocast_protocol::MAX_EXTRA_CANVASES + 1),
];
const MAX_FRAMES_IN_FLIGHT: i32 = (LIMITS[0] + LIMITS[1] + LIMITS[2] + LIMITS[3]) as i32;

static POOLS: Mutex<Vec<Weak<Budget>>> = Mutex::new(Vec::new());
static NEXT_POOL: AtomicU64 = AtomicU64::new(1);

/// Isolated developer exercise: ISOCAST_FRAME_STALL=iso|live holds one
/// native-consumer worker for 600 ms, two seconds after it starts accepting
/// real pictures. It does not stop the GPU, the daemon, or VT callbacks.
pub(crate) fn stall_consumer(target: &str) {
    static BEGAN: OnceLock<Instant> = OnceLock::new();
    static FIRED: AtomicBool = AtomicBool::new(false);
    if !isocast_diagnostics::developer()
        || std::env::var_os("ISOCAST_SOCKET").is_none()
        || std::env::var("ISOCAST_FRAME_STALL").ok().as_deref() != Some(target)
        || BEGAN.get_or_init(Instant::now).elapsed() < Duration::from_secs(2)
        || FIRED.swap(true, Ordering::AcqRel)
    {
        return;
    }
    isocast_diagnostics::diagnostic!("warn"; "injected {target} consumer stall for 600 ms (not a VT reset)");
    std::thread::sleep(Duration::from_millis(600));
    isocast_diagnostics::diagnostic!("info"; "injected {target} consumer stall ended");
}

#[derive(Default)]
struct Usage {
    next: u64,
    frames: HashMap<u64, [usize; 4]>,
    used: [usize; 4],
    peak: usize,
    refused: [u64; 4],
    allocation_refused: u64,
    notices: [Option<Instant>; 5],
}

impl Usage {
    fn notice(&mut self, domain: usize) -> bool {
        let now = Instant::now();
        if self.notices[domain]
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
        {
            return false;
        }
        self.notices[domain] = Some(now);
        true
    }
}

struct Refusal {
    domain: usize,
    count: u64,
    distinct: usize,
    notice: bool,
}

pub(crate) struct Budget {
    id: u64,
    purpose: &'static str,
    size: (u32, u32),
    format: u32,
    owner: String,
    /// Application-owned reorder pictures held inside a decoder, in the
    /// same ledger as its returned pictures (not an unaccounted second pool).
    decoder_reorder: usize,
    opaque_native: AtomicBool,
    usage: Mutex<Usage>,
}

impl Budget {
    pub(crate) fn new(purpose: &'static str, size: (u32, u32), format: u32) -> Arc<Self> {
        Self::with_decoder_reorder(purpose, size, format, 0)
    }

    pub(crate) fn with_decoder_reorder(
        purpose: &'static str,
        size: (u32, u32),
        format: u32,
        decoder_reorder: usize,
    ) -> Arc<Self> {
        let budget = Arc::new(Self {
            id: NEXT_POOL.fetch_add(1, Ordering::Relaxed),
            purpose,
            size,
            format,
            owner: std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_owned(),
            decoder_reorder,
            opaque_native: AtomicBool::new(false),
            usage: Mutex::new(Usage::default()),
        });
        let mut pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
        pools.retain(|pool| pool.strong_count() != 0);
        pools.push(Arc::downgrade(&budget));
        budget
    }

    fn limit(&self, domain: usize) -> usize {
        LIMITS[domain] + if domain == 0 { self.decoder_reorder } else { 0 }
    }

    fn physical_limit(&self) -> usize {
        MAX_FRAMES_IN_FLIGHT as usize + self.decoder_reorder
    }

    fn acquire(self: &Arc<Self>, native_id: Option<u64>) -> Option<Lease> {
        let mut state = self.usage.lock().unwrap_or_else(|e| e.into_inner());
        let first = native_id
            .and_then(|id| state.frames.get(&id))
            .is_none_or(|refs| refs[0] == 0);
        if first && state.used[0] >= self.limit(0) {
            let refused = Self::refuse(&mut state, 0);
            drop(state);
            self.report(refused);
            return None;
        }
        let id = native_id.unwrap_or_else(|| {
            state.next += 1;
            state.next
        });
        state.frames.entry(id).or_default()[0] += 1;
        state.used[0] += usize::from(first);
        state.peak = state.peak.max(state.frames.len());
        drop(state);
        Some(Lease(Arc::new(Claim {
            budget: self.clone(),
            id,
            usage: FrameUse::Production,
        })))
    }

    /// Only counters and a primitive notice under the hot ownership lock.
    fn refuse(state: &mut Usage, class: usize) -> Refusal {
        state.refused[class] += 1;
        REFUSED[class].fetch_add(1, Ordering::Relaxed);
        Refusal {
            domain: class,
            count: state.refused[class],
            distinct: state.frames.len(),
            notice: state.notice(class),
        }
    }

    fn report(&self, refused: Refusal) {
        if refused.domain == FrameUse::Auxiliary as usize {
            isocast_engine::governor::frame_pressure();
        }
        if refused.notice {
            isocast_diagnostics::output::deferred(
                "warn",
                module_path!(),
                format!(
                    "frame budget {} ({}): domain {} refused {} picture(s), distinct {} / {}",
                    self.id,
                    self.purpose,
                    refused.domain,
                    refused.count,
                    refused.distinct,
                    self.physical_limit()
                ),
            );
        }
    }

    /// Decoder output has no owned CV pool. Bound its application-visible
    /// frames with the same ledger, without copying or changing its format.
    pub(crate) fn wrap(self: &Arc<Self>, buffer: CFRetained<CVPixelBuffer>) -> Result<PixelBuffer> {
        // Two decoder samples may reference the same native buffer. Its CF
        // identity stays unique while any lease retains it; count it once.
        let id = (&*buffer as *const CVPixelBuffer) as usize as u64;
        for _ in 0..200 {
            if let Some(lease) = self.acquire(Some(id)) {
                return Ok(PixelBuffer(buffer, lease));
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        bail!(
            "frame budget {}: decoder production reserve did not recover in one second",
            self.id
        )
    }
}

static REFUSED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static ALLOCATION_REFUSED: AtomicU64 = AtomicU64::new(0);

// Every reference has a domain. A pool counts a distinct buffer once per
// domain, not once per delivery. The native buffer field drops BEFORE this
// lease, so an available permit never precedes the actual CV release.
#[derive(Clone)]
struct Lease(Arc<Claim>);

// Ordinary clones share a domain claim. Only its first/last ownership touches
// the ledger; independently acquired claims still deduplicate by buffer/domain.
struct Claim {
    budget: Arc<Budget>,
    id: u64,
    usage: FrameUse,
}

impl Lease {
    fn retain(&self, usage: FrameUse) -> Option<Self> {
        if self.0.usage == usage {
            return Some(self.clone());
        }
        let claim = &self.0;
        let mut state = claim.budget.usage.lock().unwrap_or_else(|e| e.into_inner());
        let class = usage as usize;
        let first = state.frames[&claim.id][class] == 0;
        if first && state.used[class] >= claim.budget.limit(class) {
            let refused = Budget::refuse(&mut state, class);
            drop(state);
            claim.budget.report(refused);
            return None;
        }
        state.frames.get_mut(&claim.id).expect("live frame")[class] += 1;
        state.used[class] += usize::from(first);
        if first && usage == FrameUse::Auxiliary && state.used[class] == LIMITS[class] {
            isocast_engine::governor::frame_budget_full(true);
        }
        drop(state);
        Some(Self(Arc::new(Claim {
            budget: claim.budget.clone(),
            id: claim.id,
            usage,
        })))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut state = self.budget.usage.lock().unwrap_or_else(|e| e.into_inner());
        let refs = state.frames.get_mut(&self.id).expect("live frame");
        let class = self.usage as usize;
        refs[class] -= 1;
        let last_class = refs[class] == 0;
        let last = refs.iter().all(|refs| *refs == 0);
        if last_class && self.usage == FrameUse::Auxiliary && state.used[class] == LIMITS[class] {
            isocast_engine::governor::frame_budget_full(false);
        }
        state.used[class] -= usize::from(last_class);
        if last {
            state.frames.remove(&self.id);
        }
    }
}

pub(crate) fn diagnostics() -> serde_json::Value {
    let shared = isocast_surfaces::retained_surfaces();
    let pools: Vec<_> = {
        let mut pools = POOLS.lock().unwrap_or_else(|e| e.into_inner());
        pools.retain(|pool| pool.strong_count() != 0);
        pools.iter().filter_map(Weak::upgrade).collect()
    };
    let pools: Vec<_> = pools.into_iter().map(|pool| {
        let (distinct, peak, used, refused, allocation_refused) = {
            let state = pool.usage.lock().unwrap_or_else(|e| e.into_inner());
            (state.frames.len(), state.peak, state.used, state.refused, state.allocation_refused)
        };
        let opaque = pool.opaque_native.load(Ordering::Relaxed);
        serde_json::json!({
            "id": pool.id, "purpose": pool.purpose, "owner": pool.owner, "size": pool.size, "pixel_format": pool.format,
            "distinct": if opaque { None } else { Some(distinct) }, "peak": peak,
            "application_distinct": distinct,
            "opaque_native_reservation": if opaque { pool.physical_limit() } else { 0 },
            "decoder_reorder": pool.decoder_reorder,
            "production_reserve": pool.limit(0), "physical_limit": pool.physical_limit(),
            "production": used[0], "recording": used[1], "auxiliary": used[2],
            "composition": used[3],
            "refused": refused, "allocation_refused": allocation_refused,
            "invariant": used.iter().enumerate().all(|(domain, used)| *used <= pool.limit(domain))
                && distinct <= pool.physical_limit(),
        })
    }).collect();
    serde_json::json!({
        "physical_limit": MAX_FRAMES_IN_FLIGHT, "production_reserve": LIMITS[0],
        "recording_reserve": LIMITS[1], "auxiliary_limit": LIMITS[2],
        "composition_reserve": LIMITS[3],
        "limits": "top-level reserves are defaults; each pool includes its decoder_reorder in production_reserve and physical_limit",
        "refusal_domains": ["production", "recording", "auxiliary", "composition"],
        "counts": "distinct buffers; a shared picture counts once in each domain, once in the physical union",
        "refused": REFUSED.each_ref().map(|n| n.load(Ordering::Relaxed)),
        "allocation_refused": ALLOCATION_REFUSED.load(Ordering::Relaxed),
        "opaque_native": "AVAssetWriter uses an independent threshold-limited pool; charge its entire physical reserve, not an invented native in-use count",
        "shared_surfaces": {"distinct": shared.0, "bytes": shared.1, "max_ring_slots": isocast_surfaces::MAX_SLOTS,
            "ownership": "independent viewer rings and Mach handoffs; source reads hold auxiliary permits through GPU completion"},
        "virtual_camera_native_reservation": MAX_FRAMES_IN_FLIGHT,
        "virtual_camera_native": "when enabled, the helper has its own threshold-limited copy pool; CMIO cannot retain a parent pool buffer",
        "invariant": pools.iter().all(|pool| pool["invariant"] == true) && ALLOCATION_REFUSED.load(Ordering::Relaxed) == 0, "pools": pools,
    })
}

/// A retained `CVPixelBuffer`. Cloning shares the same pixels.
#[derive(Clone)]
pub struct PixelBuffer(pub CFRetained<CVPixelBuffer>, Lease);

impl PixelBuffer {
    pub(crate) fn retain(&self, usage: FrameUse) -> Option<Self> {
        self.1
            .retain(usage)
            .map(|lease| Self(self.0.clone(), lease))
    }
}

/// A source frame/permit lives through native completion, including synchronous
/// callbacks, submission errors and invalidation without a callback. Cookies
/// are removed exactly once; neither native call site owns a raw Box.
#[derive(Default)]
pub(crate) struct NativeFrames {
    next: AtomicUsize,
    pending: Mutex<HashMap<usize, NativeFrame>>,
}

pub(crate) struct NativeFrame {
    pub submitted: u64,
    _frame: PixelBuffer,
}

impl NativeFrames {
    pub(crate) fn insert(&self, frame: PixelBuffer, submitted: u64) -> *mut c_void {
        let cookie = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                cookie,
                NativeFrame {
                    submitted,
                    _frame: frame,
                },
            );
        cookie as *mut c_void
    }

    pub(crate) fn take(&self, cookie: *mut c_void) -> Option<NativeFrame> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(cookie as usize))
    }

    pub(crate) fn drain(&self) -> Vec<NativeFrame> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .map(|(_, frame)| frame)
            .collect()
    }
}

// SAFETY: CoreVideo buffers are reference counted atomically and may be used
// from any thread. Pixels are only written before a frame is shared.
unsafe impl Send for PixelBuffer {}
unsafe impl Sync for PixelBuffer {}

/// IOSurface-backed pixel buffers of one size and format: BGRA, or 8-bit
/// 4:2:0 Y'CbCr (video range, tagged Rec. 709) for the GPU to convert into.
pub struct FramePool {
    pool: CFRetained<CVPixelBufferPool>,
    aux_attributes: CFRetained<CFDictionary<CFString, CFNumber>>,
    budget: Arc<Budget>,
}

// SAFETY: CVPixelBufferPool is thread-safe; the pool is used by one thread
// at a time anyway.
unsafe impl Send for FramePool {}

impl FramePool {
    pub(crate) fn reserve_native(&self) {
        self.budget.opaque_native.store(true, Ordering::Relaxed);
    }

    /// AVAssetWriter has no source-release callback. Give it an independent
    /// bounded pool and account for its entire reservation conservatively.
    pub(crate) fn copy_native(
        pool: &mut Option<Self>,
        source: &CVPixelBuffer,
    ) -> Result<Option<PixelBuffer>> {
        let frame = Self::copy(pool, source)?;
        if let Some(pool) = pool {
            pool.reserve_native();
        }
        Ok(frame.and_then(|frame| frame.retain(FrameUse::Recording)))
    }

    /// Copy scarce capture buffers at ingress, before any downstream owner
    /// can pin ScreenCaptureKit/AVCapture's much smaller private pools. Preserve
    /// all 8/10-bit planes and colour attachments; do not pass through BGRA.
    pub(crate) fn copy(
        pool: &mut Option<Self>,
        source: &CVPixelBuffer,
    ) -> Result<Option<PixelBuffer>> {
        use isocast_engine::FramePool as _;
        let size = (
            CVPixelBufferGetWidth(source) as u32,
            CVPixelBufferGetHeight(source) as u32,
        );
        let format = CVPixelBufferGetPixelFormatType(source);
        let bytes = match format {
            f if f == kCVPixelFormatType_32BGRA => 4,
            f if f == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                || f == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange =>
            {
                1
            }
            f if f == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
                || f == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange =>
            {
                2
            }
            _ => bail!("cannot isolate pixel format {format:#x} without changing its format"),
        };
        if pool
            .as_ref()
            .is_none_or(|p| p.budget.size != size || p.budget.format != format)
        {
            *pool = Some(Self::with_format(size.0, size.1, format)?);
        }
        let Some(target) = pool.as_mut().expect("made above").take()? else {
            return Ok(None);
        };
        let _source_lock = Locked::new(source, CVPixelBufferLockFlags::ReadOnly)?;
        let target_lock = Locked::new(&target.0, CVPixelBufferLockFlags(0))?;
        let planes = CVPixelBufferGetPlaneCount(source);
        if planes != CVPixelBufferGetPlaneCount(&target.0)
            || (format != kCVPixelFormatType_32BGRA && planes != 2)
        {
            bail!("cannot isolate a frame with unexpected planes");
        }
        for plane in 0..planes.max(1) {
            let layout = |buffer: &CVPixelBuffer| {
                if planes == 0 {
                    (
                        CVPixelBufferGetBaseAddress(buffer),
                        CVPixelBufferGetBytesPerRow(buffer),
                        CVPixelBufferGetWidth(buffer),
                        CVPixelBufferGetHeight(buffer),
                    )
                } else {
                    (
                        CVPixelBufferGetBaseAddressOfPlane(buffer, plane),
                        CVPixelBufferGetBytesPerRowOfPlane(buffer, plane),
                        CVPixelBufferGetWidthOfPlane(buffer, plane),
                        CVPixelBufferGetHeightOfPlane(buffer, plane),
                    )
                }
            };
            let (from, from_stride, width, height) = layout(source);
            let (to, to_stride, to_width, to_height) = layout(&target.0);
            let row = width * bytes * if planes > 0 && plane == 1 { 2 } else { 1 };
            if from.is_null()
                || to.is_null()
                || from_stride < row
                || to_stride < row
                || (width, height) != (to_width, to_height)
            {
                bail!("cannot isolate a frame with unexpected plane dimensions");
            }
            for y in 0..height {
                // Locked, independent buffers; both rows have `row` bytes.
                unsafe {
                    ptr::copy_nonoverlapping(
                        from.cast::<u8>().add(y * from_stride),
                        to.cast::<u8>().add(y * to_stride),
                        row,
                    )
                };
            }
        }
        target.0.remove_all_attachments();
        source.propagate_attachments(&target.0);
        drop(target_lock);
        Ok(Some(target))
    }

    /// BGRA frames.
    pub fn new(width: u32, height: u32) -> Result<Self> {
        Self::with_format(width, height, kCVPixelFormatType_32BGRA)
    }

    /// 8-bit 4:2:0 Y'CbCr frames in video range, tagged Rec. 709: what the
    /// encoders take without converting, at under half BGRA's size.
    pub fn ycbcr(width: u32, height: u32) -> Result<Self> {
        Self::with_format(
            width,
            height,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        )
    }

    fn with_format(width: u32, height: u32, pixel_format: u32) -> Result<Self> {
        let budget = Budget::new("pool", (width, height), pixel_format);
        let format = CFNumber::new_i32(pixel_format as i32);
        let width = CFNumber::new_i32(width as i32);
        let height = CFNumber::new_i32(height as i32);
        let io_surface = CFDictionary::<CFString, CFType>::empty();
        // The GPU can draw into them (the compositor) or read them.
        let metal = CFBoolean::new(true);
        // Every frame says how its Y'CbCr encodes colour, so encoders and
        // readers don't guess.
        let colour = unsafe {
            CFDictionary::<CFString, CFString>::from_slices(
                &[
                    kCVImageBufferYCbCrMatrixKey,
                    kCVImageBufferColorPrimariesKey,
                    kCVImageBufferTransferFunctionKey,
                ],
                &[
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                    kCVImageBufferColorPrimaries_ITU_R_709_2,
                    kCVImageBufferTransferFunction_ITU_R_709_2,
                ],
            )
        };
        let mut keys = unsafe {
            vec![
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferWidthKey,
                kCVPixelBufferHeightKey,
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
            ]
        };
        let mut values: Vec<&CFType> = vec![&format, &width, &height, &io_surface, metal];
        if pixel_format != kCVPixelFormatType_32BGRA {
            keys.push(unsafe { kCVBufferPropagatedAttachmentsKey });
            values.push(&colour);
        }
        let attributes = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
        let mut pool = ptr::null_mut();
        let status = unsafe {
            CVPixelBufferPool::create(
                None,
                None,
                Some(attributes.as_opaque()),
                NonNull::from(&mut pool),
            )
        };
        let Some(pool) = NonNull::new(pool).filter(|_| status == kCVReturnSuccess) else {
            bail!("could not create a pixel buffer pool (CoreVideo error {status})");
        };
        let threshold = CFNumber::new_i32(MAX_FRAMES_IN_FLIGHT);
        let aux_attributes = unsafe {
            CFDictionary::from_slices(&[kCVPixelBufferPoolAllocationThresholdKey], &[&*threshold])
        };
        Ok(Self {
            pool: unsafe { CFRetained::from_raw(pool) },
            aux_attributes,
            budget,
        })
    }
}

struct Locked<'a>(&'a CVPixelBuffer, CVPixelBufferLockFlags);

impl<'a> Locked<'a> {
    fn new(buffer: &'a CVPixelBuffer, flags: CVPixelBufferLockFlags) -> Result<Self> {
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer, flags) };
        if status != kCVReturnSuccess {
            bail!("could not lock a frame for copying (CoreVideo error {status})")
        }
        Ok(Self(buffer, flags))
    }
}

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, self.1) };
    }
}

impl isocast_engine::FramePool for FramePool {
    type Frame = PixelBuffer;

    fn draw(&mut self, draw: &mut dyn FnMut(&mut [u8], usize)) -> Result<Option<PixelBuffer>> {
        let Some(frame) = self.take()? else {
            return Ok(None);
        };
        let buffer = &frame.0;
        if CVPixelBufferGetPixelFormatType(buffer) != kCVPixelFormatType_32BGRA {
            bail!("only BGRA frames are drawn on the CPU");
        }
        let flags = CVPixelBufferLockFlags(0);
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer, flags) };
        if status != kCVReturnSuccess {
            bail!("could not lock a frame for drawing (CoreVideo error {status})");
        }
        let stride = CVPixelBufferGetBytesPerRow(buffer);
        let len = stride * CVPixelBufferGetHeight(buffer);
        let base = CVPixelBufferGetBaseAddress(buffer).cast::<u8>();
        // SAFETY: the buffer is locked, and a locked BGRA buffer holds
        // `bytes per row × height` bytes at its base address.
        draw(unsafe { slice::from_raw_parts_mut(base, len) }, stride);
        unsafe { CVPixelBufferUnlockBaseAddress(buffer, flags) };
        Ok(Some(frame))
    }

    fn take(&mut self) -> Result<Option<PixelBuffer>> {
        let Some(lease) = self.budget.acquire(None) else {
            return Ok(None);
        };
        let mut buffer = ptr::null_mut();
        let status = unsafe {
            CVPixelBufferPool::create_pixel_buffer_with_aux_attributes(
                None,
                &self.pool,
                Some(self.aux_attributes.as_opaque()),
                NonNull::from(&mut buffer),
            )
        };
        if status == kCVReturnWouldExceedAllocationThreshold {
            let notice = {
                let mut state = self.budget.usage.lock().unwrap_or_else(|e| e.into_inner());
                state.allocation_refused += 1;
                state.notice(4)
            };
            let opaque = self.budget.opaque_native.load(Ordering::Relaxed);
            if !opaque {
                ALLOCATION_REFUSED.fetch_add(1, Ordering::Relaxed);
            }
            if notice {
                isocast_diagnostics::output::deferred(
                    "error",
                    module_path!(),
                    format!(
                        "frame budget {}: native pool exhausted (opaque native ownership: {})",
                        self.budget.id, opaque
                    ),
                );
            }
            return Ok(None);
        }
        let Some(buffer) = NonNull::new(buffer).filter(|_| status == kCVReturnSuccess) else {
            bail!("could not get a frame from the pool (CoreVideo error {status})");
        };
        Ok(Some(PixelBuffer(
            unsafe { CFRetained::from_raw(buffer) },
            lease,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isocast_engine::FramePool as _;

    #[test]
    fn ordinary_clones_do_not_wait_for_the_ledger() {
        let mut pool = FramePool::new(64, 64).unwrap();
        let frame = pool.take().unwrap().unwrap();
        let other = frame.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let locked = pool.budget.usage.lock().unwrap();
        let thread = std::thread::spawn(move || {
            for _ in 0..1000 {
                drop(other.clone());
            }
            drop(other);
            tx.send(()).unwrap();
        });
        let finished = rx.recv_timeout(Duration::from_secs(2));
        drop(locked);
        thread.join().unwrap();
        assert!(
            finished.is_ok(),
            "ordinary clones blocked on budget accounting"
        );
        drop(frame);
        assert!(pool.budget.usage.lock().unwrap().frames.is_empty());
    }

    #[test]
    fn shared_deliveries_count_one_buffer() {
        let mut pool = FramePool::new(64, 64).unwrap();
        let frame = pool.take().unwrap().unwrap();
        let recording = frame.retain(FrameUse::Recording).unwrap();
        let another_file = frame.retain(FrameUse::Recording).unwrap();
        let live = frame.retain(FrameUse::Auxiliary).unwrap();
        let clone = live.clone();
        {
            let state = pool.budget.usage.lock().unwrap();
            assert_eq!(state.used, [1, 1, 1, 0]);
            assert_eq!(state.frames.len(), 1);
        }
        drop((frame, recording, live));
        assert_eq!(pool.budget.usage.lock().unwrap().used, [0, 1, 1, 0]);
        drop((another_file, clone));
        assert!(pool.budget.usage.lock().unwrap().frames.is_empty());
    }

    #[test]
    fn decoder_samples_of_one_native_buffer_share_a_charge() {
        let mut pool = FramePool::new(64, 64).unwrap();
        let frame = pool.take().unwrap().unwrap();
        let decoder = Budget::new("decoder", (64, 64), kCVPixelFormatType_32BGRA);
        let a = decoder.wrap(frame.0.clone()).unwrap();
        let b = decoder.wrap(frame.0.clone()).unwrap();
        let first = a.retain(FrameUse::Recording).unwrap();
        let second = b.retain(FrameUse::Recording).unwrap();
        drop((a, b));
        assert_eq!(decoder.usage.lock().unwrap().used, [0, 1, 0, 0]);
        assert_eq!(decoder.usage.lock().unwrap().frames.len(), 1);
        drop((first, second));
        assert!(decoder.usage.lock().unwrap().frames.is_empty());
    }

    #[test]
    fn auxiliary_cannot_spend_recording_or_production_reserves() {
        let mut pool = FramePool::new(64, 64).unwrap();
        let mut auxiliary = Vec::new();
        for _ in 0..LIMITS[2] {
            let frame = pool.take().unwrap().unwrap();
            auxiliary.push(frame.retain(FrameUse::Auxiliary).unwrap());
        }
        let mut recording = Vec::new();
        for _ in 0..LIMITS[1] {
            let frame = pool.take().unwrap().unwrap();
            assert!(frame.retain(FrameUse::Auxiliary).is_none());
            recording.push(frame.retain(FrameUse::Recording).unwrap());
        }
        let production: Vec<_> = (0..LIMITS[0])
            .map(|_| pool.take().unwrap().unwrap())
            .collect();
        // A file stall cannot consume the protected render-input window.
        // Reuse production pictures to acquire distinct composition permits,
        // then release the temporary production references for more pictures.
        drop(production);
        let mut composition = Vec::new();
        for _ in 0..LIMITS[3] {
            let frame = pool.take().unwrap().unwrap();
            assert!(frame.retain(FrameUse::Recording).is_none());
            composition.push(frame.retain(FrameUse::Composition).unwrap());
        }
        let production: Vec<_> = (0..LIMITS[0])
            .map(|_| pool.take().unwrap().unwrap())
            .collect();
        assert!(pool.take().unwrap().is_none());
        {
            let state = pool.budget.usage.lock().unwrap();
            assert_eq!(state.used, LIMITS);
            assert_eq!(state.frames.len(), MAX_FRAMES_IN_FLIGHT as usize);
            assert_eq!(state.allocation_refused, 0);
        }
        // Repeating an already permitted picture always succeeds, even full.
        assert!(recording[0].retain(FrameUse::Recording).is_some());
        assert!(auxiliary[0].retain(FrameUse::Auxiliary).is_some());
        drop((production, recording, auxiliary, composition));
        assert!(pool.budget.usage.lock().unwrap().frames.is_empty());
        assert!(pool.take().unwrap().is_some());
    }

    #[test]
    fn decoder_reorder_shares_a_bounded_production_reserve() {
        let decoder = Budget::with_decoder_reorder("decoder", (64, 64), 0, 4);
        let mut frames: Vec<_> = (0..LIMITS[0] + 4)
            .map(|_| decoder.acquire(None).unwrap())
            .collect();
        assert_eq!(decoder.usage.lock().unwrap().used[0], 12);
        assert!(decoder.acquire(None).is_none());
        drop(frames.pop());
        assert!(decoder.acquire(None).is_some());
        assert_eq!(decoder.physical_limit(), MAX_FRAMES_IN_FLIGHT as usize + 4);
        for (domain, limit) in LIMITS.iter().enumerate().skip(1) {
            assert_eq!(decoder.limit(domain), *limit);
        }
    }

    #[test]
    fn native_callbacks_keep_and_retire_shared_frames_once() {
        let mut pool = FramePool::new(64, 64).unwrap();
        let frame = pool
            .take()
            .unwrap()
            .unwrap()
            .retain(FrameUse::Recording)
            .unwrap();
        let native = NativeFrames::default();
        let a = native.insert(frame.clone(), 1);
        let b = native.insert(frame.clone(), 2);
        drop(frame);
        assert_eq!(pool.budget.usage.lock().unwrap().used, [0, 1, 0, 0]);
        drop(native.take(a).unwrap());
        assert!(native.take(a).is_none());
        assert_eq!(pool.budget.usage.lock().unwrap().used, [0, 1, 0, 0]);
        drop(native.drain());
        assert!(native.take(b).is_none());
        assert!(pool.budget.usage.lock().unwrap().frames.is_empty());
    }

    #[test]
    fn isolation_preserves_bgra_and_eight_and_ten_bit_planes() {
        for format in [
            kCVPixelFormatType_32BGRA,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
        ] {
            let mut source_pool = FramePool::with_format(64, 64, format).unwrap();
            let source = source_pool.take().unwrap().unwrap();
            let locked = Locked::new(&source.0, CVPixelBufferLockFlags(0)).unwrap();
            let planes = CVPixelBufferGetPlaneCount(&source.0);
            for plane in 0..planes.max(1) {
                let (base, stride, height) = if planes == 0 {
                    (
                        CVPixelBufferGetBaseAddress(&source.0),
                        CVPixelBufferGetBytesPerRow(&source.0),
                        64,
                    )
                } else {
                    (
                        CVPixelBufferGetBaseAddressOfPlane(&source.0, plane),
                        CVPixelBufferGetBytesPerRowOfPlane(&source.0, plane),
                        CVPixelBufferGetHeightOfPlane(&source.0, plane),
                    )
                };
                // Locked native storage; the whole plane includes padding.
                unsafe { ptr::write_bytes(base.cast::<u8>(), 0x5a + plane as u8, stride * height) };
            }
            drop(locked);
            let mut target_pool = None;
            let target = FramePool::copy(&mut target_pool, &source.0)
                .unwrap()
                .unwrap();
            assert_eq!(CVPixelBufferGetPixelFormatType(&target.0), format);
            let _locked = Locked::new(&target.0, CVPixelBufferLockFlags::ReadOnly).unwrap();
            let row = if format == kCVPixelFormatType_32BGRA {
                256
            } else if format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                || format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
            {
                64
            } else {
                128
            };
            for plane in 0..planes.max(1) {
                let (base, stride, height) = if planes == 0 {
                    (
                        CVPixelBufferGetBaseAddress(&target.0),
                        CVPixelBufferGetBytesPerRow(&target.0),
                        64,
                    )
                } else {
                    (
                        CVPixelBufferGetBaseAddressOfPlane(&target.0, plane),
                        CVPixelBufferGetBytesPerRowOfPlane(&target.0, plane),
                        CVPixelBufferGetHeightOfPlane(&target.0, plane),
                    )
                };
                for y in 0..height {
                    // Locked storage, reading only the active bytes of each row.
                    let bytes =
                        unsafe { slice::from_raw_parts(base.cast::<u8>().add(y * stride), row) };
                    assert!(bytes.iter().all(|b| *b == 0x5a + plane as u8));
                }
            }
        }
    }
}
