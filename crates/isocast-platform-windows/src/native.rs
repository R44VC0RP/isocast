use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
};
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::core::Owned;

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
