//! What this Windows machine is, the volumes recordings go to, CPU time,
//! and scheduling for the engine's media threads.
//!
//! Every query here is read-only and cheap: none opens a graphics device or
//! an encoder, writes the registry, changes process-wide priority, or
//! creates a directory.

use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use isocast_engine::Volume;
use isocast_protocol::api::Machine;
use windows::Wdk::Storage::FileSystem::{
    FileFsDeviceInformation, FileFsFullSizeInformation, NtQueryVolumeInformationFile,
};
use windows::Wdk::System::SystemServices::{
    FILE_FS_DEVICE_INFORMATION, FILE_FS_FULL_SIZE_INFORMATION, FILE_REMOTE_DEVICE, RtlGetVersion,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, FILETIME, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, GetDiskFreeSpaceExW,
    GetFinalPathNameByHandleW, GetVolumeInformationByHandleW, VOLUME_NAME_GUID,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
use windows::Win32::System::Ioctl::{FILE_DEVICE_DFS, FILE_DEVICE_NETWORK_FILE_SYSTEM};
use windows::Win32::System::Registry::{HKEY, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::System::SystemInformation::{
    GlobalMemoryStatusEx, MEMORYSTATUSEX, OSVERSIONINFOEXW, OSVERSIONINFOW,
};
use windows::Win32::System::SystemServices::VER_NT_WORKSTATION;
use windows::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetCurrentProcess, GetCurrentThread,
    GetProcessTimes, GetThreadTimes,
};
use windows::core::{PCWSTR, w};

/// The longest path `GetFinalPathNameByHandleW` is asked for, in UTF-16
/// units: NT paths are limited to 32,767 characters.
const MAX_FINAL_PATH: usize = 32_768;
/// The largest registry string read, in bytes. CPU names and version
/// labels are far shorter; anything longer isn't what we asked for.
const MAX_REGISTRY_BYTES: u32 = 1024;

/// Creation time of a live process, in Windows FILETIME ticks. An absent or
/// exited process is `None`; access/query failures are errors, never proof of death.
pub fn process_started(pid: u32) -> Result<Option<u64>> {
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    // SAFETY: query/synchronize access only, no inherited handle or mutation.
    let process = match unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            false,
            pid,
        )
    } {
        Ok(process) => process,
        Err(error) if error.code() == ERROR_INVALID_PARAMETER.to_hresult() => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let result = (|| {
        // SAFETY: the handle stays open through both queries.
        match unsafe { WaitForSingleObject(process, 0) } {
            WAIT_OBJECT_0 => return Ok(None),
            WAIT_TIMEOUT => {}
            _ => bail!("could not check process {pid}"),
        }
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        // SAFETY: all four output structures are valid and writable.
        unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }?;
        Ok(Some(
            (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
        ))
    })();
    // SAFETY: this function owns the handle returned by OpenProcess.
    let _ = unsafe { CloseHandle(process) };
    result
}

/// The CPU, memory, logical cores and Windows version. Hardware encoders
/// stay empty: listing them means loading Media Foundation transforms or
/// GPU drivers, which this cheap startup query must not do.
pub(crate) fn machine() -> Machine {
    Machine {
        chip: cpu_name().unwrap_or_else(|| "unknown".into()),
        // 0 means unknown, as on the other platforms.
        memory_gb: memory_bytes().map_or(0.0, |bytes| bytes as f64 / (1u64 << 30) as f64),
        // SAFETY: plain query; 0 (failure) means unknown.
        cores: unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) },
        os: os_version().unwrap_or_else(|| "Windows (unknown version)".into()),
        hardware_encoders: Vec::new(),
    }
}

/// The processor's marketing name, from the read-only registry key the
/// kernel fills at boot from CPUID.
fn cpu_name() -> Option<String> {
    let name = registry_string(
        w!(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0"),
        w!("ProcessorNameString"),
    )?;
    // Some CPUs pad their brand string with spaces.
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn memory_bytes() -> Option<u64> {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: a correctly sized output structure with its length set.
    unsafe { GlobalMemoryStatusEx(&mut status) }.ok()?;
    (status.ullTotalPhys > 0).then_some(status.ullTotalPhys)
}

/// The real Windows version. `RtlGetVersion`, unlike `GetVersionEx`, isn't
/// shimmed by the application manifest. The registry's `ProductName` is
/// not used: it still says "Windows 10" on Windows 11.
fn os_version() -> Option<String> {
    let mut info = OSVERSIONINFOEXW {
        dwOSVersionInfoSize: size_of::<OSVERSIONINFOEXW>() as u32,
        ..Default::default()
    };
    // SAFETY: RtlGetVersion accepts the larger OSVERSIONINFOEXW when its
    // size field says so; the pointer is to a live, correctly sized struct.
    let status = unsafe { RtlGetVersion((&raw mut info).cast::<OSVERSIONINFOW>()) };
    if status.is_err() {
        return None;
    }
    let (major, minor, build) = (info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber);
    let workstation = u32::from(info.wProductType) == VER_NT_WORKSTATION;
    // Microsoft names client builds 22000 and later Windows 11. Server
    // builds share numbers with client ones (2025 and 11 24H2 are both
    // 26100), so a server is named only by its version number.
    let name = match (workstation, major, build) {
        (true, 10, 22000..) => "Windows 11",
        (true, 10, _) => "Windows 10",
        (true, _, _) => "Windows",
        (false, _, _) => "Windows Server",
    };
    // Like "24H2"; missing on early Windows 10 builds.
    let release = registry_string(
        w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
        w!("DisplayVersion"),
    )
    .map(|release| release.trim().to_owned())
    .filter(|release| !release.is_empty());
    Some(match release {
        Some(release) => format!("{name} {release} ({major}.{minor}.{build})"),
        None => format!("{name} ({major}.{minor}.{build})"),
    })
}

/// A `REG_SZ` value under `HKEY_LOCAL_MACHINE`, read without opening the
/// key for writing. `None` if missing, not a string, too long, or holding
/// a NUL inside it.
fn registry_string(subkey: PCWSTR, value: PCWSTR) -> Option<String> {
    registry_string_in(HKEY_LOCAL_MACHINE, subkey, value)
}

/// [`registry_string`] under `root` (`HKEY_CURRENT_USER` for this user's
/// settings).
pub(crate) fn registry_string_in(root: HKEY, subkey: PCWSTR, value: PCWSTR) -> Option<String> {
    let mut bytes = 0u32;
    // SAFETY: a size query (no buffer) into a live u32.
    let status = unsafe {
        RegGetValueW(
            root,
            subkey,
            value,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut bytes),
        )
    };
    if status != ERROR_SUCCESS || bytes == 0 || bytes > MAX_REGISTRY_BYTES {
        return None;
    }
    let mut buffer = vec![0u16; bytes.div_ceil(2) as usize];
    let mut written = (buffer.len() * 2) as u32;
    // SAFETY: `buffer` holds `written` bytes; RRF_RT_REG_SZ makes the call
    // NUL-terminate the string within that size or fail (a value that grew
    // between the calls fails with ERROR_MORE_DATA, and is skipped).
    let status = unsafe {
        RegGetValueW(
            root,
            subkey,
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut written),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let units = (written as usize / 2).min(buffer.len());
    let mut text = &buffer[..units];
    while let [rest @ .., 0] = text {
        text = rest;
    }
    if text.contains(&0) {
        return None;
    }
    String::from_utf16(text).ok()
}

/// Opens `path` (a file or a directory, following links: the volume asked
/// about is the one its data is on) only to read its attributes. Every
/// later query uses this one handle, so a path retargeted meanwhile can't
/// mix two volumes' answers. Paths with a NUL are refused by std.
fn open_for_query(path: &Path) -> Result<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        // Required to open a directory handle.
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))
}

fn raw(file: &File) -> HANDLE {
    HANDLE(file.as_raw_handle())
}

/// Free space for this user (after quotas), the file system's name, and
/// whether it is a network share, for the volume holding `path`.
pub(crate) fn volume(path: &Path) -> Result<Volume> {
    let file = open_for_query(path)?;
    let handle = raw(&file);

    let free_bytes = if let Some(guid) = volume_guid(&file) {
        // This root came from the open handle, not a drive letter or a path
        // that could have been retargeted. Ask for caller-available (quota)
        // bytes, not the volume's total free space.
        let root: Vec<u16> = guid.encode_utf16().chain([u16::from(b'\\'), 0]).collect();
        let mut free = 0;
        // SAFETY: NUL-terminated volume root and a live u64 output.
        unsafe { GetDiskFreeSpaceExW(PCWSTR(root.as_ptr()), Some(&mut free), None, None) }
            .with_context(|| format!("could not read free space for {}", path.display()))?;
        free
    } else {
        // Shares/virtual filesystems may have no local volume GUID. Retain
        // the handle-based quota query rather than guessing their root.
        let size: FILE_FS_FULL_SIZE_INFORMATION =
            query_volume(handle, FileFsFullSizeInformation)
                .with_context(|| format!("could not read free space for {}", path.display()))?;
        let unit = u64::from(size.SectorsPerAllocationUnit) * u64::from(size.BytesPerSector);
        let units = u64::try_from(size.CallerAvailableAllocationUnits).ok();
        let (Some(units), true) = (units, unit > 0) else {
            bail!(
                "{} reported an impossible free space ({} units of {unit} bytes)",
                path.display(),
                size.CallerAvailableAllocationUnits
            );
        };
        units
            .checked_mul(unit)
            .with_context(|| format!("free space for {} overflows", path.display()))?
    };

    let device: FILE_FS_DEVICE_INFORMATION = query_volume(handle, FileFsDeviceInformation)
        .with_context(|| format!("could not read the device of {}", path.display()))?;
    let network = device.Characteristics & FILE_REMOTE_DEVICE != 0
        || device.DeviceType == FILE_DEVICE_NETWORK_FILE_SYSTEM
        || device.DeviceType == FILE_DEVICE_DFS;

    let mut name = [0u16; 261];
    // SAFETY: live handle; the name buffer's length is passed with it.
    unsafe { GetVolumeInformationByHandleW(handle, None, None, None, None, Some(&mut name)) }
        .with_context(|| format!("could not read the file system of {}", path.display()))?;
    let end = name
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(name.len());
    let file_system = String::from_utf16_lossy(&name[..end]);

    Ok(Volume {
        free_bytes,
        file_system,
        network,
    })
}

/// One fixed-size `FILE_FS_*` record for the volume of an open handle.
fn query_volume<T: Default>(
    handle: HANDLE,
    class: windows::Wdk::Storage::FileSystem::FS_INFORMATION_CLASS,
) -> Result<T> {
    let mut info = T::default();
    let mut io = IO_STATUS_BLOCK::default();
    // SAFETY: `handle` is a live synchronous handle (std never opens with
    // FILE_FLAG_OVERLAPPED), so the call completes before returning and
    // `io` and `info`, on this stack frame, outlive it. `T` is the plain
    // `repr(C)` record for `class`, with its exact size passed.
    let status = unsafe {
        NtQueryVolumeInformationFile(
            handle,
            &mut io,
            (&raw mut info).cast(),
            size_of::<T>() as u32,
            class,
        )
    };
    if status.is_err() {
        return Err(windows::core::Error::from_hresult(status.to_hresult()).into());
    }
    Ok(info)
}

/// `\\?\Volume{…}` for the volume holding an open file, if it has one.
fn volume_guid(file: &File) -> Option<String> {
    let mut buffer = vec![0u16; 512];
    // A few tries: the file may be renamed to a longer path between calls.
    let mut tries = 0;
    loop {
        // SAFETY: live handle; the slice length is passed with the buffer.
        let needed =
            unsafe { GetFinalPathNameByHandleW(raw(file), &mut buffer, VOLUME_NAME_GUID) } as usize;
        if needed == 0 {
            return None;
        }
        if needed < buffer.len() {
            // Success: `needed` excludes the terminating NUL.
            buffer.truncate(needed);
            break;
        }
        // Too small: `needed` is the size required, NUL included.
        tries += 1;
        if tries == 3 || needed > MAX_FINAL_PATH {
            return None;
        }
        buffer = vec![0u16; needed];
    }
    if buffer.contains(&0) {
        return None;
    }
    let path = String::from_utf16(&buffer).ok()?;
    let rest = path.strip_prefix(r"\\?\")?;
    if !rest.get(..7)?.eq_ignore_ascii_case("Volume{") {
        return None;
    }
    let end = rest.find('}')?;
    Some(path[..4 + end + 1].to_owned())
}

fn filetime_ticks(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// User plus kernel CPU seconds used by this process.
pub(crate) fn process_cpu_seconds() -> Option<f64> {
    let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
    // SAFETY: the current-process pseudo-handle needs no closing; all four
    // outputs are live FILETIMEs.
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    }
    .ok()?;
    let ticks = filetime_ticks(kernel).checked_add(filetime_ticks(user))?;
    Some(ticks as f64 / 10_000_000.0)
}

/// User plus kernel CPU time used by the calling thread. Windows updates it
/// at scheduler clock ticks (about 15.6 ms by default), so it is accurate
/// only summed over many frames.
pub(crate) fn thread_cpu_time() -> Option<Duration> {
    let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
    // SAFETY: the current-thread pseudo-handle needs no closing; all four
    // outputs are live FILETIMEs.
    unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    }
    .ok()?;
    let ticks = filetime_ticks(kernel).checked_add(filetime_ticks(user))?;
    Some(Duration::from_nanos(ticks.checked_mul(100)?))
}

/// Video clocks, GPU completion and file writers use the same above-normal
/// policy as native capture/NVENC workers, not an MMCSS audio task.
pub(crate) fn real_time_thread() {
    if let Err(error) = crate::video_thread() {
        isocast_diagnostics::diagnostic!("warn"; "Windows video scheduling: {error:#}");
    }
}

/// The native helper owns an idempotent thread-local MMCSS Audio lease,
/// reverted on that same thread at exit. Never register shared executors.
pub(crate) fn audio_thread() {
    if let Err(error) = crate::audio_thread() {
        isocast_diagnostics::diagnostic!("warn"; "Windows audio scheduling: {error:#}");
    }
}
