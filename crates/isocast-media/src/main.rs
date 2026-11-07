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
fn startup() -> anyhow::Result<String> {
    use anyhow::{Context, ensure};
    use isocast_media_protocol::VERSION;
    let mut args = std::env::args_os().skip(1);
    let version = args.next().context("missing media protocol version")?;
    ensure!(
        version == VERSION,
        "expected {VERSION}, got {}",
        version.to_string_lossy()
    );
    let operation = args
        .next()
        .and_then(|s| s.into_string().ok())
        .context("missing or invalid media operation")?;
    ensure!(
        matches!(operation.as_str(), "check" | "info" | "video" | "audio"),
        "expected check, info, video or audio; got {operation}"
    );
    let parent: libc::pid_t = args
        .next()
        .and_then(|s| s.into_string().ok())
        .context("missing or invalid daemon PID")?
        .parse()
        .context("invalid daemon PID")?;
    ensure!(
        args.next().is_none(),
        "unexpected arguments after daemon PID"
    );
    // SAFETY: no pointers; install the signal *before* checking the PID
    // supplied by the spawner, not one sampled from a possibly orphaned child.
    unsafe {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        ensure!(
            parent > 0 && libc::getppid() == parent,
            "daemon exited before helper startup"
        );
        // No owned descriptors or native threads yet. records() must create
        // its private fd only after this sweep.
        if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) < 0 {
            // Older kernels and restricted containers: enumerate actual fds,
            // not RLIMIT_NOFILE (it can be lower than an inherited fd).
            let fds = std::fs::read_dir("/proc/self/fd")
                .context("listing inherited descriptors")?
                .map(|entry| {
                    Ok(entry?
                        .file_name()
                        .to_str()
                        .context("invalid fd name")?
                        .parse::<i32>()?)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            for fd in fds.into_iter().filter(|fd| *fd > 2) {
                // The directory iterator's own fd is already closed. Linux
                // releases a valid fd even when close reports an I/O error.
                libc::close(fd);
            }
        }
    }
    Ok(operation)
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    // Set up (including the fd sweep) before duplicating the record socket,
    // but report startup errors over it just like decode errors, before INIT.
    let operation = startup();
    let mut out = records()?;
    if let Err(e) = operation.and_then(|op| server::serve(&mut out, &op)) {
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
