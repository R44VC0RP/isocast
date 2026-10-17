//! Bounded, redacted plugin output and a nonblocking callback log queue.
use serde_json::Value;
#[cfg(unix)]
use std::io::{Read, Write};
use std::sync::{
    OnceLock,
    atomic::{AtomicU64, Ordering},
    mpsc,
};

type Record = (&'static str, &'static str, String);
static QUEUE: OnceLock<mpsc::SyncSender<Record>> = OnceLock::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);
pub fn deferred(level: &'static str, component: &'static str, message: String) {
    let tx = QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Record>(64);
        let _ = std::thread::Builder::new()
            .name("callback diagnostics".into())
            .spawn(move || {
                for (level, component, message) in rx {
                    crate::emit(level, component, &message, Value::Null);
                }
            });
        tx
    });
    if tx.try_send((level, component, message)).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

#[cfg(unix)]
pub fn plugin_pipe(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    let mut fds = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    unsafe {
        libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let read = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let write = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    let path = path.to_owned();
    std::thread::Builder::new()
        .name("plugin diagnostics".into())
        .spawn(move || {
            let mut input = read;
            let mut buffer = [0; 4096];
            let mut line = Vec::new();
            let mut oversized = false;
            let save = |line: &[u8]| {
                let text =
                    crate::redact::Redactor::default().text(String::from_utf8_lossy(line).trim());
                let mut opts = std::fs::OpenOptions::new();
                opts.create(true).append(true);
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
                if path.metadata().is_ok_and(|m| m.len() >= 1 << 20) {
                    let _ = std::fs::rename(&path, path.with_extension("previous"));
                }
                if let Ok(mut file) = opts.open(&path) {
                    let _ = writeln!(file, "{text}");
                }
            };
            while let Ok(n) = input.read(&mut buffer) {
                if n == 0 {
                    break;
                }
                for b in &buffer[..n] {
                    if *b == b'\n' {
                        if oversized {
                            save(b"[plugin line omitted: exceeds 16 KiB]");
                        } else {
                            save(&line);
                        }
                        line.clear();
                        oversized = false;
                    } else if !oversized {
                        if line.len() >= 16384 {
                            line.clear();
                            oversized = true;
                        } else {
                            line.push(*b);
                        }
                    }
                }
            }
            if !line.is_empty() {
                save(&line);
            }
        })?;
    Ok(write)
}
