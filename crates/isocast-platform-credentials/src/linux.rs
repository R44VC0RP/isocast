//! Bootstrap store: plaintext 0600 files in an owner-only XDG directory.
//! Not a keyring: processes running as this user can read the credentials.

use std::ffi::CString;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

const MAX_SECRET: u64 = 64 * 1024;
/// A writer's temporary file lives for milliseconds; one this old whose
/// process has exited was abandoned by a crash.
const STALE_TEMP: Duration = Duration::from_secs(10 * 60);
/// Bounds the directory scan and removals per write.
const CLEANUP_SCAN: usize = 256;
const CLEANUP_REMOVE: usize = 16;
const WRITE_LOCK_WAIT: Duration = Duration::from_secs(2);

fn root() -> Result<PathBuf> {
    let config = match std::env::var_os("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(std::env::var_os("HOME").context("secret store: HOME is not set")?)
            .join(".config"),
    };
    ensure!(
        config.is_absolute(),
        "secret store: XDG_CONFIG_HOME must be absolute"
    );
    Ok(config.join("isocast/credentials"))
}

fn directory(path: &Path) -> Result<File> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| {
            format!(
                "secret store: cannot create {}; make its parent writable by you, or set \
                 XDG_CONFIG_HOME to a directory you own",
                path.display()
            )
        })?;
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| {
            format!(
                "secret store: cannot open {} as a directory (symlinks are refused); move it \
                 aside and run the command again to recreate it as a private 0700 directory",
                path.display()
            )
        })?;
    check_owner(&dir, path, true)?;
    Ok(dir)
}

fn command_path(path: &Path) -> String {
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    // The CLI redacts the literal home directory to ~. A quoted '~' would
    // not expand in a shell, so use a quoted variable for home-relative fixes.
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(relative) = path.strip_prefix(home)
    {
        return if relative.as_os_str().is_empty() {
            "\"$HOME\"".into()
        } else {
            format!("\"$HOME\"/{}", quote(relative))
        };
    }
    quote(path)
}

fn check_owner(file: &File, path: &Path, dir: bool) -> Result<()> {
    let meta = file
        .metadata()
        .with_context(|| format!("secret store: cannot inspect {}", path.display()))?;
    // SAFETY: geteuid reads the process effective user id.
    let uid = unsafe { libc::geteuid() };
    let quoted = command_path(path);
    let path = path.display();
    ensure!(
        meta.uid() == uid,
        "secret store: {path} is owned by uid {}, not the current user (uid {uid}); run \
         `sudo chown {uid} {quoted}` or remove it",
        meta.uid()
    );
    ensure!(
        if dir {
            meta.is_dir()
        } else {
            meta.is_file() && meta.nlink() == 1
        },
        "secret store: {path} must be {}",
        if dir {
            "a private directory; move it aside and run the command again"
        } else {
            "a regular file with no other hard links; remove it and store the secret again"
        }
    );
    ensure!(
        meta.mode() & 0o077 == 0,
        "secret store: {path} has mode {:04o} but must be owner-only; run `chmod {} {quoted}`",
        meta.mode() & 0o7777,
        if dir { "700" } else { "600" }
    );
    Ok(())
}

fn key(service: &str, account: &str) -> CString {
    let mut hash = Sha256::new();
    hash.update((service.len() as u64).to_be_bytes());
    hash.update(service.as_bytes());
    hash.update(account.as_bytes());
    CString::new(format!("{:x}", hash.finalize())).expect("hex contains no NUL")
}

fn open(dir: &File, name: &CString, flags: i32) -> std::io::Result<File> {
    // SAFETY: dir and name are live, flags never request an unnamed file;
    // the mode is supplied for O_CREAT. Returned fd is owned exactly once.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned file descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn entry(root: &Path, name: &CString) -> PathBuf {
    root.join(name.to_str().expect("names are ASCII"))
}

fn get(dir: &File, root: &Path, name: &CString) -> Result<Option<String>> {
    let path = entry(root, name);
    let file = match open(dir, name, libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "secret store: cannot open {} (symlinks are refused); remove it and store \
                     the secret again",
                    path.display()
                )
            });
        }
    };
    check_owner(&file, &path, false)?;
    ensure!(
        file.metadata()?.len() <= MAX_SECRET,
        "secret store: {} exceeds 64 KiB; remove it and store the secret again",
        path.display()
    );
    let mut value = String::new();
    file.take(MAX_SECRET + 1)
        .read_to_string(&mut value)
        .with_context(|| {
            format!(
                "secret store: cannot read {} as text; remove it and store the secret again",
                path.display()
            )
        })?;
    ensure!(
        value.len() as u64 <= MAX_SECRET,
        "secret store: {} exceeds 64 KiB; remove it and store the secret again",
        path.display()
    );
    Ok(Some(value))
}

/// The pid in a `.write-<pid>-<serial>` name `set` creates, or None for any
/// other name.
fn temp_pid(name: &str) -> Option<libc::pid_t> {
    let (pid, serial) = name.strip_prefix(".write-")?.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(pid) || !digits(serial) {
        return None;
    }
    let parsed = pid.parse::<libc::pid_t>().ok().filter(|pid| *pid > 0)?;
    let number = serial.parse::<u64>().ok()?;
    (parsed.to_string() == pid && number.to_string() == serial).then_some(parsed)
}

fn process_alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    let found = unsafe { libc::kill(pid, 0) } == 0;
    found || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Removes temporary files that crashed writers left behind. A candidate
/// must have exactly `set`'s name, be a regular single-link file owned by
/// this user (never a symlink, which is not followed), be older than
/// [`STALE_TEMP`], and its writer process must be gone, so an active write
/// is never touched. The caller holds the directory write lock, serializing
/// cleanup with every Isocast writer. Best effort and bounded.
fn cleanup(dir: &File) -> usize {
    let Ok(entries) = std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd())) else {
        return 0;
    };
    // SAFETY: geteuid reads the process effective user id.
    let uid = unsafe { libc::geteuid() };
    let own = std::process::id() as libc::pid_t;
    let mut removed = 0;
    for item in entries.take(CLEANUP_SCAN).flatten() {
        if removed == CLEANUP_REMOVE {
            break;
        }
        let name = item.file_name();
        let Some(pid) = name.to_str().and_then(temp_pid) else {
            continue;
        };
        if pid == own || process_alive(pid) {
            continue;
        }
        let Ok(name) = CString::new(name.into_encoded_bytes()) else {
            continue;
        };
        // SAFETY: stat is plain old data; fstatat writes it on success.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: single component relative to the pinned directory; the
        // final component is never followed.
        if unsafe {
            libc::fstatat(
                dir.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            continue;
        }
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_mtime.max(0) as u64);
        let stale = SystemTime::now()
            .duration_since(modified)
            .is_ok_and(|age| age >= STALE_TEMP);
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG
            || stat.st_uid != uid
            || stat.st_nlink != 1
            || !stale
        {
            continue;
        }
        // SAFETY: single component in the pinned owner-only directory;
        // unlinkat removes a name and never follows a link.
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } == 0 {
            removed += 1;
        }
    }
    removed
}

fn set(dir: &File, root: &Path, name: &CString, value: &str) -> Result<()> {
    // A stale name must not be replaced by another writer between inspection
    // and unlink. The private directory lock is shared by cleanup and writes.
    let deadline = Instant::now() + WRITE_LOCK_WAIT;
    loop {
        match dir.try_lock() {
            Ok(()) => break,
            Err(error) => {
                let error: std::io::Error = error.into();
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                return Err(error).with_context(|| format!(
                    "secret store: cannot lock {}; retry after other writes finish, or set XDG_CONFIG_HOME to a local filesystem supporting advisory locks",
                    root.display()));
            }
        }
    }
    struct Unlock<'a>(&'a File);
    impl Drop for Unlock<'_> {
        fn drop(&mut self) {
            if let Err(error) = self.0.unlock() {
                eprintln!("secret store: could not release write lock: {error}");
            }
        }
    }
    let _unlock = Unlock(dir);
    ensure!(
        value.len() as u64 <= MAX_SECRET,
        "secret store: value for {} exceeds 64 KiB; store a smaller secret",
        entry(root, name).display()
    );
    cleanup(dir);
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let temp = CString::new(format!(
        ".write-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ))?;
    let mut file =
        open(dir, &temp, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL).with_context(|| {
            format!(
                "secret store: cannot create a file in {}; run `chmod 700 {}`",
                root.display(),
                command_path(root)
            )
        })?;
    let result = (|| {
        file.write_all(value.as_bytes())?;
        file.sync_all()?;
        // SAFETY: both single-component names are relative to the same
        // pinned private directory. Rename replaces, never follows a link.
        if unsafe {
            libc::renameat(
                dir.as_raw_fd(),
                temp.as_ptr(),
                dir.as_raw_fd(),
                name.as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error()).context("installing secret");
        }
        dir.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        // SAFETY: this is only our exclusive temporary name in the pinned directory.
        unsafe { libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0) };
    }
    result.with_context(|| format!("secret store: could not persist {}; check directory permissions and free disk space, then retry", entry(root, name).display()))
}

pub fn secret(service: &str, account: &str) -> Result<Option<String>> {
    let root = root()?;
    get(&directory(&root)?, &root, &key(service, account))
}

pub fn has_secret(service: &str, account: &str) -> Result<bool> {
    Ok(secret(service, account)?.is_some())
}

pub fn set_secret(service: &str, account: &str, value: &str) -> Result<()> {
    let root = root()?;
    set(&directory(&root)?, &root, &key(service, account), value)
}

pub fn delete_secret(service: &str, account: &str) -> Result<bool> {
    let root = root()?;
    let dir = directory(&root)?;
    let name = key(service, account);
    // SAFETY: single component beneath an owned directory; unlink never
    // follows a symlink and cannot delete a directory without AT_REMOVEDIR.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(false);
        }
        return Err(error).with_context(|| {
            format!(
                "secret store: cannot remove {}; run `chmod 700 {}`",
                entry(&root, &name).display(),
                command_path(&root)
            )
        });
    }
    dir.sync_all()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn scratch(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("isocast-credentials-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    #[test]
    fn private_atomic_roundtrip_and_link_refusal() {
        let root = scratch("roundtrip");
        let dir = directory(&root).unwrap();
        let name = key("service", "../account");
        assert!(get(&dir, &root, &name).unwrap().is_none());
        set(&dir, &root, &name, "test-only-value").unwrap();
        let path = root.join(name.to_str().unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            get(&dir, &root, &name).unwrap().as_deref(),
            Some("test-only-value")
        );
        set(&dir, &root, &name, "replacement").unwrap();
        assert_eq!(
            get(&dir, &root, &name).unwrap().as_deref(),
            Some("replacement")
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = format!("{:#}", get(&dir, &root, &name).unwrap_err());
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert!(error.contains("chmod 600"), "{error}");
        assert!(!error.contains("replacement"), "{error}");
        std::fs::remove_file(&path).unwrap();
        symlink("/dev/null", &path).unwrap();
        let error = format!("{:#}", get(&dir, &root, &name).unwrap_err());
        assert!(error.contains(&path.display().to_string()), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn open_directory_names_path_and_chmod() {
        let root = scratch("mode");
        drop(directory(&root).unwrap());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = format!("{:#}", directory(&root).unwrap_err());
        assert!(error.contains(&root.display().to_string()), "{error}");
        assert!(error.contains("chmod 700"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn permission_commands_survive_home_redaction_and_spaces() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(command_path(&home.join("a b'c")), "\"$HOME\"/'a b'\\''c'");
        assert_eq!(command_path(Path::new("a b'c")), "'a b'\\''c'");
    }

    #[test]
    fn temp_names_are_exactly_sets() {
        assert_eq!(temp_pid(".write-123-0"), Some(123));
        for name in [
            ".write-0-1",
            ".write-12",
            ".write-12-",
            ".write--1",
            ".write-1-2-3",
            ".write-+1-2",
            "write-1-2",
            ".write-99999999999999-1",
            ".write-00123-0",
            ".write-123-00",
            ".write-123-18446744073709551616",
        ] {
            assert_eq!(temp_pid(name), None, "{name}");
        }
    }

    #[test]
    fn cleanup_removes_only_stale_abandoned_temp_files() {
        let root = scratch("cleanup");
        let dir = directory(&root).unwrap();
        // A pid whose process has exited (and been reaped).
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        assert!(!process_alive(dead as libc::pid_t));
        let old = SystemTime::now() - STALE_TEMP - Duration::from_secs(60);
        let make = |name: &str, aged: bool| {
            let path = root.join(name);
            let file = File::create(&path).unwrap();
            if aged {
                file.set_modified(old).unwrap();
            }
            path
        };
        let stale = make(&format!(".write-{dead}-0"), true);
        let fresh = make(&format!(".write-{dead}-1"), false);
        let own = make(&format!(".write-{}-0", std::process::id()), true);
        let other = make(".write-notes", true);
        let secret = make(&key("service", "account").into_string().unwrap(), true);
        // A symlink with a temp name: neither it nor its target is touched.
        let target = make("target", true);
        let link = root.join(format!(".write-{dead}-2"));
        symlink(&target, &link).unwrap();
        // A hard-linked temp name is someone else's data, never removed.
        let linked = root.join(format!(".write-{dead}-3"));
        std::fs::hard_link(&target, &linked).unwrap();

        dir.try_lock().unwrap();
        assert_eq!(cleanup(&dir), 1);
        dir.unlock().unwrap();
        assert!(!stale.exists());
        for kept in [&fresh, &own, &other, &secret, &target, &linked] {
            assert!(kept.exists(), "{}", kept.display());
        }
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());

        // A write sweeps abandoned temp files and leaves its own none.
        let again = make(&format!(".write-{dead}-4"), true);
        set(&dir, &root, &key("service", "other"), "test-only-value").unwrap();
        assert!(!again.exists());
        assert!(std::fs::read_dir(&root).unwrap().flatten().all(|entry| {
            !entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!(".write-{}-", std::process::id()))
                || entry.path() == own
        }));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_writer_waits_for_the_active_write() {
        let root = scratch("busy");
        let dir = directory(&root).unwrap();
        let other = directory(&root).unwrap();
        dir.try_lock().unwrap();
        let (entered, entering) = std::sync::mpsc::sync_channel(1);
        let (done, finished) = std::sync::mpsc::sync_channel(1);
        let path = root.clone();
        let writer = std::thread::spawn(move || {
            assert!(other.try_lock().is_err());
            entered.send(()).unwrap();
            done.send(set(
                &other,
                &path,
                &key("service", "account"),
                "test-only-value",
            ))
            .unwrap();
        });
        entering.recv_timeout(Duration::from_secs(1)).unwrap();
        // This is a real kernel-lock integration check, not a simulated
        // clock: the contending write must neither fail nor finish early.
        assert!(matches!(
            finished.recv_timeout(Duration::from_millis(30)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        dir.unlock().unwrap();
        finished.recv_timeout(WRITE_LOCK_WAIT).unwrap().unwrap();
        writer.join().unwrap();
        assert_eq!(
            get(&dir, &root, &key("service", "account"))
                .unwrap()
                .as_deref(),
            Some("test-only-value")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
