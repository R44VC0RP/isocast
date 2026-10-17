//! Read-only Windows process/filesystem hooks; capture and writing are still
//! unavailable. These queries do not open devices or change system settings.

use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
    GetFileInformationByHandle,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

pub(super) fn process_cpu_seconds() -> Option<f64> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: borrowed current-process pseudo-handle and valid output pointers.
    if unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return None;
    }
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Some(ticks(kernel).checked_add(ticks(user))? as f64 / 10_000_000.0)
}

pub(super) fn same_volume(left: &Path, right: &Path) -> io::Result<bool> {
    Ok(volume_id(left)? == volume_id(right)?)
}

fn volume_id(path: &Path) -> io::Result<u32> {
    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: live owned file/directory handle and a correctly sized output.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info.dwVolumeSerialNumber)
}
