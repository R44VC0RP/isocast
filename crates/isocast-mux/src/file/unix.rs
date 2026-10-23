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

#[cfg(target_os = "linux")]
pub(super) fn manifest_names(
    directory: &File,
    limit: usize,
) -> io::Result<Vec<std::ffi::OsString>> {
    use std::collections::BTreeSet;

    if limit > 1000 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "manifest name limit exceeds 1000",
        ));
    }
    // procfs resolves this live descriptor, not the directory's original
    // pathname. read_dir opens its own enumeration cursor; a rename cannot
    // redirect enumeration to a replacement folder at the original name.
    let entries = std::fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))?;
    let mut names = BTreeSet::new();
    for entry in entries {
        let name = entry?.file_name();
        let text = name.to_string_lossy();
        if !text.starts_with('.') && text.to_ascii_lowercase().ends_with(".session.json") {
            names.insert(name);
            if names.len() > limit {
                names.pop_first();
            }
        }
    }
    Ok(names.into_iter().rev().collect())
}

#[cfg(target_os = "linux")]
pub(super) fn replace_metadata(
    directory: &File,
    name: &OsStr,
    bytes: &[u8],
    durable: bool,
) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Match the Windows policy: refuse an observed symlink or hard link,
    // and never replace an entry that appeared after observing no target.
    let permissions = match open_child(directory, name, OpenMode::Read) {
        Ok(existing) => Some(existing.metadata()?.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("metadata destination is not safe to replace"),
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let mut temporary = None;
    for _ in 0..64 {
        let leaf = format!(
            ".isocast-{}-{stamp:x}-{:x}.partial",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        if name == OsStr::new(&leaf) {
            continue;
        }
        match open_child(directory, OsStr::new(&leaf), OpenMode::Create) {
            Ok(file) => {
                temporary = Some((CString::new(leaf)?, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("could not create metadata temporary"),
        }
    }
    let Some((leaf, mut file)) = temporary else {
        bail!("could not create metadata temporary after 64 name collisions");
    };
    // On failure retain the private partial rather than unlinking a pathname
    // that an external actor may have replaced. Hidden partials are never
    // treated as session manifests by recovery.
    file.write_all(bytes)
        .context("could not write metadata temporary; partial retained")?;
    let replace = permissions.is_some();
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)
            .context("could not preserve metadata permissions; partial retained")?;
    }
    if durable {
        file.sync_all()
            .context("could not flush metadata temporary; partial retained")?;
    }
    let destination = CString::new(name.as_bytes())?;
    let flags = if replace { 0 } else { libc::RENAME_NOREPLACE };
    // SAFETY: both names are single NUL-terminated leaves in the same pinned
    // directory. rename replaces directory entries; it never follows a link
    // or writes through an existing target into some other file.
    if unsafe {
        libc::renameat2(
            directory.as_raw_fd(),
            leaf.as_ptr(),
            directory.as_raw_fd(),
            destination.as_ptr(),
            flags,
        )
    } != 0
    {
        return Err(io::Error::last_os_error())
            .context("could not publish metadata; partial retained");
    }
    if durable {
        directory
            .sync_all()
            .context("metadata published, but directory flush failed")?;
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::file::Directory;
    use std::io::Read;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn manifest_operations_stay_in_the_pinned_directory() {
        let root = std::env::temp_dir().join(format!(
            "isocast-metadata-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let original = root.join("original");
        let moved = root.join("moved");
        std::fs::create_dir(&original).unwrap();
        let directory = Directory::for_repair(&original).unwrap();
        for name in [
            "a.session.json",
            "b.session.json",
            "c.session.json",
            ".hidden.session.json",
        ] {
            directory
                .replace_metadata(OsStr::new(name), b"old", true)
                .unwrap();
        }
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("c.session.json"), b"replacement folder").unwrap();
        std::fs::set_permissions(
            moved.join("c.session.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert_eq!(
            directory.manifest_names(2).unwrap(),
            ["c.session.json", "b.session.json"]
        );
        directory
            .replace_metadata(OsStr::new("c.session.json"), b"new", true)
            .unwrap();
        let mut bytes = String::new();
        directory
            .open_for_read(OsStr::new("c.session.json"))
            .unwrap()
            .read_to_string(&mut bytes)
            .unwrap();
        assert_eq!(bytes, "new");
        assert_eq!(
            std::fs::metadata(moved.join("c.session.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o644
        );
        assert_eq!(
            std::fs::read(original.join("c.session.json")).unwrap(),
            b"replacement folder"
        );
        let outside = root.join("outside");
        std::fs::write(&outside, b"untouched").unwrap();
        symlink(&outside, moved.join("link.session.json")).unwrap();
        assert!(
            directory
                .replace_metadata(OsStr::new("link.session.json"), b"bad", true)
                .is_err()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"untouched");
        assert!(Directory::for_recording(&moved).is_err());
        drop(directory);
        let live = Directory::for_recording(&moved).unwrap();
        assert!(Directory::for_repair(&moved).is_err());
        drop(live);
        std::fs::remove_dir_all(root).unwrap();
    }
}
