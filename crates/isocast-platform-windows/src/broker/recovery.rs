//! A job is disposable; its target and engine sink belong to the parent.
use super::*;
use isocast_engine::ScreenSink;
use isocast_protocol::api::WindowsDisplayOwner;
use std::collections::BTreeMap;
use windows::Win32::Foundation::{HANDLE, HWND, LUID};
use windows::Win32::Security::AllocateLocallyUniqueId;
use windows::Win32::UI::WindowsAndMessaging::{GetPropW, RemovePropW, SetPropW};
use windows::core::{PCWSTR, w};

const PIN_NAME: PCWSTR = w!("Isocast.Capture.WindowLifetime");
// Overlapping sources capturing one HWND share its registration, not its
// helper job. Serialize install and last-drop; no weak/stale entry buildup.
static PINS: Mutex<BTreeMap<usize, (usize, usize)>> = parking_lot::const_mutex(BTreeMap::new());

struct WindowPin {
    handle: usize,
    token: usize,
}

impl WindowPin {
    fn new(handle: usize) -> Result<Self> {
        let mut pins = PINS.lock();
        let hwnd = HWND(handle as _);
        let current = unsafe { GetPropW(hwnd, PIN_NAME) }.0 as usize;
        if let Some((token, owners)) = pins.get_mut(&handle)
            && current == *token
        {
            *owners += 1;
            return Ok(Self {
                handle,
                token: *token,
            });
        }
        anyhow::ensure!(
            current == 0,
            "window lifetime property is owned by another capture"
        );
        // Unique on this computer until reboot (which destroys every HWND).
        // Never allocate a global atom per capture, truncate a LUID to fit,
        // store a process pointer in a window, or re-install on retry.
        let mut id = LUID::default();
        unsafe { AllocateLocallyUniqueId(&mut id) }?;
        let token = usize::try_from((u64::from(id.HighPart as u32) << 32) | u64::from(id.LowPart))?;
        anyhow::ensure!(token != 0, "window lifetime token is zero");
        unsafe { SetPropW(hwnd, PIN_NAME, Some(HANDLE(token as _))) }?;
        pins.insert(handle, (token, 1));
        Ok(Self { handle, token })
    }
    fn valid(&self) -> bool {
        unsafe { GetPropW(HWND(self.handle as _), PIN_NAME).0 as usize == self.token }
    }
}

impl Drop for WindowPin {
    fn drop(&mut self) {
        let mut pins = PINS.lock();
        let Some((token, owners)) = pins.get_mut(&self.handle) else {
            return;
        };
        if *token != self.token {
            return;
        } // HWND has been recycled and freshly pinned
        *owners -= 1;
        if *owners != 0 {
            return;
        }
        if self.valid() {
            let _ = unsafe { RemovePropW(HWND(self.handle as _), PIN_NAME) };
        }
        pins.remove(&self.handle);
    }
}

enum Pin {
    Window(WindowPin),
    Monitor(String),
    // UIPI or a mirrored/unidentified display need not break a working
    // initial capture. They do make a later automatic reopen unsafe.
    InitialOnly(CaptureTarget, String),
}

impl Pin {
    fn new(target: CaptureTarget) -> Self {
        match target {
            CaptureTarget::Window(h) => WindowPin::new(h).map(Self::Window),
            _ => crate::devices::display_pin(target).map(|(path, _)| Self::Monitor(path)),
        }
        .unwrap_or_else(|e| Self::InitialOnly(target, e.to_string()))
    }
    fn resolve(&self, reopening: bool) -> Result<CaptureTarget> {
        match self {
            Self::Window(p) if p.valid() => Ok(CaptureTarget::Window(p.handle)),
            Self::Window(_) => {
                bail!("the original pinned window was closed or replaced; select the window again")
            }
            Self::Monitor(path) => crate::devices::pinned_monitor(path).map(CaptureTarget::Monitor),
            Self::InitialOnly(target, _) if !reopening => Ok(*target),
            Self::InitialOnly(_, why) => bail!(
                "cannot prove the original capture target is still present ({why}); select it again"
            ),
        }
    }
}

#[derive(Default)]
struct Active {
    stop: bool,
    child: Weak<Control>,
}

pub(super) struct Epoch {
    pin: Arc<Pin>,
    active: Arc<Mutex<Active>>,
    pub(super) deadline: Option<Instant>,
    pub(super) transport_lost: AtomicBool,
}

impl Epoch {
    pub(super) fn attach(&self, control: &Arc<Control>) -> Result<()> {
        let mut active = self.active.lock();
        anyhow::ensure!(!active.stop, "capture stopped");
        active.child = Arc::downgrade(control);
        Ok(())
    }
    pub(super) fn verify(&self, target: CaptureTarget) -> Result<()> {
        // Ready follows item creation. Bracket that call with the identity
        // check BEFORE permitting even one frame from this receiver.
        anyhow::ensure!(
            self.pin.resolve(self.deadline.is_some())? == target,
            "original capture target changed while the helper was opening"
        );
        Ok(())
    }
}

pub struct PinnedCapture {
    active: Arc<Mutex<Active>>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for PinnedCapture {
    fn drop(&mut self) {
        let child = {
            let mut active = self.active.lock();
            active.stop = true;
            active.child.upgrade()
        };
        if let Some(child) = child {
            child.process.kill();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Default)]
struct Budget {
    restarts: u8,
}

impl Budget {
    fn next(
        &mut self,
        transport_lost: bool,
        healthy_for: Option<Duration>,
        why: &str,
    ) -> Result<()> {
        anyhow::ensure!(transport_lost, "{why}");
        if healthy_for.is_some_and(|t| t >= Duration::from_secs(60)) {
            self.restarts = 0;
        }
        anyhow::ensure!(
            self.restarts == 0 || healthy_for.is_some(),
            "replacement helper died before its first fresh frame: {why}"
        );
        anyhow::ensure!(
            self.restarts < 3,
            "capture helper restart limit reached: {why}"
        );
        self.restarts += 1;
        Ok(())
    }
}

/// The child can be Ready with an empty pool. Only a completed import
/// stamped after this open and the last delivered generation can resume it.
struct Pictures {
    opened: Option<Time>,
    previous: Option<Time>,
    deadline: Option<Instant>,
    last: Option<Time>,
    first_import: Option<Instant>,
}

impl Pictures {
    fn admit(&mut self, stamp: Time, now: Instant) -> bool {
        if self.opened.is_some_and(|t| stamp < t)
            || self.previous.is_some_and(|t| stamp <= t)
            || self.last.is_none() && self.deadline.is_some_and(|end| now >= end)
        {
            return false;
        }
        self.last = Some(stamp);
        self.first_import.get_or_insert(now);
        true
    }
}

pub(crate) fn open(
    device: GraphicsDevice,
    source: String,
    (selected, initial_owner): (CaptureTarget, Option<WindowsDisplayOwner>),
    fps: u32,
    cursor: bool,
    sink: ScreenSink<Frame>,
) -> Result<(PinnedCapture, CaptureFormat)> {
    let pin = Arc::new(Pin::new(selected));
    let helper = std::env::current_exe()?.with_file_name("isocast-capture.exe");
    let active = Arc::new(Mutex::new(Active::default()));
    let (tx, rx) = mpsc::sync_channel(1);
    let run = {
        let active = active.clone();
        move || {
            let mut startup = Some(tx);
            let sink = Arc::new(sink);
            let result = (|| -> Result<()> {
                let mut budget = Budget::default();
                let mut last_at = None;
                loop {
                    if active.lock().stop {
                        return Ok(());
                    }
                    let replacing = startup.is_none();
                    // Selection observed the initial owner. On reopen resolve
                    // only the physical path, never the setup's alias/name,
                    // and attribute the HMONITOR actually sent to this child.
                    let (target, owner) = match pin.as_ref() {
                        Pin::Monitor(path)
                            if replacing || selected == CaptureTarget::PrimaryDisplay =>
                        {
                            crate::devices::pinned_monitor_route(path)?
                        }
                        _ => {
                            let target = pin.resolve(replacing)?;
                            anyhow::ensure!(
                                target == selected,
                                "original capture target changed after selection"
                            );
                            (target, initial_owner.clone())
                        }
                    };
                    #[cfg(debug_assertions)]
                    let fixture = fixture_mode().is_some() && fixture_pixel(target).is_some();
                    unsafe { device.raw().GetDeviceRemovedReason() }
                        .context("D3D11 device lost; cannot restart the capture helper")?;
                    let deadline = replacing.then(|| Instant::now() + Duration::from_secs(5));
                    let epoch = Arc::new(Epoch {
                        pin: pin.clone(),
                        active: active.clone(),
                        deadline,
                        transport_lost: AtomicBool::new(false),
                    });
                    let (ended, end) = mpsc::sync_channel(1);
                    let pictures = Arc::new(Mutex::new(Pictures {
                        opened: replacing.then(crate::now),
                        previous: last_at,
                        deadline,
                        last: None,
                        first_import: None,
                    }));
                    let video = VideoSink {
                        frame: Box::new({
                            let sink = sink.clone();
                            let pictures = pictures.clone();
                            move |frame, at| {
                                if pictures.lock().admit(at, Instant::now()) {
                                    (sink.video.frame)(frame, at);
                                }
                            }
                        }),
                        stopped: Box::new(move |why| {
                            let _ = ended.try_send(why);
                        }),
                    };
                    let (mut capture, format) = open_generation(
                        &device,
                        &helper,
                        target,
                        fps,
                        cursor,
                        video,
                        Some(epoch.clone()),
                    )?;
                    // Ready (including pin/LUID verification) succeeded. This
                    // observation lives only as long as this helper generation.
                    capture.route = Some(crate::route::capture(&source, owner, &device, {
                        let pictures = Arc::downgrade(&pictures);
                        move || pictures.upgrade().is_some_and(|p| p.lock().last.is_some())
                    }));
                    if let Some(tx) = startup.take() {
                        tx.send(Ok(format))
                            .map_err(|_| anyhow::anyhow!("capture cancelled"))?;
                    }
                    let why = loop {
                        if active.lock().stop {
                            return Ok(());
                        }
                        let timeout = deadline
                            .filter(|_| pictures.lock().last.is_none())
                            .map(|end| end.saturating_duration_since(Instant::now()));
                        if timeout == Some(Duration::ZERO) {
                            bail!(
                                "replacement capture helper delivered no fresh picture before its deadline"
                            );
                        }
                        #[cfg(debug_assertions)]
                        let timeout = if !replacing && fixture {
                            if isocast_diagnostics::developer() {
                                capture.control.process.kill();
                            }
                            Some(Duration::from_millis(50))
                        } else {
                            timeout
                        };
                        // Healthy captures sleep until EOF, Stop (which kills
                        // our owned process), or an explicit source-close message.
                        let event = match timeout {
                            Some(t) => end.recv_timeout(t),
                            None => end.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
                        };
                        match event {
                            Ok(why) => break why,
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(_) => bail!("capture helper receiver exited without a result"),
                        }
                    };
                    // Stop and join the OLD receiver before invalidating the
                    // slot. No frame or Ack can cross to the next process.
                    let ended_at = Instant::now();
                    drop(capture);
                    if active.lock().stop {
                        return Ok(());
                    }
                    last_at = pictures.lock().last;
                    pin.resolve(true)?;
                    budget.next(
                        epoch.transport_lost.load(Ordering::Acquire),
                        pictures
                            .lock()
                            .first_import
                            .map(|first| ended_at.saturating_duration_since(first)),
                        &why,
                    )?;
                    (sink.interruption)(
                        format!("pinned capture helper exited; restarting: {why}"),
                        false,
                    );
                    #[cfg(debug_assertions)]
                    if fixture {
                        let deadline = Instant::now() + Duration::from_secs(10);
                        while isocast_diagnostics::developer()
                            && !active.lock().stop
                            && Instant::now() < deadline
                        {
                            thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
            })();
            if let Err(e) = result {
                if let Some(tx) = startup {
                    let _ = tx.send(Err(format!("{e:#}")));
                } else if !active.lock().stop {
                    (sink.interruption)(format!("{e:#}"), true);
                }
            }
        }
    };
    let worker = thread::Builder::new()
        .name("pinned WGC capture".into())
        .spawn(run)?;
    let capture = PinnedCapture {
        active,
        worker: Some(worker),
    };
    let format = rx
        .recv_timeout(Duration::from_secs(9))?
        .map_err(anyhow::Error::msg)?;
    Ok((capture, format))
}

/// Only isolated debug daemons with a fixture window and their adjacent helper.
#[cfg(debug_assertions)]
pub(crate) fn fixture_mode() -> Option<String> {
    let setup = std::path::PathBuf::from(std::env::var_os("ISOCAST_SETUP")?);
    let socket = std::path::PathBuf::from(std::env::var_os("ISOCAST_SOCKET")?);
    if !setup.is_absolute()
        || setup.parent() != socket.parent()
        || !matches!(socket.file_name()?.to_str()?, "d.sock")
    {
        return None;
    }
    std::env::var("ISOCAST_TEST_WGC_HELPER")
        .ok()
        .filter(|v| matches!(v.as_str(), "fixture" | "native"))
}

#[cfg(debug_assertions)]
pub(crate) fn fixture_pixel(target: CaptureTarget) -> Option<u8> {
    let CaptureTarget::Window(handle) = target else {
        return None;
    };
    let prop = unsafe {
        GetPropW(
            HWND(handle as _),
            windows::core::w!("Isocast.E2E.WindowPixel"),
        )
    };
    match prop.0 as usize {
        1 => Some(32),
        2 => Some(224),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_POPUP,
    };
    use windows::core::w;

    #[test]
    fn pin_dies_with_window_and_is_not_a_title_process_or_handle_value() -> Result<()> {
        let window = || unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("same title"),
                WS_POPUP,
                0,
                0,
                32,
                32,
                None,
                None,
                None,
                None,
            )
        };
        let old = window()?;
        let pin = WindowPin::new(old.0 as usize)?;
        let other = WindowPin::new(old.0 as usize)?;
        assert!(other.valid());
        drop(other);
        assert!(pin.valid());
        unsafe {
            DestroyWindow(old)?;
        }
        let decoy = window()?;
        let fresh = WindowPin::new(decoy.0 as usize)?;
        assert_ne!(fresh.token, pin.token);
        assert!(!pin.valid());
        drop(pin);
        assert!(fresh.valid());
        drop(fresh);
        assert!(unsafe { GetPropW(decoy, PIN_NAME) }.0.is_null());
        unsafe {
            DestroyWindow(decoy)?;
        }
        Ok(())
    }

    #[test]
    fn only_transport_loss_reopens_and_ready_is_not_recovery() {
        let mut budget = Budget::default();
        let short = Some(Duration::from_secs(1));
        assert!(budget.next(false, short, "target closed").is_err());
        budget.next(true, short, "pipe closed").unwrap();
        assert!(budget.next(true, None, "no fresh frame").is_err());
        for _ in 1..3 {
            budget.next(true, short, "pipe closed").unwrap();
        }
        assert!(budget.next(true, short, "flapping").is_err());
        budget
            .next(true, Some(Duration::from_secs(60)), "new outage")
            .unwrap();
        assert_eq!(budget.restarts, 1);
    }

    #[test]
    fn stale_or_late_first_picture_cannot_resume_capture() {
        let t = Instant::now();
        let stamp = Time::from_nanos;
        let mut pictures = Pictures {
            opened: Some(stamp(20)),
            previous: Some(stamp(15)),
            deadline: Some(t + Duration::from_secs(5)),
            last: None,
            first_import: None,
        };
        assert!(!pictures.admit(stamp(15), t));
        assert!(!pictures.admit(stamp(19), t));
        assert!(!pictures.admit(stamp(20), t + Duration::from_secs(5)));
        assert_eq!(pictures.last, None);
        assert!(pictures.admit(stamp(21), t + Duration::from_secs(4)));
        // A static, already-resumed window is not required to produce every five seconds.
        assert!(pictures.admit(stamp(120), t + Duration::from_secs(12)));
    }
}
