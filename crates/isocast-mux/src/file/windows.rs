use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_INFORMATION_CLASS, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
    FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_RENAME_POSIX_SEMANTICS,
    FILE_RENAME_REPLACE_IF_EXISTS, FILE_SYNCHRONOUS_IO_NONALERT, FileRenameInformation,
    FileRenameInformationEx, NtCreateFile, NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, NTSTATUS, OBJ_CASE_INSENSITIVE,
    RtlNtStatusToDosError, STATUS_INVALID_INFO_CLASS, STATUS_INVALID_PARAMETER,
    STATUS_NOT_SUPPORTED, UNICODE_STRING,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FULL_DIR_INFO, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_ID_INFO, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_TYPE_DISK, FileBasicInfo, FileDispositionInfo, FileFullDirectoryInfo,
    FileFullDirectoryRestartInfo, FileIdInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetFileType, GetFinalPathNameByHandleW,
    SetFileInformationByHandle, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use super::{MetadataBlocked, OpenMode};

fn publication_error(error: io::Error) -> anyhow::Error {
    if isocast_protocol::fs::is_replace_busy(&error) {
        MetadataBlocked(error).into()
    } else {
        error.into()
    }
}

pub(super) fn open_directory(path: &Path) -> io::Result<File> {
    // Only this initial resolution may follow a symlink/junction. Permit a
    // directory rename: RootDirectory below still refers to the same object.
    let file = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recording folder is not a directory",
        ));
    }
    Ok(file)
}

pub(super) fn manifest_names(directory: &File, limit: usize) -> io::Result<Vec<OsString>> {
    const BUFFER_BYTES: usize = 64 * 1024;
    const NAME_OFFSET: usize = offset_of!(FILE_FULL_DIR_INFO, FileName);
    // FILE_FULL_DIR_INFO and each NextEntryOffset require 8-byte alignment.
    let mut buffer = vec![0u64; BUFFER_BYTES / size_of::<u64>()];
    let mut class = FileFullDirectoryRestartInfo;
    if limit > 1000 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "manifest name limit exceeds 1000",
        ));
    }
    let mut names = BTreeSet::new();
    loop {
        // The Win32 wrapper completes the query synchronously. There is no
        // stack IOSB or pending NT query retaining our buffer after return.
        // Zeroing also prevents a malformed short response reusing old names.
        buffer.fill(0);
        // SAFETY: the pinned directory and aligned, writable buffer remain
        // live for the whole call. Directory::manifest_names serializes its cursor.
        if unsafe {
            GetFileInformationByHandleEx(
                directory.as_raw_handle(),
                class,
                buffer.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                Ok(names.into_iter().rev().collect())
            } else {
                Err(error)
            };
        }
        class = FileFullDirectoryInfo;
        let mut offset: usize = 0;
        loop {
            if !offset.is_multiple_of(8) || offset > BUFFER_BYTES - NAME_OFFSET {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid directory record alignment or bounds",
                ));
            }
            // Read only the fixed header, not sizeof(FILE_FULL_DIR_INFO): its
            // trailing padding need not be present after the final short name.
            // SAFETY: the fixed header fits in the initialized allocation and
            // both u32 fields have their required alignment. No returned pointer
            // is dereferenced.
            let (length, next) = unsafe {
                let base = buffer.as_ptr().cast::<u8>().add(offset);
                (
                    base.add(offset_of!(FILE_FULL_DIR_INFO, FileNameLength))
                        .cast::<u32>()
                        .read() as usize,
                    base.add(offset_of!(FILE_FULL_DIR_INFO, NextEntryOffset))
                        .cast::<u32>()
                        .read() as usize,
                )
            };
            let available = BUFFER_BYTES - offset;
            if length == 0
                || !length.is_multiple_of(2)
                || length > available - NAME_OFFSET
                || (next != 0
                    && (!next.is_multiple_of(8)
                        || next < NAME_OFFSET + length
                        || next > available - NAME_OFFSET))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid directory name length or next-entry offset",
                ));
            }
            // SAFETY: the even byte length and trailing UTF-16 buffer fit
            // entirely within this record and the allocation, as checked above.
            let wide = unsafe {
                std::slice::from_raw_parts(
                    buffer
                        .as_ptr()
                        .cast::<u8>()
                        .add(offset + NAME_OFFSET)
                        .cast::<u16>(),
                    length / 2,
                )
            };
            if wide.contains(&0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "NUL in directory entry name",
                ));
            }
            // Windows names can contain unpaired surrogates. Preserve them;
            // callers apply the existing child-name policy before opening.
            let name = OsString::from_wide(wide);
            let text = name.to_string_lossy();
            if !text.starts_with('.') && text.to_ascii_lowercase().ends_with(".session.json") {
                names.insert(name);
                if names.len() > limit {
                    names.pop_first();
                }
            }
            if next == 0 {
                break;
            }
            offset += next;
        }
    }
}

pub(super) fn path(directory: &File) -> io::Result<PathBuf> {
    final_path(directory)
}

fn final_path(file: &File) -> io::Result<PathBuf> {
    let mut wide = vec![0u16; 512];
    // A rename can grow the path between calls. Bound both allocation and
    // retries rather than trusting a size obtained from a separate query.
    for _ in 0..4 {
        // SAFETY: file is live and wide has the specified writable capacity.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                wide.as_mut_ptr(),
                wide.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length < wide.len() {
            return Ok(PathBuf::from(OsString::from_wide(&wide[..length])));
        }
        if length >= 65_536 {
            break;
        }
        wide.resize(length + 1, 0);
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "normalized handle path is too long or keeps changing",
    ))
}

pub(super) fn replace_metadata(
    directory: &File,
    name: &OsStr,
    bytes: &[u8],
    durable: bool,
) -> Result<()> {
    let mut temporary = create_metadata_temp(directory, name)?;
    let publish = (|| -> Result<()> {
        temporary
            .write_all(bytes)
            .context("could not write complete metadata temporary")?;
        if durable {
            temporary
                .sync_all()
                .context("could not flush metadata temporary before publication")?;
        }
        let replace = match isocast_protocol::fs::retry_replace(|| {
            open_child(directory, name, OpenMode::Read)
        }) {
            Ok(existing) => {
                // Refuse existing reparse/hard links (open_child), and short-name
                // or case aliases here. Only compare this handle's normalized
                // basename; NEVER reopen or rename through the returned path.
                if final_path(&existing)?.file_name() != Some(name) {
                    bail!("metadata destination must use its exact long file name, not an alias");
                }
                // Rename cannot replace a destination held open without
                // delete sharing. Drop our inspection before the atomic rename.
                true
            }
            Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => false,
            Err(error) => {
                return Err(publication_error(error))
                    .context("metadata destination is not safe to replace");
            }
        };
        // This replaces the directory entry, never writes through the target.
        // A newly appearing target is refused if inspection found no target.
        // Other processes can still change an existing entry after inspection;
        // rename does not follow its final symlink or modify other hard links.
        isocast_protocol::fs::retry_replace(|| {
            rename_metadata(directory, &temporary, name, replace)
        })
        .map_err(publication_error)
        .context("could not publish metadata temporary")
    })();
    if let Err(error) = publish {
        // Only the owned create-new handle is eligible for cleanup. In
        // particular, do not remove the temp name via a pathname after closing.
        if let Err(cleanup) = delete_metadata_temp(&temporary) {
            return Err(error).context(format!(
                "also could not remove owned metadata temporary: {cleanup}"
            ));
        }
        return Err(error);
    }
    // Publication succeeded. No cleanup is allowed from this point onward,
    // even if the final flush fails: this handle is now the destination file.
    if durable {
        temporary
            .sync_all()
            .context("metadata was published, but its post-publication flush failed")?;
    }
    Ok(())
}

fn create_metadata_temp(directory: &File, destination: &OsStr) -> Result<File> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for _ in 0..64 {
        let serial = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        // Hidden-name convention plus .partial: a process-kill orphan is never
        // a .session.json recovery candidate. Creation is atomic, not a probe.
        let name = format!(
            ".isocast-{}-{stamp:x}-{serial:x}.partial",
            std::process::id()
        );
        if name
            .as_bytes()
            .eq_ignore_ascii_case(destination.as_encoded_bytes())
        {
            continue;
        }
        match open_child(directory, OsStr::new(&name), OpenMode::MetadataTemp) {
            Ok(file) => return Ok(file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("could not create metadata temporary"),
        }
    }
    bail!("could not create metadata temporary after 64 name collisions")
}

fn rename_metadata(directory: &File, file: &File, name: &OsStr, replace: bool) -> io::Result<()> {
    rename_metadata_with(directory, name, replace, |info, bytes, class| {
        let mut status_block = IO_STATUS_BLOCK::default();
        // Windows 10 rejected our non-NULL RootDirectory rename through
        // SetFileInformationByHandle(FileRenameInfo) with ERROR_INVALID_PARAMETER.
        // Both native classes preserve the pinned, handle-relative destination.
        // https://learn.microsoft.com/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information
        //
        // SAFETY: rename_metadata_with supplies an initialized, aligned buffer
        // that remains live throughout this call. This private temporary was
        // opened with FILE_SYNCHRONOUS_IO_NONALERT and SYNCHRONIZE (in
        // FILE_GENERIC_*). NtCreateFile's contract makes ALL subsequent I/O
        // synchronous: the I/O manager waits non-alertably for completion even
        // if a driver pends its IRP. No pending I/O retains the IOSB or buffer.
        // Do not pass an asynchronous/alertable handle or change those flags.
        // https://learn.microsoft.com/windows-hardware/drivers/ddi/ntifs/nf-ntifs-ntcreatefile
        unsafe {
            NtSetInformationFile(
                file.as_raw_handle(),
                &mut status_block,
                info.cast(),
                bytes,
                class,
            )
        }
    })
}

fn rename_metadata_with(
    directory: &File,
    name: &OsStr,
    replace: bool,
    mut set_info: impl FnMut(*const FILE_RENAME_INFORMATION, u32, FILE_INFORMATION_CLASS) -> NTSTATUS,
) -> io::Result<()> {
    let wide: Vec<u16> = name.encode_wide().collect();
    let name_bytes = wide
        .len()
        .checked_mul(2)
        .filter(|length| *length <= u16::MAX as usize)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "metadata name is too long"))?;
    let bytes = size_of::<FILE_RENAME_INFORMATION>() + name_bytes;
    let mut buffer = vec![0u64; bytes.div_ceil(size_of::<u64>())];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: the zeroed allocation has sufficient size and alignment for the
    // structure, its counted UTF-16 name, and a trailing NUL. Both handles are
    // live throughout the call; only one validated child is used.
    unsafe {
        (*info).Anonymous.Flags = if replace {
            FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS
        } else {
            0
        };
        (*info).RootDirectory = directory.as_raw_handle();
        (*info).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(
            wide.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            wide.len(),
        );
    }
    // POSIX replacement lets a delete-sharing reader keep the old file open
    // while new opens see the replacement. Ordinary readers still block it.
    let mut status = set_info(info, bytes as u32, FileRenameInformationEx);
    if matches!(
        status,
        STATUS_INVALID_INFO_CLASS | STATUS_INVALID_PARAMETER | STATUS_NOT_SUPPORTED
    ) {
        // Older kernels/filesystems may lack the extended class or flags.
        // Never fall back for access/sharing failures or other operational errors.
        // SAFETY: same live buffer; clear the entire flags union before setting
        // the legacy one-byte BOOLEAN (Rust bool), not a Win32 BOOL.
        unsafe {
            (*info).Anonymous.Flags = 0;
            (*info).Anonymous.ReplaceIfExists = replace;
        }
        status = set_info(info, bytes as u32, FileRenameInformation);
    }
    if status < 0 {
        // SAFETY: a pure conversion of the completed NT error status. For a
        // non-pending return, the API's status (not the IOSB) is authoritative.
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    Ok(())
}

fn delete_metadata_temp(file: &File) -> io::Result<()> {
    // Fail closed if validation itself fails. A leftover hidden .partial is
    // preferable to deleting an object we cannot prove is our regular file.
    validate_file(file)?;
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: this is our still-open create-new DELETE handle, with write and
    // delete sharing denied since creation. It has not been renamed/published.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn cancel_empty(
    directory: &File,
    file: File,
    name: &OsStr,
    expected_header: &[u8],
) -> Result<()> {
    if expected_header.is_empty() || expected_header.len() > 256 {
        bail!("empty media header must contain 1 to 256 bytes");
    }
    validate_file(&file).context("created media is not a single regular file")?;
    validate_empty_header(&file, name, expected_header)
        .context("created media no longer contains only its initial header")?;
    let identity = file_identity(&file).context("could not identify created media")?;
    // The live writer deliberately has no DELETE access and denies delete
    // sharing so ordinary ffprobe/Python readers work. Close it, but keep the
    // caller's same Directory lease pinned throughout this closing gap.
    drop(file);
    let file = open_child(directory, name, OpenMode::CancelEmpty)
        .context("could not reopen created media for deletion")?;
    // open_child rejects reparse points, directories and multiple links. This
    // new handle denies competing writers/deleters. Never follow the old file
    // by ID if it moved, or delete a replacement at the original child name.
    if file_identity(&file).context("could not identify reopened media")? != identity {
        bail!("media identity changed while reopening for cancellation");
    }
    validate_empty_header(&file, name, expected_header)
        .context("reopened media no longer contains only its initial header")?;
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: this live DELETE handle has been revalidated against the original
    // owned object and exact empty header. Only this handle is marked; no path
    // deletion, truncation, POSIX disposition or sharing bypass is used.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error()).context("could not mark empty media for deletion");
    }
    // File closes before returning to the caller that owns the folder lease.
    Ok(())
}

fn validate_empty_header(file: &File, name: &OsStr, expected: &[u8]) -> Result<()> {
    // A rename must not turn the original leaf into a short-name/case alias.
    // Inspect the handle's name only; never reopen using the returned path.
    if final_path(file)?.file_name() != Some(name) {
        bail!("media no longer has its original exact file name");
    }
    if file.metadata()?.len() != expected.len() as u64 {
        bail!("media length does not match its initial header");
    }
    let mut header = [0u8; 256];
    let mut read = 0;
    while read < expected.len() {
        match read_at(file, &mut header[read..expected.len()], read as u64) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof).into()),
            Ok(n) => read += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    if &header[..expected.len()] != expected || file.metadata()?.len() != expected.len() as u64 {
        bail!("media bytes or length do not match its initial header");
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    id: [u8; 16],
    created: i64,
}

fn file_identity(file: &File) -> io::Result<FileIdentity> {
    let mut id = FILE_ID_INFO::default();
    // SAFETY: file is live; id is an aligned, writable structure of this class.
    // Use the full 128-bit ID (including on ReFS), not a truncated file index.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut id as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: file is live; basic is the correctly sized output for this class.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&mut basic as *mut FILE_BASIC_INFO).cast(),
            size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // LastWriteTime/ChangeTime can change on closing the writer; neither is
    // identity. Unsupported identity queries fail closed, without a fallback.
    Ok(FileIdentity {
        volume: id.VolumeSerialNumber,
        id: id.FileId.Identifier,
        created: basic.CreationTime,
    })
}

pub(super) fn open_child(directory: &File, name: &OsStr, mode: OpenMode) -> io::Result<File> {
    let mut wide: Vec<u16> = name.encode_wide().collect();
    let length = wide
        .len()
        .checked_mul(2)
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "child name is too long"))?;
    let name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: wide.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: directory.as_raw_handle(),
        ObjectName: &name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: null(),
        SecurityQualityOfService: null(),
    };
    let (access, sharing, disposition) = match mode {
        // FILE_CREATE is NT's atomic create_new, not OPEN_IF or overwrite.
        OpenMode::Create => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ,
            FILE_CREATE,
        ),
        // Keep this very same owned handle through write, flush, rename and
        // cleanup. No other writer or deleter can swap the temporary entry.
        OpenMode::MetadataTemp => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
            FILE_SHARE_READ,
            FILE_CREATE,
        ),
        OpenMode::Repair => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ,
            FILE_OPEN,
        ),
        OpenMode::Read => (
            FILE_GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN,
        ),
        // Cancellation alone needs DELETE, after closing the original writer.
        // Readers denying delete sharing make this fail closed; do not weaken
        // the live Create/Repair access or sharing modes to avoid that refusal.
        OpenMode::CancelEmpty => (FILE_GENERIC_READ | DELETE, FILE_SHARE_READ, FILE_OPEN),
        // Several recordings need to open the same lock for LockFileEx's
        // shared lock. Deny delete sharing, so nobody can replace its name
        // and thereby create an independent folder-lock domain.
        OpenMode::Lock => (
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            FILE_OPEN_IF,
        ),
    };
    let mut handle = null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: all pointers outlive the synchronous call; the counted name
    // is one validated child (no slash, colon, root or device syntax), and
    // the RootDirectory handle remains open. No OBJ_INHERIT is specified.
    //
    // NtCreateFile is a synchronous create API: IRP_MJ_CREATE carries
    // IRP_SYNCHRONOUS_API, even if a lower driver pends its IRP. The I/O
    // manager completes the create before returning to user mode. In
    // addition FILE_SYNCHRONOUS_IO_NONALERT (with SYNCHRONIZE included in
    // FILE_GENERIC_*) makes every subsequent I/O synchronous/non-alertable.
    // There is therefore no outstanding operation retaining this stack
    // IOSB on return. Do not change these to overlapped/alertable options.
    // See Microsoft's NtCreateFile and IRP_MJ_CREATE API documentation.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access,
            &attributes,
            &mut status_block,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            sharing,
            disposition,
            FILE_OPEN_REPARSE_POINT | FILE_NON_DIRECTORY_FILE | FILE_SYNCHRONOUS_IO_NONALERT,
            null(),
            0,
        )
    };
    if status < 0 {
        // SAFETY: a pure conversion of the completed NT error status.
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32,
        ));
    }
    // SAFETY: a successful synchronous NtCreateFile returns a new owned
    // handle. File closes it on validation failure as well as on normal Drop.
    let file = unsafe { File::from_raw_handle(handle) };
    validate_file(&file)?;
    // For writable media, FILE_SHARE_READ is the lifetime ownership guard:
    // any competing write/delete open conflicts, including existing handles
    // and opens through hard-link/short-name aliases. Repair requests WRITE
    // with that same sharing policy, so cannot acquire a live writer's file.
    // Unlike LockFileEx on media bytes, it permits normal read-only probes
    // (the reader must share WRITE, as for any file being written on Windows).
    // The guard is released by closing this handle or process termination.
    Ok(file)
}

pub(super) fn validate_file(file: &File) -> io::Result<()> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: file owns a live handle and info is a writable output structure.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Metadata is queried on the opened object, never through its path.
    if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "reparse points (including cloud/sync placeholders) are not supported; choose a local, non-synced recording folder",
        ));
    }
    // FILE_NON_DIRECTORY_FILE alone also admits devices, hence FILE_TYPE_DISK.
    // SAFETY: file owns a live handle throughout the query.
    let kind = unsafe { GetFileType(file.as_raw_handle()) };
    if kind != FILE_TYPE_DISK
        || info.dwFileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_DEVICE) != 0
        || info.nNumberOfLinks != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "child must be a regular disk file with exactly one link and no reparse point",
        ));
    }
    Ok(())
}

pub(super) fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    file.seek_read(buf, offset)
}

pub(super) fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<usize> {
    file.seek_write(buf, offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file::{Directory, metadata_blocked};
    use std::io::Read;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{
        STATUS_ACCESS_DENIED, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
        STATUS_SHARING_VIOLATION,
    };

    #[test]
    fn metadata_rename_fallback() {
        let folder = std::env::temp_dir().join(format!(
            "isocast-metadata-fallback-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&folder).unwrap();
        let directory = open_directory(&folder).unwrap();
        let name = OsStr::new("take.session.json");
        let path = folder.join(name);
        for status in [
            STATUS_INVALID_INFO_CLASS,
            STATUS_INVALID_PARAMETER,
            STATUS_NOT_SUPPORTED,
            STATUS_ACCESS_DENIED,
            STATUS_SHARING_VIOLATION,
            STATUS_OBJECT_NAME_COLLISION,
            STATUS_OBJECT_NAME_NOT_FOUND,
        ] {
            std::fs::write(&path, b"old").unwrap();
            let mut temporary = create_metadata_temp(&directory, name).unwrap();
            temporary.write_all(b"new").unwrap();
            let mut calls = Vec::new();
            let result = rename_metadata_with(&directory, name, true, |info, bytes, class| {
                calls.push(class);
                if class == FileRenameInformationEx {
                    // Simulate an older kernel/filesystem at the native seam.
                    return status;
                }
                assert_eq!(class, FileRenameInformation);
                let mut status_block = IO_STATUS_BLOCK::default();
                // SAFETY: the helper owns the initialized buffer for this call;
                // the temporary is a live synchronous handle, as in production.
                unsafe {
                    NtSetInformationFile(
                        temporary.as_raw_handle(),
                        &mut status_block,
                        info.cast(),
                        bytes,
                        class,
                    )
                }
            });
            if [
                STATUS_INVALID_INFO_CLASS,
                STATUS_INVALID_PARAMETER,
                STATUS_NOT_SUPPORTED,
            ]
            .contains(&status)
            {
                result.unwrap();
                assert_eq!(calls, [FileRenameInformationEx, FileRenameInformation]);
                drop(temporary);
                assert_eq!(std::fs::read(&path).unwrap(), b"new");
            } else {
                let error = result.unwrap_err();
                // SAFETY: pure conversion of the injected NT error status.
                let code = unsafe { RtlNtStatusToDosError(status) } as i32;
                assert_eq!(error.raw_os_error(), Some(code));
                assert_eq!(calls, [FileRenameInformationEx]);
                assert_eq!(std::fs::read(&path).unwrap(), b"old");
                delete_metadata_temp(&temporary).unwrap();
            }
        }
        drop(directory);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn metadata_locked_reader() {
        let folder = std::env::temp_dir().join(format!(
            "isocast-metadata-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&folder).unwrap();
        let directory = Directory::for_recording(&folder).unwrap();
        let name = OsStr::new("take.session.json");
        let path = folder.join(name);
        let old = br#"{"status":"recording"}"#;
        let final_bytes = br#"{"status":"complete","frames":540}"#;
        directory.replace_metadata(name, old, true).unwrap();
        let mut shared_reader = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&path)
            .unwrap();
        directory.replace_metadata(name, final_bytes, true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), final_bytes);
        let mut held_bytes = Vec::new();
        shared_reader.read_to_end(&mut held_bytes).unwrap();
        assert_eq!(held_bytes, old, "the held handle still reads the old file");
        drop(shared_reader);
        directory.replace_metadata(name, old, true).unwrap();
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .unwrap();
        let error = directory
            .replace_metadata(name, final_bytes, true)
            .unwrap_err();
        eprintln!("held-reader finalization: {error:#}");
        assert!(metadata_blocked(&error));
        assert_eq!(std::fs::read(&path).unwrap(), old);
        assert!(!std::fs::read_dir(&folder).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "partial")
        }));
        // Real Windows contention clears inside the bounded retry window.
        let published = std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                drop(reader);
            });
            directory.replace_metadata(name, final_bytes, true)
        });
        published.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), final_bytes);
        // Unsafe destinations are not a transient publication failure.
        let alias = folder.join("alias");
        std::fs::hard_link(&path, &alias).unwrap();
        let error = directory
            .replace_metadata(name, b"unsafe", true)
            .unwrap_err();
        assert!(!metadata_blocked(&error));
        assert_eq!(std::fs::read(&alias).unwrap(), final_bytes);
        // Even a raw sharing error after publication must not be replayed.
        let flush = anyhow::Error::from(io::Error::from_raw_os_error(5))
            .context("metadata was published, but its post-publication flush failed");
        assert!(!metadata_blocked(&flush));
        drop(directory);
        std::fs::remove_dir_all(folder).unwrap();
    }
}
