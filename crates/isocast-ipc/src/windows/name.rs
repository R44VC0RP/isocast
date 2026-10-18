use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{null, null_mut};

use sha2::{Digest, Sha256};
use windows_sys::Win32::Globalization::{LCMAP_UPPERCASE, LCMapStringEx};

use super::last_error;

pub(super) fn pipe_name(path: &Path, sid: &[u8]) -> io::Result<Vec<u16>> {
    // Resolve relative paths, separators and dot components once. Resolve the
    // existing parent (junctions, symlinks, short names and verbatim prefixes),
    // not the logical socket itself: Windows never creates that file.
    let absolute = std::path::absolute(path)?;
    let file_name = absolute.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "pipe path needs a file name")
    })?;
    let parent = absolute
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pipe path needs a parent"))?;
    let normalized = parent.canonicalize()?.join(file_name);
    let wide: Vec<u16> = normalized.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "pipe path contains NUL",
        ));
    }
    let length = i32::try_from(wide.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pipe path is too long"))?;
    // Windows' invariant case mapping, not the user's locale or lossy UTF-8.
    // Logical endpoints are case-insensitive, including in case-sensitive dirs.
    let locale = [0u16];
    let size = unsafe {
        LCMapStringEx(
            locale.as_ptr(),
            LCMAP_UPPERCASE,
            wide.as_ptr(),
            length,
            null_mut(),
            0,
            null(),
            null(),
            0,
        )
    };
    if size == 0 {
        return Err(last_error());
    }
    let mut folded = vec![0u16; size as usize];
    let written = unsafe {
        LCMapStringEx(
            locale.as_ptr(),
            LCMAP_UPPERCASE,
            wide.as_ptr(),
            length,
            folded.as_mut_ptr(),
            size,
            null(),
            null(),
            0,
        )
    };
    if written == 0 {
        return Err(last_error());
    }
    let mut hash = Sha256::new();
    // Versioned, length-delimited binary fields: neither SID text formatting
    // nor an ambiguous SID/path concatenation is part of the namespace.
    // SIDs are public identifiers, not secrets: this isolates users' logical
    // paths, but cannot prevent targeted squatting. Keep all peer/ACL checks.
    hash.update(b"isocast.local-pipe\0v2\0");
    hash.update((sid.len() as u64).to_le_bytes());
    hash.update(sid);
    hash.update((u64::from(written as u32) * 2).to_le_bytes());
    for unit in &folded[..written as usize] {
        hash.update(unit.to_le_bytes());
    }
    Ok(format!(r"\\.\pipe\isocast-{:x}", hash.finalize())
        .encode_utf16()
        .chain(Some(0))
        .collect())
}
