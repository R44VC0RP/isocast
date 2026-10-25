//! Atomic file publication that survives brief Windows sharing conflicts.
//!
//! On Windows, replacing a file by rename fails with access denied (5),
//! sharing violation (32) or lock violation (33) while another process
//! (an antivirus scanner, the search indexer, or a reader without
//! `FILE_SHARE_DELETE`) holds the destination open. The muxer's native POSIX
//! rename permits delete-sharing readers on supported Windows/filesystems;
//! legacy rename can still be blocked by those readers.
//! These holds are usually a few milliseconds, so the replacement is tried
//! again a bounded number of times. Nothing is deleted first: the
//! destination is always either the old file or the new one, and the last
//! error is returned unchanged when the budget runs out. Other platforms
//! try once.

use std::io;
use std::path::Path;
use std::time::Duration;

/// Waits between attempts on Windows: six attempts over 310 ms at most.
pub const REPLACE_BACKOFF: [Duration; 5] = [
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(40),
    Duration::from_millis(80),
    Duration::from_millis(160),
];

/// Run `replace` (an atomic publication, such as a rename over the
/// destination) again while it fails because the destination is briefly
/// held open on Windows. Returns its first success or its last error.
/// Other errors, and every error on other platforms, return at once.
pub fn retry_replace<T>(replace: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    retry_with(
        replace,
        is_replace_busy,
        &REPLACE_BACKOFF,
        std::thread::sleep,
    )
}

/// Atomically replace `to` with `from` by rename, retried as
/// [`retry_replace`] describes.
pub fn replace(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    retry_replace(|| std::fs::rename(from, to))
}

/// Whether `error` is a Windows sharing conflict that a later replacement
/// can get past (access denied, sharing or lock violation). Always false on
/// other platforms, where those numbers mean other errors.
pub fn is_replace_busy(error: &io::Error) -> bool {
    cfg!(windows) && error.raw_os_error().is_some_and(windows_busy_code)
}

fn windows_busy_code(code: i32) -> bool {
    // ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION.
    matches!(code, 5 | 32 | 33)
}

fn retry_with<T>(
    mut replace: impl FnMut() -> io::Result<T>,
    busy: impl Fn(&io::Error) -> bool,
    backoff: &[Duration],
    mut sleep: impl FnMut(Duration),
) -> io::Result<T> {
    let mut waits = backoff.iter();
    loop {
        match replace() {
            Ok(value) => return Ok(value),
            Err(error) => match waits.next() {
                Some(wait) if busy(&error) => sleep(*wait),
                _ => return Err(error),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy_code(error: &io::Error) -> bool {
        error.raw_os_error().is_some_and(windows_busy_code)
    }

    #[test]
    fn windows_busy_codes_are_exactly_access_sharing_and_lock() {
        for code in [5, 32, 33] {
            assert!(windows_busy_code(code), "{code}");
        }
        for code in [0, 2, 3, 4, 6, 17, 31, 34, 80, 183] {
            assert!(!windows_busy_code(code), "{code}");
        }
    }

    #[test]
    fn classifier_matches_the_platform() {
        let denied = io::Error::from_raw_os_error(5);
        let sharing = io::Error::from_raw_os_error(32);
        let lock = io::Error::from_raw_os_error(33);
        let missing = io::Error::from_raw_os_error(2);
        let other = io::Error::other("not an OS error");
        for error in [&denied, &sharing, &lock] {
            assert_eq!(is_replace_busy(error), cfg!(windows), "{error}");
        }
        assert!(!is_replace_busy(&missing));
        assert!(!is_replace_busy(&other));
    }

    #[test]
    fn a_non_retryable_error_is_tried_exactly_once() {
        let mut calls = 0;
        let mut slept = Vec::new();
        let result: io::Result<()> = retry_with(
            || {
                calls += 1;
                Err(io::Error::from_raw_os_error(2))
            },
            busy_code,
            &REPLACE_BACKOFF,
            |wait| slept.push(wait),
        );
        assert_eq!(result.unwrap_err().raw_os_error(), Some(2));
        assert_eq!(calls, 1);
        assert!(slept.is_empty());
    }

    #[test]
    fn a_transient_conflict_succeeds_once_and_stops() {
        let mut calls = 0;
        let mut slept = Vec::new();
        let result = retry_with(
            || {
                calls += 1;
                if calls < 3 {
                    Err(io::Error::from_raw_os_error(32))
                } else {
                    Ok(calls)
                }
            },
            busy_code,
            &REPLACE_BACKOFF,
            |wait| slept.push(wait),
        );
        assert_eq!(result.unwrap(), 3);
        assert_eq!(calls, 3, "no attempt after the success");
        assert_eq!(slept, REPLACE_BACKOFF[..2]);
    }

    #[test]
    fn a_lasting_conflict_returns_the_last_error_within_the_budget() {
        let mut calls = 0;
        let mut slept = Vec::new();
        let result: io::Result<()> = retry_with(
            || {
                calls += 1;
                Err(io::Error::from_raw_os_error(if calls % 2 == 0 {
                    5
                } else {
                    33
                }))
            },
            busy_code,
            &REPLACE_BACKOFF,
            |wait| slept.push(wait),
        );
        assert_eq!(calls, REPLACE_BACKOFF.len() + 1);
        assert_eq!(
            result.unwrap_err().raw_os_error(),
            Some(5),
            "the last error"
        );
        assert_eq!(slept, REPLACE_BACKOFF);
        assert!(slept.iter().sum::<Duration>() <= Duration::from_millis(500));
    }

    #[test]
    fn replace_renames_over_the_destination() {
        let dir = std::env::temp_dir().join(format!(
            "isocast-fs-replace-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (from, to) = (dir.join("new.partial"), dir.join("file"));
        std::fs::write(&to, b"old").unwrap();
        std::fs::write(&from, b"new").unwrap();
        replace(&from, &to).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"new");
        assert!(!from.exists());
        let missing = replace(dir.join("missing"), &to).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
        assert_eq!(std::fs::read(&to).unwrap(), b"new", "untouched on error");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
