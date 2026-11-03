//! Read the loader's path and DT_SONAME from the *mapped* object containing
//! a codec symbol. A symlink's name is not evidence of ABI compatibility;
//! reopening it would race package upgrades.

use std::ffi::{CStr, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::slice;

use libc::{PT_DYNAMIC, PT_LOAD, c_int, c_void, dl_phdr_info};

struct Lookup {
    symbol: usize,
    result: Option<(PathBuf, Option<String>)>,
}

pub(super) fn identify(symbol: usize) -> Option<(PathBuf, Option<String>)> {
    let mut lookup = Lookup {
        symbol,
        result: None,
    };
    // SAFETY: synchronous callback; lookup lives until the walk returns.
    unsafe { libc::dl_iterate_phdr(Some(visit), (&raw mut lookup).cast()) };
    lookup.result
}

unsafe extern "C" fn visit(info: *mut dl_phdr_info, _: usize, data: *mut c_void) -> c_int {
    // SAFETY: the loader supplies live native program headers for this callback;
    // data is identify's exclusive borrow. All address reads below are confined
    // to the object owning the symbol, held loaded by its OpenH264API.
    let (info, lookup) = unsafe { (&*info, &mut *data.cast::<Lookup>()) };
    if info.dlpi_phnum == 0 || info.dlpi_phdr.is_null() {
        return 0;
    }
    // SAFETY: the loader's non-null phdr array spans phnum initialized native headers for this visit.
    let headers = unsafe { slice::from_raw_parts(info.dlpi_phdr, info.dlpi_phnum.into()) };
    let base = info.dlpi_addr as usize;
    let mapped = |addr: usize, size: usize| {
        headers
            .iter()
            .filter(|h| h.p_type == PT_LOAD && h.p_flags & libc::PF_R != 0)
            .any(|h| {
                let start = base.wrapping_add(h.p_vaddr as usize);
                addr >= start
                    && addr
                        .checked_add(size)
                        .zip(start.checked_add(h.p_memsz as usize))
                        .is_some_and(|(end, limit)| end <= limit)
            })
    };
    if !mapped(lookup.symbol, 1) || info.dlpi_name.is_null() {
        return 0;
    }
    // SAFETY: dlpi_name is the loader's NUL-terminated path.
    let path = unsafe { CStr::from_ptr(info.dlpi_name) };
    let soname = (|| {
        let dynamic = headers.iter().find(|h| h.p_type == PT_DYNAMIC)?;
        let addr = base.checked_add(dynamic.p_vaddr as usize)?;
        let size = dynamic.p_memsz as usize;
        // ElfW(Dyn): one native tag followed by one native word. Never walk
        // beyond the segment even if a library has no DT_NULL terminator.
        if !mapped(addr, size) || !addr.is_multiple_of(align_of::<usize>()) {
            return None;
        }
        // SAFETY: the aligned range is inside one readable segment; only complete native entries are borrowed.
        let entries = unsafe {
            slice::from_raw_parts(addr as *const [usize; 2], size / size_of::<[usize; 2]>())
        };
        let value = |tag| {
            entries
                .iter()
                .take_while(|e| e[0] != 0)
                .find(|e| e[0] == tag)
                .map(|e| e[1])
        };
        let strings = value(5)?; // DT_STRTAB
        let size = value(10)?; // DT_STRSZ
        let offset = value(14)?; // DT_SONAME
        // glibc can relocate DT_STRTAB in place; musl leaves it base-relative.
        let strings = if mapped(strings, size) {
            strings
        } else {
            base.checked_add(strings)?
        };
        if !mapped(strings, size) || offset >= size {
            return None;
        }
        // SAFETY: offset is within the mapped string table and the slice ends at its checked boundary.
        let bytes =
            unsafe { slice::from_raw_parts((strings + offset) as *const u8, size - offset) };
        Some(
            CStr::from_bytes_until_nul(bytes)
                .ok()?
                .to_string_lossy()
                .into_owned(),
        )
    })();
    lookup.result = Some((OsStr::from_bytes(path.to_bytes()).into(), soname));
    1
}
