#[cfg(target_os = "linux")]
mod decoder;
#[cfg(target_os = "linux")]
mod server;

/// GStreamer and its plugins are allowed to print at any time. Only this
/// close-on-exec descriptor may reach the parent as protocol data.
#[cfg(target_os = "linux")]
fn records() -> std::io::Result<std::fs::File> {
    use std::os::fd::AsFd;
    let records = std::io::stdout().as_fd().try_clone_to_owned()?;
    // SAFETY: no threads or native libraries have been started yet. dup2
    // atomically redirects fd 1; the private duplicate is unaffected.
    if unsafe { libc::dup2(libc::STDERR_FILENO, libc::STDOUT_FILENO) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(records.into())
}

#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    let mut out = records()?;
    if let Err(e) = server::serve(&mut out) {
        let mut reason = format!("{e:#}");
        reason.truncate(reason.floor_char_boundary(isocast_media_protocol::MAX_TEXT));
        let _ = isocast_media_protocol::write_record(&mut out, b"FAIL", &[reason.as_bytes()]);
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("isocast-media requires Linux");
    std::process::exit(1);
}
