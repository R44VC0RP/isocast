//! Native stderr relay. It outlives the producer and drains the pipe on EOF,
//! so a fatal native exit cannot race an in-process reader. No raw log file.
#[cfg(unix)]
mod unix {
    use serde_json::{Value, json};
    use std::collections::VecDeque;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{net::UnixDatagram, process::CommandExt};
    use std::process::{Command, Stdio};
    use std::sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicU32, Ordering},
    };
    const PENDING: &[u8] = b"\x1eisocast-seed-pending\n";
    const READY: &[u8] = b"\x1eisocast-seed-ready\n";
    // Darwin's default maxdgram/SO_SNDBUF is only 2048 bytes, independent of
    // the receive budget. Leave room for the fragment header on both OSes.
    const SEED_FRAGMENT: usize = 1024;
    struct Seeds {
        socket: UnixDatagram,
        pending: VecDeque<Vec<u8>>,
        withheld: bool,
    }
    static SEED: OnceLock<Arc<(Mutex<Seeds>, Condvar)>> = OnceLock::new();
    static PID: AtomicU32 = AtomicU32::new(0);
    pub fn pid() -> Option<u32> {
        let p = PID.load(Ordering::Relaxed);
        (p != 0).then_some(p)
    }
    impl Seeds {
        fn flush(&mut self) {
            while let Some(message) = self.pending.front() {
                if self.socket.send(message).is_err() {
                    break;
                }
                self.pending.pop_front();
            }
            let withheld = !self.pending.is_empty();
            if withheld != self.withheld {
                // Ordered with the producer's text, and serialized with every
                // seed send/retry. A delimiter also handles a partial native
                // line preceding this control record. No secret crosses here.
                let marker = if withheld { PENDING } else { READY };
                let mut record = vec![b'\n'];
                record.extend_from_slice(marker);
                let _ = std::io::stderr().write_all(&record);
                self.withheld = withheld;
            }
        }
    }
    pub fn seed(secret: &str, pinned: bool) {
        if let Some(queue) = SEED.get() {
            let mut seeds = queue.0.lock().unwrap_or_else(|p| p.into_inner());
            // Fragment large explicit credentials into small datagrams; the
            // receiver registers them only after their final fragment arrives.
            let chunks = secret.as_bytes().chunks(SEED_FRAGMENT);
            let count = chunks.len();
            for (i, chunk) in chunks.enumerate() {
                let mut message = vec![(u8::from(pinned) << 1) | u8::from(i + 1 == count)];
                message.extend_from_slice(chunk);
                seeds.pending.push_back(message);
            }
            seeds.flush();
            queue.1.notify_one();
        }
    }
    pub fn start(folder: &std::path::Path, launch: &str, process: &str) -> std::io::Result<()> {
        let (tx, rx) = UnixDatagram::pair()?;
        let capacity: libc::c_int = 256 * 1024;
        if unsafe {
            libc::setsockopt(
                rx.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&raw const capacity).cast(),
                size_of_val(&capacity) as libc::socklen_t,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
        tx.set_nonblocking(true)?;
        let fd = rx.as_raw_fd();
        let context =
            json!({"folder":folder,"launch":launch,"process":process,"pid":std::process::id()});
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--native-log-relay")
            .env("ISOCAST_NATIVE_RELAY", context.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // Only async-signal-safe descriptor operations between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let input = child.stdin.take().unwrap();
        if unsafe { libc::dup2(input.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::last_os_error());
        }
        drop(input);
        drop(rx);
        PID.store(child.id(), Ordering::Relaxed);
        let queue = Arc::new((
            Mutex::new(Seeds {
                socket: tx,
                pending: VecDeque::new(),
                withheld: false,
            }),
            Condvar::new(),
        ));
        let _ = SEED.set(queue.clone());
        std::thread::Builder::new()
            .name("native credential retry".into())
            .spawn(move || {
                let mut seeds = queue.0.lock().unwrap_or_else(|p| p.into_inner());
                loop {
                    while seeds.pending.is_empty() {
                        seeds = queue.1.wait(seeds).unwrap_or_else(|p| p.into_inner());
                    }
                    // The relay's poll loop drains independently of native text.
                    // Never wait for it on the caller's control/capture thread.
                    seeds = queue
                        .1
                        .wait_timeout(seeds, std::time::Duration::from_millis(10))
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                    seeds.flush();
                }
            })?;
        for (secret, pinned) in crate::redact::known_secrets() {
            seed(&secret, pinned);
        }
        std::thread::spawn(move || {
            let _ = child.wait();
            PID.store(0, Ordering::Relaxed);
        });
        Ok(())
    }
    pub fn requested() -> bool {
        std::env::args().nth(1).as_deref() == Some("--native-log-relay")
    }
    pub fn run() -> std::io::Result<()> {
        let raw = std::env::var("ISOCAST_NATIVE_RELAY").map_err(std::io::Error::other)?;
        let context: Value = serde_json::from_str(&raw).map_err(std::io::Error::other)?;
        let folder = std::path::PathBuf::from(
            context["folder"]
                .as_str()
                .ok_or_else(|| std::io::Error::other("missing relay folder"))?,
        );
        let launch = context["launch"]
            .as_str()
            .ok_or_else(|| std::io::Error::other("missing launch"))?;
        if launch.contains(['/', '\\']) {
            return Err(std::io::Error::other("invalid launch"));
        }
        let rx = unsafe { UnixDatagram::from_raw_fd(3) };
        rx.set_nonblocking(true)?;
        let mut segment = 0;
        let mut bytes = 0;
        let mut file =
            crate::private_file(&folder.join(format!("{launch}-native-{segment:04}.jsonl")))?;
        let mut buffer = [0; 4096];
        let mut credential = Vec::new();
        let mut pending = Vec::new();
        let mut overlong = false;
        let mut closed = false;
        let mut write = |line: &[u8], credential: &mut Vec<u8>| -> std::io::Result<()> {
            drain_seeds(&rx, credential);
            if line == PENDING {
                closed = true;
                return Ok(());
            }
            if line == READY {
                closed = false;
                return Ok(());
            }
            if line == b"\n" {
                return Ok(());
            }
            let message = if closed {
                "[native text withheld: credential seed delivery pending]".into()
            } else {
                crate::redact::Redactor::default().text(String::from_utf8_lossy(line).trim())
            };
            let record = json!({"time":chrono::Utc::now().to_rfc3339(),"process":context["process"],"pid":context["pid"],"launch":launch,"thread":"native relay","component":"native","level":"info","message":message,"fields":{"relay_pid":std::process::id()}});
            let line = record.to_string() + "\n";
            if bytes + line.len() > 4 << 20 {
                let next = segment + 1;
                let opened =
                    crate::private_file(&folder.join(format!("{launch}-native-{next:04}.jsonl")))?;
                file = opened;
                segment = next;
                bytes = 0;
            }
            file.write_all(line.as_bytes())?;
            bytes += line.len();
            Ok(())
        };
        loop {
            let mut fds = [
                libc::pollfd {
                    fd: rx.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) } < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            // A seed-only wakeup must drain immediately, without waiting for a
            // native line. Use the same receiver before processing each line.
            drain_seeds(&rx, &mut credential);
            if fds[1].revents == 0 {
                continue;
            }
            let n =
                unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
            if n < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if n == 0 {
                break;
            }
            for byte in &buffer[..n as usize] {
                if *byte == b'\n' {
                    if overlong {
                        write(b"[native line omitted: exceeds 16 KiB]", &mut credential)?;
                    } else {
                        pending.push(*byte);
                        write(&pending, &mut credential)?;
                    }
                    pending.clear();
                    overlong = false;
                } else if !overlong {
                    if pending.len() >= 16384 {
                        pending.clear();
                        overlong = true;
                    } else {
                        pending.push(*byte);
                    }
                }
            }
        }
        if !pending.is_empty() {
            write(&pending, &mut credential)?;
        }
        if overlong {
            write(b"[native line omitted: exceeds 16 KiB]", &mut credential)?;
        }
        file.sync_all()
    }

    fn drain_seeds(rx: &UnixDatagram, credential: &mut Vec<u8>) {
        let mut packet = [0u8; SEED_FRAGMENT + 1];
        while let Ok(n) = rx.recv(&mut packet) {
            if n == 0 {
                continue;
            }
            credential.extend_from_slice(&packet[1..n]);
            if packet[0] & 1 != 0 {
                if let Ok(secret) = std::str::from_utf8(credential) {
                    if packet[0] & 2 != 0 {
                        crate::redact::remember_secret(secret);
                    } else {
                        crate::redact::learn_secret(secret);
                    }
                }
                credential.clear();
            }
        }
    }
}
#[cfg(unix)]
pub use unix::{pid, requested, run, seed, start};
#[cfg(not(unix))]
pub fn seed(_secret: &str, _pinned: bool) {}
#[cfg(not(unix))]
pub fn pid() -> Option<u32> {
    None
}
