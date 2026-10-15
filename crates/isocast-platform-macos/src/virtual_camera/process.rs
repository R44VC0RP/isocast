//! All CMIO calls live in an authenticated child. Deadline expiry kills that
//! child, not an uninterruptible native thread in the recording daemon.
use super::*;
use anyhow::Context;
use isocast_surfaces::{Exporter, Surface, Token};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreateWithIOSurface, CVPixelBufferGetIOSurface,
};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const OPEN: Duration = Duration::from_secs(2);
const FRAME: Duration = Duration::from_millis(250);
const CLOSE: Duration = Duration::from_millis(500);
static UNREAPED: AtomicBool = AtomicBool::new(false);

struct NativeProcess {
    socket: UnixStream,
    child: Option<Child>,
    exporter: &'static Exporter,
    accepted_at: Instant,
}

fn timeout(socket: &UnixStream, duration: Duration) -> Result<()> {
    socket.set_read_timeout(Some(duration))?;
    socket.set_write_timeout(Some(duration))?;
    Ok(())
}

fn response(socket: &mut UnixStream, operation: &str) -> Result<bool> {
    let mut bytes = [0; 8];
    socket.read_exact(&mut bytes).with_context(|| format!("virtual_camera: native {operation} failed or exceeded its deadline; helper is terminated"))?;
    let status = i32::from_le_bytes(bytes[..4].try_into().unwrap());
    let stage = i32::from_le_bytes(bytes[4..].try_into().unwrap());
    match (status, stage) {
        (0, _) => Ok(true),
        (1, 7) => Ok(false),
        (-1, 1) => bail!(
            "virtual_camera: {NOT_INSTALLED}; run scripts/vcam-install.sh (a development engine never installs it)"
        ),
        (_, 2) => bail!(
            "virtual_camera: installed device has no unique Isocast Camera Sink output stream (CoreMediaIO {status})"
        ),
        (_, 3) => bail!(
            "virtual_camera.width/height: installed camera refused the requested BGRA format (CoreMediaIO {status}); choose an advertised output size"
        ),
        (_, 4) => bail!(
            "virtual_camera.fps: installed camera refused canvas.fps (CoreMediaIO {status}); supported range is 1–240"
        ),
        (_, 6) => bail!(
            "virtual_camera: installed camera refused its producer (CoreMediaIO {status}); check signing/team authorization or stop the other Isocast producer"
        ),
        (_, 9) => {
            bail!("virtual_camera: native helper rejected its peer audit token/code signature")
        }
        _ => bail!(
            "virtual_camera: native {operation} failed at stage {stage} (CoreMediaIO {status})"
        ),
    }
}

pub(super) fn open(width: u32, height: u32, fps: u32) -> Result<Box<dyn CameraSink<PixelBuffer>>> {
    if UNREAPED.load(Ordering::Acquire) {
        bail!(
            "virtual_camera: a killed native helper has not exited; no additional helper will be created"
        );
    }
    let exporter = Exporter::shared()?;
    let (socket, child_socket) = UnixStream::pair()?;
    timeout(&socket, OPEN)?;
    let fd = child_socket.as_raw_fd();
    let mut command = Command::new(std::env::current_exe()?);
    command.args([
        "--virtual-camera-sink-child",
        &fd.to_string(),
        &width.to_string(),
        &height.to_string(),
        &fps.to_string(),
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: only async-signal-safe fcntl in the pre-exec child. A private
    // socketpair is inherited; nothing is published in the filesystem.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .context("virtual_camera: could not launch native helper")?;
    drop(child_socket);
    let mut process = NativeProcess {
        socket,
        child: Some(child),
        exporter,
        accepted_at: Instant::now(),
    };
    let service = exporter.service().as_bytes();
    process
        .socket
        .write_all(&(service.len() as u32).to_le_bytes())?;
    process.socket.write_all(service)?;
    if let Err(error) = response(&mut process.socket, "open (2 s)") {
        process.terminate();
        return Err(error);
    }
    timeout(&process.socket, FRAME)?;
    Ok(Box::new(process))
}

impl NativeProcess {
    fn terminate(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        let _ = child.kill();
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // A kernel-delayed exit must not turn waitpid into an unbounded RPC.
        // At most one such child/reaper per daemon; new starts are refused.
        UNREAPED.store(true, Ordering::Release);
        std::thread::spawn(move || {
            let _ = child.wait();
            UNREAPED.store(false, Ordering::Release);
        });
    }
}

impl CameraSink<PixelBuffer> for NativeProcess {
    fn transport(&self) -> &'static str {
        #[cfg(feature = "vcam-diagnostics")]
        if std::env::var_os("ISOCAST_VCAM_CHILD_TEST").is_some() {
            return "diagnostic-native-child";
        }
        "cmio-iosurface"
    }
    fn close(&mut self) -> Result<()> {
        if self.child.is_none() {
            return Ok(());
        }
        timeout(&self.socket, CLOSE)?;
        let result = self
            .socket
            .write_all(&[0])
            .map_err(anyhow::Error::from)
            .and_then(|()| response(&mut self.socket, "close (500 ms)").map(|_| ()));
        self.terminate();
        result
    }
    fn send(&mut self, frame: &PixelBuffer, pts: Time) -> Result<bool> {
        let surface = CVPixelBufferGetIOSurface(Some(&frame.0))
            .context("virtual_camera: missing IOSurface")?;
        let token = self.exporter.offer(vec![Surface::from_io_surface(surface)]);
        let result = (|| {
            self.socket.write_all(&[1])?;
            self.socket.write_all(&token.0)?;
            self.socket.write_all(&pts.as_nanos().to_le_bytes())?;
            let accepted = response(&mut self.socket, "frame (250 ms)")?;
            if accepted {
                self.accepted_at = Instant::now();
            } else if self.accepted_at.elapsed() > Duration::from_secs(1) {
                bail!("virtual_camera: native sink queue was full for 1 second; helper terminated");
            }
            Ok(accepted)
        })();
        self.exporter.withdraw(token);
        if result.is_err() {
            self.terminate();
        }
        result
    }
}

impl Drop for NativeProcess {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.close();
            self.terminate();
        }
    }
}

fn reply(socket: &mut UnixStream, status: i32, stage: i32) -> Result<()> {
    socket.write_all(&status.to_le_bytes())?;
    socket.write_all(&stage.to_le_bytes())?;
    Ok(())
}

/// Private executable entry point. Reject any caller that is not the same
/// running signed Isocast executable, using kernel-supplied peer credentials.
pub fn run_child(args: &[String]) -> Result<()> {
    anyhow::ensure!(args.len() == 4, "invalid camera helper arguments");
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
    let epoch = Instant::now();
    let deadline = Arc::new(AtomicU64::new(2_000));
    let parent = unsafe { libc::getppid() };
    std::thread::spawn({
        let deadline = deadline.clone();
        move || loop {
            if unsafe { libc::getppid() } != parent
                || epoch.elapsed().as_millis() as u64 > deadline.load(Ordering::Acquire)
            {
                // No destructors: those may be inside the stalled native API.
                unsafe { libc::_exit(124) }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    let fd: i32 = args[0].parse()?;
    let mut socket_type: i32 = 0;
    let mut socket_type_size = std::mem::size_of::<i32>() as libc::socklen_t;
    anyhow::ensure!(
        fd >= 3
            && unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_TYPE,
                    (&mut socket_type as *mut i32).cast(),
                    &mut socket_type_size,
                )
            } == 0
            && socket_type == libc::SOCK_STREAM,
        "invalid helper socket"
    );
    // SAFETY: authenticated connected stream inherited explicitly above.
    let mut socket = unsafe { UnixStream::from_raw_fd(fd) };
    // Only the initial exec may inherit this channel.
    anyhow::ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
        "could not protect helper socket"
    );
    timeout(&socket, Duration::from_secs(5))?;
    if !unsafe { isocast_camera_auth_parent(fd) } {
        reply(&mut socket, -1, 9)?;
        bail!("camera helper peer authentication failed");
    }
    let width: u32 = args[1].parse()?;
    let height: u32 = args[2].parse()?;
    let fps: u32 = args[3].parse()?;
    anyhow::ensure!(
        isocast_protocol::virtual_camera::SIZES.contains(&(width, height))
            && (1..=240).contains(&fps),
        "invalid camera helper format"
    );
    let mut length = [0; 4];
    socket.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    anyhow::ensure!(length <= 256, "invalid surface service");
    let mut service = vec![0; length];
    socket.read_exact(&mut service)?;
    let service = std::str::from_utf8(&service)?;
    #[cfg(feature = "vcam-diagnostics")]
    let fault = std::env::var("ISOCAST_VCAM_CHILD_TEST").unwrap_or_default();
    #[cfg(not(feature = "vcam-diagnostics"))]
    let fault = String::new();
    anyhow::ensure!(
        ["", "pass", "open", "send", "close"].contains(&fault.as_str()),
        "invalid diagnostic child fault"
    );
    let stall = |operation: &str| {
        if fault == operation {
            unsafe {
                libc::raise(libc::SIGSTOP);
            }
        }
    };
    stall("open");
    let mut error = 0;
    let mut stage = 0;
    let handle = if fault.is_empty() {
        unsafe { isocast_camera_open(width, height, fps, &mut error, &mut stage) }
    } else {
        std::ptr::null_mut()
    };
    if fault.is_empty() && handle.is_null() {
        reply(&mut socket, error, stage)?;
        return Ok(());
    }
    let mut camera = (!handle.is_null()).then(|| Camera(handle));
    reply(&mut socket, 0, 0)?;
    loop {
        deadline.store(
            epoch.elapsed().as_millis() as u64 + 6_000,
            Ordering::Release,
        );
        let mut kind = [0];
        match socket.read_exact(&mut kind) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => break,
        }
        deadline.store(epoch.elapsed().as_millis() as u64 + 500, Ordering::Release);
        if kind == [0] {
            deadline.store(epoch.elapsed().as_millis() as u64 + 500, Ordering::Release);
            stall("close");
            drop(camera.take());
            reply(&mut socket, 0, 8)?;
            return Ok(());
        }
        anyhow::ensure!(kind == [1], "invalid camera helper operation");
        deadline.store(epoch.elapsed().as_millis() as u64 + 500, Ordering::Release);
        let mut token = [0; 16];
        let mut pts = [0; 8];
        socket.read_exact(&mut token)?;
        socket.read_exact(&mut pts)?;
        let surfaces = isocast_surfaces::fetch(service, Token(token), Duration::from_secs(1))?;
        anyhow::ensure!(
            surfaces.len() == 1 && surfaces[0].width() == width && surfaces[0].height() == height,
            "invalid camera surface"
        );
        let mut pixels = std::ptr::null_mut();
        let code = unsafe {
            CVPixelBufferCreateWithIOSurface(
                None,
                surfaces[0].io_surface(),
                None,
                NonNull::from(&mut pixels),
            )
        };
        let pixels = NonNull::new(pixels)
            .filter(|_| code == 0)
            .context("could not wrap camera IOSurface")?;
        let frame = PixelBuffer(unsafe { CFRetained::<CVPixelBuffer>::from_raw(pixels) });
        stall("send");
        let code = if let Some(camera) = &camera {
            unsafe {
                isocast_camera_send(
                    camera.0,
                    (&*frame.0 as *const CVPixelBuffer).cast(),
                    i64::from_le_bytes(pts),
                )
            }
        } else {
            0
        };
        // CMIO retains an accepted sample. Mach/CoreVideo use counts protect
        // its surface after this local wrapper and the parent's offer go away.
        drop(frame);
        drop(surfaces);
        reply(&mut socket, code, 7)?;
        if code != 0 && code != 1 {
            break;
        }
    }
    deadline.store(epoch.elapsed().as_millis() as u64 + 500, Ordering::Release);
    drop(camera);
    Ok(())
}
