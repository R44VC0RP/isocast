//! Secrets live in the desktop keyring through the Secret Service (GNOME
//! Keyring, KWallet, KeePassXC) when one answers on the session bus, as items
//! with the attributes `application`, `service` and `account`. Without one
//! (headless machines, CI, VMs), or with `ISOCAST_SECRET_STORE=file`, they
//! are plaintext 0600 files in an owner-only XDG directory, which processes
//! running as this user can read.
//! A file is always newer than the keyring's item: a keyring write deletes
//! it, so reads take the file first. A secret deleted while the keyring is
//! away leaves an empty file, which hides the keyring's older item until a
//! keyring write or delete removes it. Reading a file once the keyring
//! answers moves its value, or its delete, into the keyring. Isocast never starts the keyring's
//! unlock or confirmation prompts: a locked keyring is an error.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use zbus::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

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

fn file_secret(service: &str, account: &str) -> Result<Option<String>> {
    let root = root()?;
    get(&directory(&root)?, &root, &key(service, account))
}

fn file_set(service: &str, account: &str, value: &str) -> Result<()> {
    let root = root()?;
    set(&directory(&root)?, &root, &key(service, account), value)
}

fn file_delete(service: &str, account: &str) -> Result<bool> {
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

const SECRETS: &str = "org.freedesktop.secrets";
const SERVICE: &str = "org.freedesktop.Secret.Service";
const COLLECTION: &str = "org.freedesktop.Secret.Collection";
const ITEM: &str = "org.freedesktop.Secret.Item";
/// Every Secret Service call fails after this long.
const KEYRING_CALL: Duration = Duration::from_secs(5);
/// After a time-out, calls fail at once for this long, so stream status,
/// which checks every destination's key, doesn't wait on each.
const KEYRING_BACKOFF: Duration = Duration::from_secs(60);
static TIMED_OUT_AT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);
const TIMED_OUT: &str = "keyring: the session bus or its Secret Service (org.freedesktop.secrets) \
                         did not answer within 5 seconds; restart the desktop keyring, or log out \
                         and in, and retry (Isocast asks it again 60 s after a time-out)";

/// Isocast never starts a Secret Service prompt: the daemon would wait on it
/// while holding its state lock, as the macOS build never lets the Keychain
/// ask either.
const LOCKED: &str = "keyring: the desktop keyring is locked, and Isocast never asks to unlock \
                      it; unlock it (log in to the desktop session, or unlock it in Passwords \
                      and Keys, KWallet or KeePassXC) and retry, or start Isocast with \
                      ISOCAST_SECRET_STORE=file to keep its secrets in files";

fn confirm(action: &str) -> String {
    format!(
        "keyring: the Secret Service wants a prompt to confirm this {action}, and Isocast never \
         shows one; turn that confirmation off in your keyring app and retry"
    )
}

fn timed_out_at() -> std::sync::MutexGuard<'static, Option<Instant>> {
    TIMED_OUT_AT
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// zbus reports a call past [`KEYRING_CALL`] as a `TimedOut` I/O error;
/// `Keyring::open` reports a slow connection as a bare one.
fn timed_out(error: &anyhow::Error) -> bool {
    let timed_out = |io: &std::io::Error| io.kind() == std::io::ErrorKind::TimedOut;
    error.chain().any(|cause| {
        matches!(cause.downcast_ref::<zbus::Error>(), Some(zbus::Error::InputOutput(io)) if timed_out(io))
            || cause.downcast_ref::<std::io::Error>().is_some_and(timed_out)
    })
}

/// Calls a Secret Service method and decodes its reply into the type the
/// caller binds it to.
macro_rules! call {
    ($keyring:expr, $path:expr, $interface:expr, $method:expr, $body:expr) => {
        async {
            $keyring
                .conn
                .call_method(Some(SECRETS), $path, Some($interface), $method, $body)
                .await?
                .body()
                .deserialize()
        }
        .await
        .with_context(|| format!("keyring: {}.{} failed", $interface, $method))
    };
}

fn no_path(path: &ObjectPath<'_>) -> bool {
    path.as_str() == "/"
}

/// A Secret Service session with the "plain" algorithm: secrets cross only
/// the user's private session bus, which same-user processes could read
/// from the keyring anyway.
struct Keyring {
    conn: Connection,
    session: OwnedObjectPath,
}

impl Keyring {
    /// None when there is no session bus, or no Secret Service on it.
    async fn open() -> Result<Option<Self>> {
        // method_timeout covers calls only, not the connect, SASL and Hello.
        let connect = async {
            zbus::connection::Builder::session()?
                .method_timeout(KEYRING_CALL)
                .build()
                .await
        };
        let Ok(connected) = tokio::time::timeout(KEYRING_CALL, connect).await else {
            return Err(std::io::Error::from(std::io::ErrorKind::TimedOut).into());
        };
        let conn = match connected {
            Ok(conn) => conn,
            Err(zbus::Error::Address(_)) => return Ok(None),
            Err(zbus::Error::Connection(io, _))
                if matches!(
                    io.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error).context("keyring: cannot connect to the session bus"),
        };
        let reply = conn
            .call_method(
                Some(SECRETS),
                "/org/freedesktop/secrets",
                Some(SERVICE),
                "OpenSession",
                &("plain", Value::from("")),
            )
            .await;
        let (_, session): (OwnedValue, OwnedObjectPath) = match reply {
            Err(zbus::Error::MethodError(name, ..))
                if [
                    "org.freedesktop.DBus.Error.ServiceUnknown",
                    "org.freedesktop.DBus.Error.NameHasNoOwner",
                ]
                .contains(&name.as_str()) =>
            {
                return Ok(None);
            }
            reply => reply
                .and_then(|reply| reply.body().deserialize())
                .with_context(|| format!("keyring: {SERVICE}.OpenSession failed"))?,
        };
        Ok(Some(Self { conn, session }))
    }

    /// The unlocked items, then the locked ones.
    async fn search(
        &self,
        service: &str,
        account: &str,
    ) -> Result<(Vec<OwnedObjectPath>, Vec<OwnedObjectPath>)> {
        call!(
            self,
            "/org/freedesktop/secrets",
            SERVICE,
            "SearchItems",
            &attributes(service, account)
        )
    }

    async fn read(&self, service: &str, account: &str) -> Result<Option<String>> {
        let (unlocked, locked) = self.search(service, account).await?;
        let Some(item) = unlocked.into_iter().next() else {
            ensure!(locked.is_empty(), LOCKED);
            return Ok(None);
        };
        let (_, _, value, _): (OwnedObjectPath, Vec<u8>, Vec<u8>, String) =
            call!(self, item.as_str(), ITEM, "GetSecret", &(&self.session,))?;
        String::from_utf8(value).map(Some).with_context(|| {
            format!(
                "keyring: the item for {service} ({account}) is not text; delete it with \
                 `secret-tool clear application isocast service '{service}' account '{account}'` \
                 and store the secret again"
            )
        })
    }

    /// The collection behind the `default` alias, else `login`.
    async fn collection(&self) -> Result<OwnedObjectPath> {
        for alias in ["default", "login"] {
            let path: OwnedObjectPath = call!(
                self,
                "/org/freedesktop/secrets",
                SERVICE,
                "ReadAlias",
                &alias
            )?;
            if !no_path(&path) {
                return Ok(path);
            }
        }
        bail!(
            "keyring: the Secret Service has no default keyring (no \"default\" or \"login\" \
             alias); create one in your keyring app (Passwords and Keys, KWallet, KeePassXC) \
             and retry, or start Isocast with ISOCAST_SECRET_STORE=file to keep its secrets in \
             files"
        )
    }

    async fn create(
        &self,
        collection: &ObjectPath<'_>,
        service: &str,
        account: &str,
        value: &str,
    ) -> Result<()> {
        let label = format!("Isocast: {service} ({account})");
        let properties = HashMap::from([
            (
                "org.freedesktop.Secret.Item.Label",
                Value::from(label.as_str()),
            ),
            (
                "org.freedesktop.Secret.Item.Attributes",
                Value::from(attributes(service, account)),
            ),
        ]);
        let secret = (
            &self.session,
            Vec::<u8>::new(),
            value.as_bytes(),
            "text/plain",
        );
        let (_, prompt): (OwnedObjectPath, OwnedObjectPath) = call!(
            self,
            collection.as_str(),
            COLLECTION,
            "CreateItem",
            // true: replace an item with the same attributes.
            &(properties, secret, true)
        )?;
        ensure!(no_path(&prompt), confirm("write"));
        Ok(())
    }

    async fn store(&self, service: &str, account: &str, value: &str) -> Result<()> {
        let collection = self.collection().await?;
        let locked: OwnedValue = call!(
            self,
            collection.as_str(),
            "org.freedesktop.DBus.Properties",
            "Get",
            &(COLLECTION, "Locked")
        )?;
        ensure!(!bool::try_from(locked)?, LOCKED);
        self.create(&collection, service, account, value).await
    }

    async fn delete(&self, service: &str, account: &str) -> Result<bool> {
        let (unlocked, locked) = self.search(service, account).await?;
        ensure!(locked.is_empty(), LOCKED);
        for item in &unlocked {
            let prompt: OwnedObjectPath = call!(self, item.as_str(), ITEM, "Delete", &())?;
            ensure!(no_path(&prompt), confirm("delete"));
        }
        Ok(!unlocked.is_empty())
    }
}

fn attributes<'a>(service: &'a str, account: &'a str) -> HashMap<&'a str, &'a str> {
    HashMap::from([
        ("application", "isocast"),
        ("service", service),
        ("account", account),
    ])
}

/// Runs `f` against the desktop keyring, on its own thread and runtime since
/// callers may already be on one. None when there is no session bus or no
/// Secret Service on it, or with `ISOCAST_SECRET_STORE=file`; callers then
/// use the files. Any other failure is an error, never a file write.
fn with_keyring<T: Send>(f: impl AsyncFnOnce(&Keyring) -> Result<T> + Send) -> Result<Option<T>> {
    match std::env::var_os("ISOCAST_SECRET_STORE") {
        Some(store) if store == "file" => return Ok(None),
        Some(store) if !store.is_empty() => bail!(
            "secret store: ISOCAST_SECRET_STORE is {store:?}; set it to \"file\" to keep \
             secrets in files, or unset it to use the keyring"
        ),
        _ => {}
    }
    if timed_out_at().is_some_and(|at| at.elapsed() < KEYRING_BACKOFF) {
        bail!(TIMED_OUT);
    }
    let result = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("keyring: cannot start its D-Bus runtime")?;
                let result = runtime.block_on(async {
                    match Keyring::open().await? {
                        Some(keyring) => f(&keyring).await.map(Some),
                        None => Ok(None),
                    }
                });
                // Also cancels zbus's socket tasks.
                runtime.shutdown_timeout(Duration::from_millis(100));
                result
            })
            .join()
            .map_err(|_| anyhow::anyhow!("keyring: the D-Bus thread panicked"))?
    });
    match result {
        Err(error) if timed_out(&error) => {
            *timed_out_at() = Some(Instant::now());
            bail!(TIMED_OUT)
        }
        other => other,
    }
}

/// What the files hold for a secret.
enum Filed {
    Value(String),
    /// An empty file: the secret was deleted while the keyring was away, and
    /// the keyring's older item must stay hidden.
    Deleted,
    Nothing,
}

fn filed(service: &str, account: &str) -> Result<Filed> {
    Ok(match file_secret(service, account)? {
        None => Filed::Nothing,
        Some(value) if value.is_empty() => Filed::Deleted,
        Some(value) => Filed::Value(value),
    })
}

/// A file left from a time without the keyring moves into it once the
/// keyring answers with no prompt, and a pending delete finishes. Neither
/// ever fails the read.
pub fn secret(service: &str, account: &str) -> Result<Option<String>> {
    match filed(service, account)? {
        Filed::Value(value) => {
            let stored =
                with_keyring(async |keyring| keyring.store(service, account, &value).await);
            if let Ok(Some(())) = stored {
                let _ = file_delete(service, account);
            }
            Ok(Some(value))
        }
        Filed::Deleted => {
            let deleted = with_keyring(async |keyring| keyring.delete(service, account).await);
            if let Ok(Some(_)) = deleted {
                let _ = file_delete(service, account);
            }
            Ok(None)
        }
        Filed::Nothing => {
            Ok(with_keyring(async |keyring| keyring.read(service, account).await)?.flatten())
        }
    }
}

/// True for an item in a locked keyring too.
pub fn has_secret(service: &str, account: &str) -> Result<bool> {
    match filed(service, account)? {
        Filed::Value(_) => Ok(true),
        Filed::Deleted => Ok(false),
        Filed::Nothing => {
            let kept = with_keyring(async |keyring| {
                let (unlocked, locked) = keyring.search(service, account).await?;
                Ok(!unlocked.is_empty() || !locked.is_empty())
            })?;
            Ok(kept == Some(true))
        }
    }
}

pub fn set_secret(service: &str, account: &str, value: &str) -> Result<()> {
    ensure!(
        value.len() as u64 <= MAX_SECRET,
        "secret store: value for {service} ({account}) exceeds 64 KiB; store a smaller secret"
    );
    ensure!(
        !value.is_empty(),
        "secret store: value for {service} ({account}) is empty; delete the secret instead"
    );
    match with_keyring(async |keyring| keyring.store(service, account, value).await)? {
        // Reads take a file first, so an older one must go.
        Some(()) => file_delete(service, account).map(drop),
        None => file_set(service, account, value),
    }
}

pub fn delete_secret(service: &str, account: &str) -> Result<bool> {
    let in_file = matches!(filed(service, account)?, Filed::Value(_));
    match with_keyring(async |keyring| keyring.delete(service, account).await)? {
        Some(in_keyring) => {
            file_delete(service, account)?;
            Ok(in_keyring || in_file)
        }
        // The keyring may hold an older item, which must not come back.
        None => {
            file_set(service, account, "")?;
            Ok(in_file)
        }
    }
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
