use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use super::OpenMode;

pub(super) fn open_directory(path: &Path) -> io::Result<File> {
    // Follow the caller's intentional folder link only here. All child opens
    // use this descriptor, not a joined or canonicalized path.
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
}

pub(super) fn open_child(directory: &File, name: &OsStr, mode: OpenMode) -> io::Result<File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in child name"))?;
    let access = match mode {
        OpenMode::Create => libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        OpenMode::Repair => libc::O_RDWR,
        OpenMode::Read => libc::O_RDONLY,
        OpenMode::Lock => libc::O_RDWR | libc::O_CREAT,
    };
    // O_NONBLOCK prevents a staged FIFO from blocking before fstat can reject
    // it. It has no effect on regular-file I/O. Never truncate an existing file.
    let flags = access | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    // SAFETY: directory is live and name is a single NUL-terminated component.
    // The mode argument is supplied for both of the O_CREAT dispositions.
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat just returned a new descriptor; this File is its sole owner.
    let file = unsafe { File::from_raw_fd(fd) };
    validate_file(&file)?;
    Ok(file)
}

pub(super) fn validate_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "child must be a regular file with exactly one link",
        ));
    }
    Ok(())
}

pub(super) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    file.read_at(buf, offset)
}

pub(super) fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    file.write_at(buf, offset)
}
