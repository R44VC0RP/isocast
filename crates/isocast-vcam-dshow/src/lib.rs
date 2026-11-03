//! Rust owns authenticated IPC and pixel buffers. The native boundary uses
//! the Windows SDK's DirectShow interfaces (no engine or capture SDK in host).
#[cfg(windows)]
mod receiver;
