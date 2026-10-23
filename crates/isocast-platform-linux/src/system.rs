//! Read-only Linux clock, procfs and recording-volume queries.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use anyhow::{Context, Result};
use isocast_engine::{Time, Volume};
use isocast_protocol::api::Machine;

pub fn now() -> Time {
    static ORIGIN: OnceLock<Instant> = OnceLock::new();
    Time::from_nanos(
        i64::try_from(ORIGIN.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(i64::MAX),
    )
}

pub fn machine() -> Machine {
    let memory = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("MemTotal:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            })
        });
    Machine {
        chip: std::fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|text| {
                text.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    (key.trim() == "model name").then(|| value.trim().to_owned())
                })
            })
            .unwrap_or_else(|| std::env::consts::ARCH.into()),
        memory_gb: memory.map_or(0.0, |kb| kb as f64 / (1024.0 * 1024.0)),
        cores: std::thread::available_parallelism().map_or(0, |cores| cores.get() as u32),
        os: std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .map(|release| format!("Linux {}", release.trim()))
            .unwrap_or_else(|_| "Linux".into()),
        hardware_encoders: Vec::new(),
    }
}

pub fn volume(path: &Path) -> Result<Volume> {
    let path = CString::new(path.as_os_str().as_bytes()).context("volume.path contains NUL")?;
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: path is NUL-terminated and info has space for one statfs.
    if unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()).context("checking recording volume");
    }
    // SAFETY: successful statfs initialized the structure.
    let info = unsafe { info.assume_init() };
    let (file_system, network) = file_system(info.f_type as u64);
    Ok(Volume {
        free_bytes: info.f_bavail.saturating_mul(info.f_bsize.max(0) as u64),
        file_system: file_system.into(),
        network,
    })
}

fn file_system(kind: u64) -> (&'static str, bool) {
    match kind {
        0xef53 => ("ext", false),
        0x9123683e => ("btrfs", false),
        0x58465342 => ("xfs", false),
        0x01021994 => ("tmpfs", false),
        0x6969 => ("nfs", true),
        0xff534d42 | 0xfe534d42 => ("cifs", true),
        0x794c7630 => ("overlayfs", false),
        0x2011bab0 => ("exfat", false),
        0x4d44 => ("vfat", false),
        0x5346544e => ("ntfs3", false),
        0xf2f52010 => ("f2fs", false),
        0x2fc12fc1 => ("zfs", false),
        // FUSE may be a remote transport. Conservatively apply network
        // volume rules to it, but don't label every unknown filesystem remote.
        0x65735546 => ("fuse", true),
        _ => ("unknown", false),
    }
}

pub fn same_volume(left: &Path, right: &Path) -> Result<bool> {
    Ok(std::fs::metadata(left)?.dev() == std::fs::metadata(right)?.dev())
}

fn process_ticks(text: &str) -> Option<u64> {
    // comm (field 2) may contain spaces and parentheses. Everything after
    // its LAST ')' has fixed columns, starting with state (field 3).
    let fields: Vec<_> = text.rsplit_once(')')?.1.split_whitespace().collect();
    fields
        .get(11)?
        .parse::<u64>()
        .ok()?
        .checked_add(fields.get(12)?.parse::<u64>().ok()?)
}

pub fn process_cpu_seconds() -> Option<f64> {
    let ticks = process_ticks(&std::fs::read_to_string("/proc/self/stat").ok()?)?;
    // SAFETY: sysconf is a read-only process constant query.
    let rate = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    (rate > 0).then(|| ticks as f64 / rate as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_comm_may_contain_parentheses_and_spaces() {
        assert_eq!(
            process_ticks("1 (a ) weird name) S 0 0 0 0 0 0 0 0 0 0 12 34 0"),
            Some(46)
        );
        assert_eq!(process_ticks("broken"), None);
    }

    #[test]
    fn native_queries_are_real() {
        assert!(process_cpu_seconds().is_some());
        assert!(machine().memory_gb > 0.0);
        assert!(volume(Path::new(".")).unwrap().free_bytes > 0);
        assert!(same_volume(Path::new("."), Path::new(".")).unwrap());
    }

    #[test]
    fn local_filesystems_are_not_reported_as_network_shares() {
        for (magic, name) in [
            (0x2011bab0, "exfat"),
            (0x4d44, "vfat"),
            (0x5346544e, "ntfs3"),
            (0xf2f52010, "f2fs"),
            (0x2fc12fc1, "zfs"),
            (0, "unknown"),
        ] {
            assert_eq!(file_system(magic), (name, false));
        }
        for (magic, name) in [
            (0x65735546, "fuse"),
            (0x6969, "nfs"),
            (0xff534d42, "cifs"),
            (0xfe534d42, "cifs"),
        ] {
            assert_eq!(file_system(magic), (name, true));
        }
    }
}
