use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use parking_lot::Mutex;
use windows::Win32::Foundation::{
    CloseHandle, E_INVALIDARG, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
};
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW,
    GetCurrentProcess, GetCurrentThread, GetProcessInformation,
    PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
    PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
    ProcessPowerThrottling, SetEvent, SetProcessInformation, SetThreadPriority,
    THREAD_PRIORITY_ABOVE_NORMAL,
};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::{Owned, w};

/// An owned, unnamed NT sharing handle, created once per resource. D3D11
/// CreateSharedHandle cannot be called repeatedly on the same texture.
pub(crate) struct SharedHandle(Owned<HANDLE>);

// SAFETY: these are shareable kernel resource/fence handles, not apartment
// objects. The enclosing frame/device owns them until its last reference.
unsafe impl Send for SharedHandle {}
unsafe impl Sync for SharedHandle {}

impl SharedHandle {
    /// Takes ownership of a newly created/duplicated sharing handle, including
    /// on failure. Enforce non-inheritance even though every creator also
    /// requests it (null security attributes or DuplicateHandle(false)).
    pub(crate) unsafe fn new(handle: HANDLE) -> Result<Self> {
        let owned = Self(unsafe { Owned::new(handle) });
        unsafe { SetHandleInformation(owned.handle(), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
            .context("could not make the GPU sharing handle non-inheritable")?;
        Ok(owned)
    }

    pub(crate) fn handle(&self) -> HANDLE {
        *self.0
    }
}

pub(crate) struct Apartment;

impl Apartment {
    pub(crate) fn new() -> Result<Self> {
        // windows-rs caches agile WinRT activation factories for the process.
        // Ending the LAST MTA after a capture unloads their implementation on
        // Windows 10: reopening then calls an unmapped IsSupported vtable.
        // Keep one MTA reference for exactly that process-lifetime cache;
        // individual workers still balance their own RoInitialize below.
        static MTA: OnceLock<windows::core::Result<usize>> = OnceLock::new();
        MTA.get_or_init(|| unsafe { CoIncrementMTAUsage() }.map(|cookie| cookie.0 as usize))
            .as_ref()
            .map_err(|error| anyhow!("could not keep the capture MTA alive: {error}"))?;
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

pub(crate) struct Event(HANDLE);

// SAFETY: an owned kernel event can be signalled/waited on from any thread;
// the handle remains open until all users of the enclosing Arc have left.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self(unsafe { CreateEventW(None, false, false, None) }?))
    }

    pub(crate) fn handle(&self) -> HANDLE {
        self.0
    }

    pub(crate) fn signal(&self) {
        // This owned, unnamed event cannot be closed by another thread.
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct AudioTask(HANDLE);

impl Drop for AudioTask {
    fn drop(&mut self) {
        if let Err(error) = unsafe { AvRevertMmThreadCharacteristics(self.0) } {
            eprintln!("Windows audio scheduling cleanup: {error}");
        }
    }
}

/// Call once on an owned audio worker, not a shared executor thread. MMCSS
/// registration belongs to that thread and is reverted on normal thread exit.
pub fn audio_thread() -> Result<()> {
    thread_local! {
        static TASK: std::result::Result<AudioTask, String> = {
            let mut index = 0;
            unsafe { AvSetMmThreadCharacteristicsW(w!("Audio"), &mut index) }
                .map(AudioTask)
                .map_err(|error| format!("Windows audio MMCSS registration: {error}"))
        };
    }
    TASK.with(|task| {
        task.as_ref()
            .map(|_| ())
            .map_err(|error| anyhow!("{error}"))
    })
}

/// Call once on an owned capture, frame-clock or completion worker. Video
/// uses above-normal priority, NOT MMCSS/time-critical or a process-wide
/// priority-class change. The setting ends with the worker thread.
pub fn video_thread() -> Result<()> {
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) }
        .context("Windows video thread priority")
}

#[derive(Default)]
struct PowerState {
    users: usize,
    previous: Option<PROCESS_POWER_THROTTLING_STATE>,
    applied: u32,
}

impl PowerState {
    fn restore(&mut self) -> Result<()> {
        if let Some(previous) = &self.previous {
            set_power(previous).context("restore recording power policy")?;
            self.previous = None;
            self.applied = 0;
        }
        Ok(())
    }
}

fn power_state() -> &'static Mutex<PowerState> {
    static STATE: OnceLock<Mutex<PowerState>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

fn set_power(state: &PROCESS_POWER_THROTTLING_STATE) -> windows::core::Result<()> {
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            (state as *const PROCESS_POWER_THROTTLING_STATE).cast(),
            std::mem::size_of_val(state) as u32,
        )
    }
}

/// A recording-lifetime lease on this process's power-throttling opt-out.
/// Nested recordings share one saved policy; the last lease restores it.
/// No process priority-class change and no timeBeginPeriod request: the
/// high-resolution waitable timer does not require a raised timer period.
pub struct RecordingPowerGuard {
    applied: u32,
}

impl RecordingPowerGuard {
    /// `None` when the OS cannot query the policy to restore (notably Windows
    /// 10). Leave it untouched rather than guessing a prior/default policy.
    pub fn new() -> Result<Option<Self>> {
        let mut state = power_state().lock();
        if state.users == 0 {
            // If a prior restore failed, retain/retry its original snapshot,
            // never overwrite it with our own still-active policy.
            state.restore()?;
            let mut previous = PROCESS_POWER_THROTTLING_STATE {
                Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
                ..Default::default()
            };
            let queried = unsafe {
                GetProcessInformation(
                    GetCurrentProcess(),
                    ProcessPowerThrottling,
                    (&mut previous as *mut PROCESS_POWER_THROTTLING_STATE).cast(),
                    std::mem::size_of_val(&previous) as u32,
                )
            };
            if let Err(error) = queried {
                if error.code() == E_INVALIDARG {
                    return Ok(None);
                }
                return Err(error).context("read recording power policy");
            }
            let mut applied = PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION;
            let policy = |flags: u32| PROCESS_POWER_THROTTLING_STATE {
                Version: previous.Version,
                ControlMask: previous.ControlMask | flags,
                StateMask: previous.StateMask & !flags,
            };
            if let Err(error) = set_power(&policy(applied)) {
                if error.code() != E_INVALIDARG {
                    return Err(error).context("set recording power policy");
                }
                // IGNORE_TIMER_RESOLUTION is not available on every Windows
                // 10 build. Report the applied mask; do not claim that flag
                // worked, or disable unrelated controls to make a call pass.
                applied = PROCESS_POWER_THROTTLING_EXECUTION_SPEED;
                set_power(&policy(applied)).context("set recording execution-speed policy")?;
            }
            state.previous = Some(previous);
            state.applied = applied;
        }
        state.users = state
            .users
            .checked_add(1)
            .context("recording power lease overflow")?;
        Ok(Some(Self {
            applied: state.applied,
        }))
    }

    /// The PROCESS_POWER_THROTTLING_* flags actually accepted by Windows.
    pub fn applied_flags(&self) -> u32 {
        self.applied
    }
}

impl Drop for RecordingPowerGuard {
    fn drop(&mut self) {
        let mut state = power_state().lock();
        state.users -= 1;
        if state.users == 0
            && let Err(error) = state.restore()
        {
            eprintln!("Windows recording power cleanup: {error:#}");
        }
    }
}
