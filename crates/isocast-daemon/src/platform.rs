//! Select the engine platform once, rather than at every daemon call site.

#[cfg(target_os = "macos")]
pub use isocast_platform_macos::MacPlatform as NativePlatform;
#[cfg(not(target_os = "macos"))]
pub use isocast_platform_stub::StubPlatform as NativePlatform;

pub type Frame = <NativePlatform as isocast_engine::Platform>::Frame;
