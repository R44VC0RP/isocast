use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ffi::CStr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    pub name: String,
    pub version: String,
}

pub fn c_string(ptr: *mut i8) -> String {
    if ptr.is_null() {
        return String::new();
    }

    unsafe { CStr::from_ptr(ptr.cast()) }
        .to_string_lossy()
        .into_owned()
}

unsafe extern "C" fn xaac_alloc(size: u32, alignment: u32) -> *mut std::ffi::c_void {
    let requested_align = alignment
        .max(libxaac_sys::DEFAULT_MEM_ALIGN_8)
        .next_power_of_two() as usize;
    let header_words = 2 * std::mem::size_of::<usize>();
    let total_size = size as usize + requested_align + header_words;
    let layout =
        Layout::from_size_align(total_size, std::mem::align_of::<usize>()).expect("valid layout");
    let base = unsafe { alloc_zeroed(layout) };
    if base.is_null() {
        return std::ptr::null_mut();
    }

    let aligned_addr =
        (base as usize + header_words + requested_align - 1) & !(requested_align - 1);
    let aligned = aligned_addr as *mut u8;
    let meta = unsafe { aligned.cast::<usize>().sub(2) };
    unsafe {
        meta.write(base as usize);
        meta.add(1).write(total_size);
    }
    aligned.cast()
}

unsafe extern "C" fn xaac_free(ptr: *mut std::ffi::c_void) {
    if ptr.is_null() {
        return;
    }

    let meta = unsafe { (ptr as *mut usize).sub(2) };
    let base = unsafe { meta.read() } as *mut u8;
    let total_size = unsafe { meta.add(1).read() };
    let layout =
        Layout::from_size_align(total_size, std::mem::align_of::<usize>()).expect("valid layout");
    unsafe {
        dealloc(base, layout);
    }
}

pub(crate) fn encoder_alloc() -> Option<unsafe extern "C" fn(u32, u32) -> *mut std::ffi::c_void> {
    Some(xaac_alloc)
}

pub(crate) fn encoder_free() -> Option<unsafe extern "C" fn(*mut std::ffi::c_void)> {
    Some(xaac_free)
}
