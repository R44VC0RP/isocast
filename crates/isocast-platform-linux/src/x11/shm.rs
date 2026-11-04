//! SysV segment shared with the X server, not with other users. Linux
//! permits attach after IPC_RMID: mark it as soon as our own attach succeeds,
//! even before asking the server. SIGKILL cannot orphan a live capture's SHM.
use anyhow::{Result, ensure};
use std::{ptr, sync::Arc};
use x11rb::{
    connection::Connection, protocol::shm::ConnectionExt, rust_connection::RustConnection,
};

pub(super) struct Segment {
    conn: Arc<RustConnection>,
    pub id: u32,
    address: *mut libc::c_void,
    pub len: usize,
}

impl Segment {
    pub fn new(conn: &Arc<RustConnection>, len: usize) -> Result<Self> {
        let id = conn.generate_id()?;
        // SAFETY: kernel chooses the id/address; no Rust-owned memory is passed.
        let sys = unsafe { libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600) };
        ensure!(sys >= 0, "shmget: {}", std::io::Error::last_os_error());
        // SAFETY: attach this newly allocated segment at a kernel-chosen address.
        let address = unsafe { libc::shmat(sys, ptr::null(), 0) };
        // SAFETY: IPC_RMID takes no buffer. Also removes an unattached segment on failure.
        let removed = unsafe { libc::shmctl(sys, libc::IPC_RMID, ptr::null_mut()) };
        ensure!(address != (-1isize as *mut _), "shmat failed");
        let segment = Self {
            conn: conn.clone(),
            address,
            len,
            id,
        };
        ensure!(removed == 0, "could not mark capture segment for removal");
        conn.shm_attach(id, sys as u32, false)?.check()?;
        Ok(segment)
    }

    /// Called only after shm_get_image replied; no outstanding server writes.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: successful shmat of len bytes, owned until Drop. The server
        // filled this region and our next get_image requires exclusive access.
        unsafe { std::slice::from_raw_parts(self.address.cast(), self.len) }
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        if let Ok(cookie) = self.conn.shm_detach(self.id) {
            let _ = cookie.check();
        }
        // SAFETY: exactly one local attach; IPC_RMID reclaims after last detach.
        unsafe {
            libc::shmdt(self.address);
        }
    }
}
