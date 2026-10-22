//! Shared credential storage for the engine and clients. These are the same
//! native paths used for stream keys; neither media frameworks nor an engine
//! are needed merely to authenticate a remote CLI connection.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::{delete_secret, has_secret, secret, set_secret};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{delete as delete_secret, get as secret, has as has_secret, set as set_secret};

#[cfg(not(any(target_os = "macos", windows)))]
pub fn secret(_service: &str, _account: &str) -> anyhow::Result<Option<String>> {
    anyhow::bail!("the OS credential store is not implemented on this platform")
}
#[cfg(not(any(target_os = "macos", windows)))]
pub fn set_secret(_service: &str, _account: &str, _secret: &str) -> anyhow::Result<()> {
    anyhow::bail!("the OS credential store is not implemented on this platform")
}
#[cfg(not(any(target_os = "macos", windows)))]
pub fn has_secret(service: &str, account: &str) -> anyhow::Result<bool> {
    Ok(secret(service, account)?.is_some())
}
#[cfg(not(any(target_os = "macos", windows)))]
pub fn delete_secret(_service: &str, _account: &str) -> anyhow::Result<bool> {
    anyhow::bail!("the OS credential store is not implemented on this platform")
}
