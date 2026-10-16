//! The process's open-file limit (#92). macOS starts processes with a soft
//! limit of 256 descriptors; a busy isocastd (plugins, obs-websocket and
//! watch clients, recordings, logs) can reach it, and then VideoToolbox
//! fails to make an encoder with an error that reads like "no hardware
//! encoder is free". isocastd raises the soft limit at launch, and errors
//! that may come from running out say so.

use std::sync::OnceLock;

use serde_json::{Value, json};

/// macOS's `OPEN_MAX`: what `setrlimit` accepts for `RLIMIT_NOFILE` even
/// when the hard limit says unlimited.
pub const TARGET: u64 = 10240;
/// Descriptors a failing call may have needed (a VideoToolbox session
/// opens about four): this close to the limit counts as out, when the
/// call left no `EMFILE` behind.
const HEADROOM: usize = 8;

/// The soft limit before and after [`raise`], once it ran.
static RAISED: OnceLock<(u64, u64)> = OnceLock::new();

/// Raise this process's soft open-file limit toward its hard limit, at
/// most [`TARGET`]. Children (the native log relay, plugin processes)
/// inherit it. Returns the soft limit before and after; call once, early.
pub fn raise() -> (u64, u64) {
    *RAISED.get_or_init(|| {
        let Some((soft, hard)) = limits() else {
            return (0, 0);
        };
        let target = hard.min(TARGET);
        if soft >= target {
            return (soft, soft);
        }
        #[cfg(unix)]
        {
            let limit = libc::rlimit {
                rlim_cur: target,
                rlim_max: hard,
            };
            // SAFETY: setrlimit reads the struct we pass and nothing else.
            if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } == 0 {
                return (soft, target);
            }
        }
        (soft, soft)
    })
}

/// The soft and hard open-file limits now.
fn limits() -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit writes only the struct we pass.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
            return None;
        }
        Some((limit.rlim_cur, limit.rlim_max))
    }
    #[cfg(not(unix))]
    None
}

/// How many descriptors are open now; `Err` with the OS error when even
/// listing them failed (out of descriptors itself, often).
fn open_count() -> std::io::Result<usize> {
    // The listing's own descriptor is counted too; one more is close enough.
    Ok(std::fs::read_dir("/dev/fd")?.count())
}

/// Clear this thread's `errno`, so [`exhausted`] right after a call that
/// failed sees only what that call set.
pub fn clear_os_error() {
    #[cfg(target_os = "macos")]
    // SAFETY: __error returns this thread's errno slot.
    unsafe {
        *libc::__error() = 0;
    }
    #[cfg(target_os = "linux")]
    // SAFETY: __errno_location returns this thread's errno slot.
    unsafe {
        *libc::__errno_location() = 0;
    }
}

/// When this process is out of open files, or so close that a failure
/// just now likely came from it: an error saying so, with the numbers.
/// Call it right after the failing call (with [`clear_os_error`] before
/// it): a call that ran out leaves `EMFILE` in `errno`, even when the
/// descriptors are free again by now (a burst of plugin starts, say).
pub fn exhausted() -> Option<String> {
    let errno = std::io::Error::last_os_error().raw_os_error();
    let (soft, _) = limits()?;
    if errno.is_some_and(is_out_of_files) {
        return Some(message(None, soft));
    }
    let open = match open_count() {
        Ok(open) => open,
        Err(err) if matches!(err.raw_os_error(), Some(code) if is_out_of_files(code)) => {
            return Some(message(None, soft));
        }
        Err(_) => return None,
    };
    (open + HEADROOM >= soft as usize).then(|| message(Some(open), soft))
}

/// `EMFILE` (this process) or `ENFILE` (the whole system).
pub fn is_out_of_files(code: i32) -> bool {
    #[cfg(unix)]
    {
        code == libc::EMFILE || code == libc::ENFILE
    }
    #[cfg(not(unix))]
    {
        let _ = code;
        false
    }
}

fn message(open: Option<usize>, soft: u64) -> String {
    let how = match open {
        Some(open) => format!("has {open} of the {soft}"),
        None => format!("ran out of the {soft}"),
    };
    format!(
        "too many open files: isocastd {how} files and connections it may have open \
         (recordings, plugins, obs-websocket and other clients); disconnect some clients or \
         plugins, or restart isocastd"
    )
}

/// For `status`'s diagnostics: the limit, what it was raised from, and how
/// many are open.
pub fn health() -> Value {
    let (soft, hard) = limits().unwrap_or_default();
    json!({
        "open": open_count().ok(),
        "limit": soft,
        "hard_limit": hard,
        "raised_from": RAISED.get().filter(|(before, after)| before != after).map(|(before, _)| before),
    })
}
