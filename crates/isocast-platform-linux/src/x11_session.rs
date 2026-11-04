//! Shared by the recorder and the UI before it initializes winit. systemd's
//! user environment can outlive the compositor that set WAYLAND_DISPLAY.
pub(crate) fn x11_session() -> bool {
    use std::{env, os::unix::net::UnixStream, path::PathBuf};
    if env::var_os("DISPLAY").is_none_or(|d| d.is_empty())
        || env::var_os("WAYLAND_SOCKET").is_some()
    {
        return false;
    }
    let name = PathBuf::from(env::var_os("WAYLAND_DISPLAY").unwrap_or_else(|| "wayland-0".into()));
    let path = if name.is_absolute() {
        name
    } else if let Some(dir) = env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join(name)
    } else {
        return true;
    };
    // Unknown (e.g. socket exists but access is denied) is not evidence
    // that this is X11. Never substitute an Xwayland desktop in that case.
    UnixStream::connect(path).is_err_and(|e| {
        matches!(
            e.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
        )
    })
}
