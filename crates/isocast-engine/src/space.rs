//! How much disk a recording needs.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

use crate::WriterSpec;

/// How long an open-ended recording must have room for before it starts.
pub const MIN_OPEN_ENDED_SECONDS: f64 = 10.0 * 60.0;

/// Device of an existing path or its closest existing parent. Only a local
/// calibration on this same filesystem may provide a disk-speed observation.
pub fn device(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        path.ancestors()
            .find_map(|p| fs::metadata(p).ok().map(|m| m.dev()))
    }
    #[cfg(windows)]
    {
        windows_device(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        None
    }
}

#[cfg(windows)]
fn windows_device(path: &Path) -> Option<u64> {
    use std::hash::{DefaultHasher, Hash, Hasher};
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null_mut;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, GetFinalPathNameByHandleW,
        GetVolumeInformationByHandleW, VOLUME_NAME_GUID,
    };

    // Resolve relative and root-relative paths before walking missing parents.
    // Follow links: calibration belongs to the target volume, not the link.
    let path = std::path::absolute(path).ok()?;
    let mut file = None;
    for parent in path.ancestors() {
        match fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(parent)
        {
            Ok(opened) => {
                file = Some(opened);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A dangling junction is not a missing directory on this
                // volume. Its target's identity is unknown.
                match fs::symlink_metadata(parent) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    _ => return None,
                }
            }
            // Do not borrow a parent's identity across an inaccessible mount.
            Err(_) => return None,
        }
    }
    let file = file?;
    let mut serial = 0;
    // SAFETY: the handle stays live for both queries; only serial is requested.
    if unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            null_mut(),
            0,
            &mut serial,
            null_mut(),
            null_mut(),
            null_mut(),
            0,
        )
    } == 0
    {
        return None;
    }
    let mut name = vec![0u16; 512];
    for _ in 0..3 {
        // SAFETY: a live handle and a buffer with its actual UTF-16 capacity.
        let needed = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                name.as_mut_ptr(),
                name.len() as u32,
                VOLUME_NAME_GUID,
            )
        } as usize;
        if needed == 0 || needed > 32_768 {
            return None;
        }
        if needed >= name.len() {
            name.resize(needed, 0);
            continue;
        }
        // Serial numbers alone can collide (e.g. cloned filesystems). Include
        // the local volume GUID, independent of its drive letter/mount point.
        // Shares without a local GUID remain uncalibrated, rather than guessed.
        let end = name[..needed]
            .iter()
            .position(|&unit| unit == u16::from(b'}'))?;
        let volume = String::from_utf16(&name[..=end]).ok()?.to_ascii_lowercase();
        if !volume.starts_with(r"\\?\volume{") {
            return None;
        }
        let mut identity = DefaultHasher::new();
        volume.hash(&mut identity);
        serial.hash(&mut identity);
        return Some(identity.finish());
    }
    None
}

/// A generous estimate of a file's data rate, in bytes per second (the
/// protocol's `encoder_bytes_per_second`, which clients share). Real files
/// are usually far smaller (a still screen compresses to almost nothing);
/// this is for deciding whether a recording fits.
pub fn bytes_per_second(spec: &WriterSpec) -> f64 {
    let channels: u32 = spec.audio.iter().map(|track| track.channels).sum();
    let video = &spec.video;
    isocast_protocol::encoder_bytes_per_second(
        &video.encoder,
        video.width,
        video.height,
        video.fps,
        channels,
    )
}

/// Bytes as a short human figure, like "3.2 GB".
pub fn human(bytes: f64) -> String {
    if bytes >= 1e9 {
        format!("{:.1} GB", bytes / 1e9)
    } else {
        format!("{:.0} MB", bytes / 1e6)
    }
}

/// Size of the file written to measure a disk's speed.
const SPEED_TEST_BYTES: usize = 64 << 20;

/// A name in `folder` for a temporary file or folder of this run only:
/// `prefix`, this process's id and a counter, so it never meets one
/// another run (or a person) left there.
pub fn owned_temp_name(folder: &Path, prefix: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    folder.join(format!(
        "{prefix}-{}-{}-{nanos:x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

/// A new folder in `folder` that this run owns (made with `create_dir`,
/// which fails rather than reuse anything already there).
pub fn owned_temp_dir(folder: &Path, prefix: &str) -> std::io::Result<std::path::PathBuf> {
    loop {
        let path = owned_temp_name(folder, prefix);
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
}
const SPEED_TEST_CHUNK: usize = 8 << 20;

/// Measure how fast `folder` takes data, in bytes per second, by writing a
/// temporary file and waiting until the disk has it. Catches slow and
/// network volumes; it measures a short burst, so sustained speed on some
/// drives is lower.
pub fn write_speed(folder: &Path) -> std::io::Result<f64> {
    let path = owned_temp_name(folder, ".isocast-speed-test");
    // Bytes that don't compress, in case the file system compresses.
    let mut chunk = vec![0u8; SPEED_TEST_CHUNK];
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for byte in chunk.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *byte = x as u8;
    }
    let started = Instant::now();
    // A new file only: never one already there, or a link's target. If it
    // can't be made, nothing is removed (whatever is there isn't ours).
    let mut file = File::create_new(&path)?;
    let result = (|| {
        for _ in 0..SPEED_TEST_BYTES / SPEED_TEST_CHUNK {
            file.write_all(&chunk)?;
        }
        file.sync_all()
    })();
    let elapsed = started.elapsed().as_secs_f64();
    drop(file);
    // This run made it: removed whether the test worked or not.
    let _ = fs::remove_file(&path);
    result?;
    Ok(SPEED_TEST_BYTES as f64 / elapsed.max(1e-6))
}
