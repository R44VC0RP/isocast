//! Select the engine platform once, rather than at every daemon call site.

#[cfg(target_os = "linux")]
pub use isocast_platform_linux::LinuxPlatform as NativePlatform;
#[cfg(target_os = "macos")]
pub use isocast_platform_macos::MacPlatform as NativePlatform;
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub use isocast_platform_stub::StubPlatform as NativePlatform;
#[cfg(windows)]
pub use isocast_platform_windows::WindowsPlatform as NativePlatform;

pub type Frame = <NativePlatform as isocast_engine::Platform>::Frame;

/// Windows services share one lazily opened graphics device. The other
/// backends use stateless platform values.
#[cfg(windows)]
pub fn native() -> &'static NativePlatform {
    static PLATFORM: std::sync::OnceLock<NativePlatform> = std::sync::OnceLock::new();
    PLATFORM.get_or_init(NativePlatform::default)
}

#[cfg(not(windows))]
pub fn native() -> &'static NativePlatform {
    &NativePlatform
}
