//! Handle-relative media I/O and lifetime ownership for the portable muxer.
//!
//! The directory is resolved once (an intentional folder symlink is fine).
//! Its lock and every child are opened relative to that same handle. Neither
//! a raw file handle nor an unlock operation escapes this module. These are
//! process-lifetime guarantees, not directory-entry/power-loss guarantees.

use std::ffi::OsStr;
#[cfg(any(windows, target_os = "linux"))]
use std::ffi::OsString;
use std::fs::{File, Metadata};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(windows)]
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

#[cfg(unix)]
#[path = "file/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "file/windows.rs"]
mod platform;

const FOLDER_LOCK: &str = ".isocast.lock";

/// True only when publication has not happened and a Windows reader or access
/// policy blocked it. Never retries a post-publication durability failure.
#[cfg(windows)]
pub fn metadata_blocked(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<MetadataBlocked>())
}

#[cfg(windows)]
#[derive(Debug)]
struct MetadataBlocked(io::Error);

#[cfg(windows)]
impl std::fmt::Display for MetadataBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(windows)]
impl std::error::Error for MetadataBlocked {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

#[derive(Clone, Copy)]
enum OpenMode {
    Create,
    Repair,
    Read,
    Lock,
    #[cfg(windows)]
    MetadataTemp,
    #[cfg(windows)]
    CancelEmpty,
}

/// An anchored recording (shared) or repair (exclusive) folder lease.
///
/// Clones share the lease, not a newly resolved path. Media files retain a
/// clone, so dropping this value cannot release their folder ownership.
#[derive(Clone, Debug)]
pub struct Directory {
    inner: Arc<DirectoryInner>,
}

#[derive(Debug)]
struct DirectoryInner {
    directory: File,
    _lock: File,
    repairing: bool,
    #[cfg(windows)]
    enumeration: Mutex<()>,
}

impl Directory {
    /// Fail rather than wait if a repair owns the folder, or locking fails.
    pub fn for_recording(folder: &Path) -> Result<Self> {
        Self::open(folder, false)
    }

    /// Exclusively try-lock the folder for a whole repair pass.
    pub fn for_repair(folder: &Path) -> Result<Self> {
        Self::open(folder, true)
    }

    fn open(folder: &Path, repairing: bool) -> Result<Self> {
        let directory = platform::open_directory(folder)
            .with_context(|| format!("could not open recording folder {}", folder.display()))?;
        // Unix allows unlinking a locked file. Also lock the directory inode
        // there, so replacing .isocast.lock cannot split this module's lease
        // domain. Windows denies delete sharing on the lock file instead.
        #[cfg(unix)]
        try_lock(&directory, repairing)
            .with_context(|| format!("could not lock recording folder {}", folder.display()))?;
        let lock = platform::open_child(&directory, OsStr::new(FOLDER_LOCK), OpenMode::Lock)
            .context("could not open recording folder's .isocast.lock")?;
        try_lock(&lock, repairing).context("recording folder is busy or cannot be locked")?;
        platform::validate_file(&lock)
            .context("recording folder lock is not a single regular file")?;
        Ok(Self {
            inner: Arc::new(DirectoryInner {
                directory,
                _lock: lock,
                repairing,
                #[cfg(windows)]
                enumeration: Mutex::new(()),
            }),
        })
    }

    /// Create a new child without replacing anything, under this shared lease.
    /// Failure after creation can leave an empty file; it is never unlinked
    /// through a path which might by then refer to a different file.
    pub fn create(&self, name: &OsStr) -> Result<MediaFile> {
        if self.inner.repairing {
            bail!("creating media requires a recording folder lease, not a repair lease");
        }
        self.open_media(name, OpenMode::Create)
    }

    /// Open a child for repair without releasing this exclusive folder lease.
    pub fn open_for_repair(&self, name: &OsStr) -> Result<MediaFile> {
        if !self.inner.repairing {
            bail!("repairing media requires an exclusive repair folder lease");
        }
        self.open_media(name, OpenMode::Repair)
    }

    /// Inspect a child, including a live recording, through a read-only handle.
    /// This does not lock media bytes or acquire writable media ownership.
    pub fn open_for_read(&self, name: &OsStr) -> Result<MediaFile> {
        self.open_media(name, OpenMode::Read)
    }

    /// Newest named manifests in the pinned folder, retaining at most `limit`
    /// names while scanning. This is not a snapshot: validate names on open.
    #[cfg(any(windows, target_os = "linux"))]
    pub fn manifest_names(&self, limit: usize) -> Result<Vec<OsString>> {
        // Windows keeps the enumeration cursor on the shared directory handle.
        #[cfg(windows)]
        let _enumeration = self
            .inner
            .enumeration
            .lock()
            .map_err(|_| anyhow::anyhow!("recording folder enumeration lock is poisoned"))?;
        platform::manifest_names(&self.inner.directory, limit)
            .context("could not enumerate recording folder")
    }

    /// The pinned folder's current normalized path, for reporting only. It can
    /// change immediately after this call; never use it to open children.
    #[cfg(windows)]
    pub fn path(&self) -> Result<PathBuf> {
        platform::path(&self.inner.directory).context("could not name pinned recording folder")
    }

    /// Publish complete metadata under either a recording or repair lease.
    /// Readers that hold the destination open can cause a sharing error; close
    /// them before retrying. On Windows, durable mode flushes the same file
    /// before and after its handle-relative rename, not the directory: no
    /// directory-entry durability guarantee. A post-publish flush error leaves
    /// the complete new file published and must not be treated as a rollback.
    /// Linux flushes the temporary before rename and the pinned directory
    /// after publication when durable mode is requested.
    #[cfg(any(windows, target_os = "linux"))]
    pub fn replace_metadata(&self, name: &OsStr, bytes: &[u8], durable: bool) -> Result<()> {
        validate_name(name)?;
        platform::replace_metadata(&self.inner.directory, name, bytes, durable)
            .with_context(|| format!("could not replace metadata {}", name.to_string_lossy()))
    }

    fn open_media(&self, name: &OsStr, mode: OpenMode) -> Result<MediaFile> {
        validate_name(name)?;
        let file = platform::open_child(&self.inner.directory, name, mode)
            .with_context(|| format!("could not open media {}", name.to_string_lossy()))?;
        let writable = !matches!(mode, OpenMode::Read);
        // Windows already owns writable media exclusively via its share mode.
        // A whole-file Windows byte lock would prevent ordinary live readers.
        #[cfg(unix)]
        if writable {
            try_lock(&file, true)
                .context("media is still in use or its lifetime lock is unsupported")?;
        }
        platform::validate_file(&file).with_context(|| {
            format!(
                "media {} is not a single regular file",
                name.to_string_lossy()
            )
        })?;
        Ok(MediaFile {
            file,
            _directory: self.clone(),
            position: 0,
            writable,
            #[cfg(windows)]
            created: matches!(mode, OpenMode::Create),
            #[cfg(windows)]
            name: name.to_os_string(),
        })
    }
}

/// A media descriptor together with the folder lease that authorized it.
/// There is deliberately no `File` conversion, clone, or raw-handle accessor.
#[derive(Debug)]
pub struct MediaFile {
    // Close media before releasing the folder lease (field drop order).
    file: File,
    _directory: Directory,
    // Windows FileExt::seek_read/seek_write change the OS cursor. Always use
    // explicit offsets, including for sequential I/O, so a read or header
    // patch cannot redirect the next append. No OS cursor is ever consulted.
    position: u64,
    writable: bool,
    #[cfg(windows)]
    created: bool,
    #[cfg(windows)]
    name: OsString,
}

impl MediaFile {
    /// Cancel only a newly created movie matching its initial header. The caller
    /// must have accepted no samples and supply its own nonempty header (<=256
    /// bytes). Any refusal retains the file, never truncates it. The same folder
    /// lease outlives both the original and deletion handles.
    #[cfg(windows)]
    pub(crate) fn cancel_empty(self, expected_header: &[u8]) -> Result<()> {
        let Self {
            file,
            _directory: directory,
            created,
            name,
            ..
        } = self;
        let cancel = (|| -> Result<()> {
            if !created {
                bail!("only media opened for creation can be cancelled");
            }
            platform::cancel_empty(&directory.inner.directory, file, &name, expected_header)
        })();
        cancel.with_context(|| {
            format!(
                "could not cancel empty media {}; file retained",
                name.to_string_lossy()
            )
        })
    }

    pub fn metadata(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }

    /// Commit file data using sync_data (FlushFileBuffers on Windows).
    /// Errors reach the caller; neither Drop nor Write::flush commits data.
    pub fn sync_data(&self) -> io::Result<()> {
        self.require_writable()?;
        self.file.sync_data()
    }

    pub fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        check_range(offset, buf.len())?;
        while !buf.is_empty() {
            match platform::read_at(&self.file, buf, offset) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    offset += n as u64;
                    buf = &mut buf[n..];
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    pub fn write_all_at(&self, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
        self.require_writable()?;
        check_range(offset, buf.len())?;
        while !buf.is_empty() {
            match platform::write_at(&self.file, buf, offset) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    offset += n as u64;
                    buf = &buf[n..];
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn require_writable(&self) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "media was opened for read-only inspection, not writing or repair",
            ));
        }
        Ok(())
    }
}

impl Write for MediaFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.require_writable()?;
        check_range(self.position, buf.len())?;
        let n = platform::write_at(&self.file, buf, self.position)?;
        self.position += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.require_writable()?;
        self.file.flush()
    }
}

impl Read for MediaFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        check_range(self.position, buf.len())?;
        let n = platform::read_at(&self.file, buf, self.position)?;
        self.position += n as u64;
        Ok(n)
    }
}

impl Seek for MediaFile {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.metadata()?.len()) + i128::from(offset),
        };
        if !(0..=i128::from(i64::MAX)).contains(&position) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid media seek offset",
            ));
        }
        self.position = position as u64;
        Ok(self.position)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.position)
    }
}

/// Resolve the parent once, acquire its shared lease, and create a new child.
pub fn create(path: &Path) -> Result<MediaFile> {
    let name = path
        .file_name()
        .context("media path must end with a plain file name")?;
    validate_name(name)?;
    let folder = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    Directory::for_recording(folder)?.create(name)
}

/// Acquire an exclusive folder lease and keep it for the returned file's life.
pub fn open_for_repair(folder: &Path, name: &OsStr) -> Result<MediaFile> {
    validate_name(name)?;
    Directory::for_repair(folder)?.open_for_repair(name)
}

/// Read-only inspection, sharing the folder with live recordings. Writes,
/// patches and sync_data on this value fail; the OS handle is also read-only.
pub fn open_for_read(folder: &Path, name: &OsStr) -> Result<MediaFile> {
    validate_name(name)?;
    Directory::for_recording(folder)?.open_for_read(name)
}

fn try_lock(file: &File, exclusive: bool) -> io::Result<()> {
    let result = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    result.map_err(Into::into)
}

fn check_range(offset: u64, len: usize) -> io::Result<()> {
    if offset
        .checked_add(len as u64)
        .is_none_or(|end| end > i64::MAX as u64)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "media byte range exceeds signed 64-bit offsets",
        ));
    }
    Ok(())
}

fn validate_name(name: &OsStr) -> Result<()> {
    // Use the same portable name policy on Unix comparison runs. ASCII is
    // preserved in OsStr's encoding, including Windows' non-UTF-8 names.
    let bytes = name.as_encoded_bytes();
    if bytes.is_empty()
        || bytes.iter().any(|b| *b < 32 || b"/\\:<>\"|?*".contains(b))
        || matches!(bytes.last(), Some(b'.' | b' '))
        || bytes.eq_ignore_ascii_case(FOLDER_LOCK.as_bytes())
    {
        bail!("{} is not a plain media file name", name.to_string_lossy());
    }
    let text = name.to_string_lossy();
    let stem = text
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let port = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"));
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || port.is_some_and(|n| {
        matches!(
            n,
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        )
    }) {
        bail!(
            "{} is a reserved device name, not a media file name",
            name.to_string_lossy()
        );
    }
    Ok(())
}
