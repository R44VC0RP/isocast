// Copyright The pipewire-rs Contributors.
// SPDX-License-Identifier: MIT

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
#[allow(unpredictable_function_pointer_comparisons)]
#[allow(unnecessary_transmutes)]
#[allow(clippy::all)]
mod bindings {
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
    pub(super) type StreamLoop = unsafe extern "C" fn(*mut pw_stream) -> *mut pw_loop;
    impl PwApi {
        pub(super) fn stream_loop(&self) -> Option<StreamLoop> {
            // SAFETY: signature from stream.h; `runtime()` pins this library.
            unsafe {
                self.__library
                    .get(b"pw_stream_get_data_loop\0")
                    .ok()
                    .map(|s| *s)
            }
        }
    }
}
#[allow(non_upper_case_globals, clippy::type_complexity)]
mod dispatch {
    use super::bindings::*;
    use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};
    use std::ffi::{c_char, CStr};
    use std::sync::OnceLock;

    type Version = (u32, u32, u32);
    type GetVersion = unsafe extern "C" fn() -> *const c_char;
    struct Runtime {
        version: Version,
        api: PwApi,
        stream_loop: Option<StreamLoop>,
    }

    impl Runtime {
        fn open() -> Result<Self, String> {
            let failure = |e| format!("PipeWire backend unavailable (libpipewire-0.3.so.0): {e}");
            // Check the one stable version entry point *before* looking up
            // newer symbols, so an old SONAME-compatible library reports its age.
            // SAFETY: pipewire/version.h declares this no-argument function
            // returning a static NUL-terminated string. lib is live throughout.
            let lib = unsafe { Library::open(Some("libpipewire-0.3.so.0"), RTLD_NOW | RTLD_LOCAL) }
                .map_err(failure)?;
            let get_version = unsafe {
                *lib.get::<GetVersion>(b"pw_get_library_version\0")
                    .map_err(failure)?
            };
            Self::from_version_fn(lib, get_version)
        }

        fn from_version_fn(lib: Library, get_version: GetVersion) -> Result<Self, String> {
            // SAFETY: open() resolved the version function from this handle.
            // Tests substitute a function with the same ABI and lifetime.
            let ptr = unsafe { get_version() };
            if ptr.is_null() {
                return Err("PipeWire backend unavailable: null library version".into());
            }
            let loaded = unsafe { CStr::from_ptr(ptr) }.to_string_lossy();
            let parse = || {
                let (major, tail) = loaded.split_once('.')?;
                let (minor, micro) = tail.split_once('.')?;
                Some((
                    major.parse().ok()?,
                    minor.parse().ok()?,
                    micro.parse().ok()?,
                ))
            };
            // pw_buffer.time was appended in 1.0.5. All PipeWire paths share
            // this ABI floor, including callers that don't read timestamps.
            let version: Version = parse().filter(|v| *v >= (1, 0, 5)).ok_or_else(|| {
                format!("PipeWire backend unavailable: loaded {loaded}, needs 1.0.5 (bookworm-backports) or newer")
            })?;
            // SAFETY: the version-checked system library implements the header
            // ABI. Bindgen resolves the table before it becomes visible to callers.
            let api = unsafe { PwApi::from_library(lib) }
                .map_err(|e| format!("PipeWire backend unavailable (libpipewire-0.3.so.0): {e}"))?;
            Ok(Self {
                version,
                stream_loop: api.stream_loop(),
                api,
            })
        }
    }

    fn runtime() -> Result<&'static Runtime, &'static str> {
        static API: OnceLock<Result<Runtime, String>> = OnceLock::new();
        API.get_or_init(Runtime::open)
            .as_ref()
            .map_err(String::as_str)
    }

    /// Require the supported runtime before entering infallible pipewire-rs APIs.
    /// Returns its cached version for version-dependent capture configuration.
    pub fn require_supported() -> Result<Version, &'static str> {
        runtime().map(|rt| rt.version)
    }

    #[cfg(test)]
    #[test]
    fn rejects_bookworm_before_resolving_capture_symbols() {
        unsafe extern "C" fn old_version() -> *const c_char {
            c"0.3.65".as_ptr()
        }
        let err = Runtime::from_version_fn(Library::this(), old_version)
            .err()
            .expect("old library accepted");
        assert!(
            err.contains("loaded 0.3.65, needs 1.0.5 (bookworm-backports)"),
            "{err}"
        );
    }

    pub struct Function<F: 'static>(fn(&'static PwApi) -> &'static F);
    impl<F> std::ops::Deref for Function<F> {
        type Target = F;
        #[inline]
        fn deref(&self) -> &F {
            (self.0)(
                &runtime()
                    .expect("check the PipeWire runtime before use")
                    .api,
            )
        }
    }
    include!(concat!(env!("OUT_DIR"), "/calls.rs"));

    /// Added in PipeWire 1.1. Look it up on the loaded LOCAL handle, not in
    /// RTLD_DEFAULT. Older libraries still use the context's single data loop.
    pub fn stream_data_loop_fn() -> Option<unsafe extern "C" fn(*mut pw_stream) -> *mut pw_loop> {
        runtime().ok()?.stream_loop
    }
}
pub use bindings::*;
pub use dispatch::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init() {
        unsafe {
            pw_init(std::ptr::null_mut(), std::ptr::null_mut());
            pw_deinit();
        }
    }
}
