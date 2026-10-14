//! Native stderr relay. It outlives the producer and drains the pipe on EOF,
//! so a fatal native exit cannot race an in-process reader. No raw log file.
#[cfg(unix)]
mod unix {
    use serde_json::{Value, json};
    use std::collections::VecDeque;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{net::UnixDatagram, process::CommandExt};
    use std::process::{Child, Command, Stdio};
    use std::sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicU32, AtomicU64, Ordering},
    };
    const PENDING: &[u8] = b"\x1eisocast-seed-pending\n";
    const READY: &[u8] = b"\x1eisocast-seed-ready\n";
    // Darwin's default maxdgram/SO_SNDBUF is only 2048 bytes, independent of
    // the receive budget. Leave room for the fragment header on both OSes.
    const SEED_FRAGMENT: usize = 1024;
    struct Seeds {
        socket: Option<UnixDatagram>,
        pending: VecDeque<Vec<u8>>,
        withheld: bool,
        dropped: u64,
    }
    static SEED: OnceLock<Arc<(Mutex<Seeds>, Condvar)>> = OnceLock::new();
    static PID: AtomicU32 = AtomicU32::new(0);
    static DEATHS: AtomicU64 = AtomicU64::new(0);
    static RESTARTS: AtomicU64 = AtomicU64::new(0);
    static LAST_WARNING: Mutex<Option<String>> = Mutex::new(None);
    static WARNING: OnceLock<Box<dyn Fn(String) + Send + Sync>> = OnceLock::new();
    pub fn pid() -> Option<u32> {
        let p = PID.load(Ordering::Relaxed);
        (p != 0).then_some(p)
    }
    pub fn health() -> Value {
        let (pending, dropped) = SEED.get().map_or((0, 0), |queue| {
            let seeds = queue.0.lock().unwrap_or_else(|p| p.into_inner());
            (
                seeds.pending.iter().map(Vec::len).sum::<usize>(),
                seeds.dropped,
            )
        });
        json!({"unexpected_exits":DEATHS.load(Ordering::Relaxed),
            "restarts":RESTARTS.load(Ordering::Relaxed),"pending_seed_bytes":pending,
            "dropped_seed_messages":dropped,
            "last_warning":LAST_WARNING.lock().unwrap_or_else(|p| p.into_inner()).clone()})
    }
    pub fn on_warning(notify: impl Fn(String) + Send + Sync + 'static) {
        let _ = WARNING.set(Box::new(notify));
        let previous = LAST_WARNING
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(message) = previous
            && let Some(notify) = WARNING.get()
        {
            notify(message);
        }
    }
    fn warning(message: String) {
        *LAST_WARNING.lock().unwrap_or_else(|p| p.into_inner()) = Some(message.clone());
        crate::emit("warn", "native", &message, Value::Null);
        if let Some(notify) = WARNING.get() {
            notify(message);
        }
    }
    impl Seeds {
        fn disconnected(&mut self) {
            self.socket = None;
            self.dropped += self.pending.len() as u64;
            self.pending.clear();
            self.pending.shrink_to_fit();
            self.withheld = true;
        }
        fn flush(&mut self) {
            let Some(socket) = &self.socket else { return };
            while let Some(message) = self.pending.front() {
                match socket.send(message) {
                    Ok(_) => {
                        self.pending.pop_front();
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.raw_os_error() == Some(libc::ENOBUFS) =>
                    {
                        break;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        // The registry remains authoritative. Never keep a
                        // retry backlog for a dead peer; its replacement is
                        // seeded afresh before it may disclose native text.
                        self.disconnected();
                        return;
                    }
                }
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
        fn enqueue(&mut self, secret: &str, owner: Option<(&str, &str)>) {
            if self.socket.is_none() {
                self.dropped += 1;
                return;
            }
            let payload = owner
                .map(|(service, account)| serde_json::to_vec(&(service, account, secret)).unwrap());
            let chunks = payload
                .as_deref()
                .unwrap_or(secret.as_bytes())
                .chunks(SEED_FRAGMENT);
            let count = chunks.len();
            for (i, chunk) in chunks.enumerate() {
                let mut message = vec![(u8::from(owner.is_some()) << 1) | u8::from(i + 1 == count)];
                message.extend_from_slice(chunk);
                self.pending.push_back(message);
            }
        }
    }
    pub fn seed(secret: &str, owner: Option<(&str, &str)>) {
        if let Some(queue) = SEED.get() {
            let mut seeds = queue.0.lock().unwrap_or_else(|p| p.into_inner());
            seeds.enqueue(secret, owner);
            seeds.flush();
            queue.1.notify_one();
        }
    }
    pub fn start(folder: &std::path::Path, launch: &str, process: &str) -> std::io::Result<()> {
        let queue = Arc::new((
            Mutex::new(Seeds {
                socket: None,
                pending: VecDeque::new(),
                withheld: true,
                dropped: 0,
            }),
            Condvar::new(),
        ));
        let _ = SEED.set(queue.clone());
        let child = spawn_relay(folder, launch, process, 0, &queue)?;
        let retry = queue.clone();
        std::thread::Builder::new()
            .name("native credential retry".into())
            .spawn(move || {
                let mut seeds = retry.0.lock().unwrap_or_else(|p| p.into_inner());
                loop {
                    while seeds.pending.is_empty() {
                        seeds = retry.1.wait(seeds).unwrap_or_else(|p| p.into_inner());
                    }
                    seeds = retry
                        .1
                        .wait_timeout(seeds, std::time::Duration::from_millis(10))
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                    seeds.flush();
                }
            })?;
        let (folder, launch, process) = (folder.to_owned(), launch.to_owned(), process.to_owned());
        std::thread::Builder::new().name("native relay supervisor".into()).spawn(move || {
            let mut child = child;
            let mut generation = 0;
            loop {
                let ended = child.wait();
                PID.store(0, Ordering::Relaxed);
                {
                    let mut seeds = queue.0.lock().unwrap_or_else(|p| p.into_inner());
                    seeds.disconnected();
                    // Avoid EPIPE in native producers while no reader exists;
                    // never redirect unsanitized text to a persistent log.
                    if let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
                        unsafe { libc::dup2(null.as_raw_fd(), libc::STDERR_FILENO); }
                    }
                }
                DEATHS.fetch_add(1, Ordering::Relaxed);
                let reason = ended.map_or_else(|e| e.to_string(), |s| s.to_string());
                warning(format!("native stderr relay exited unexpectedly ({reason}); restarting; native output during the gap is unavailable"));
                let mut previous_error = None;
                loop {
                    // Bound respawn storms (e.g. its log volume is unavailable).
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    generation += 1;
                    match spawn_relay(&folder, &launch, &process, generation, &queue) {
                        Ok(next) => {
                            child = next;
                            RESTARTS.fetch_add(1, Ordering::Relaxed);
                            warning("native stderr relay restarted; credential redaction restored before native logging resumes".into());
                            break;
                        }
                        Err(error) => {
                            let error = error.to_string();
                            if previous_error.as_ref() != Some(&error) {
                                warning(format!("native stderr relay unavailable: {error}; retrying"));
                                previous_error = Some(error);
                            }
                        }
                    }
                }
            }
        })?;
        Ok(())
    }
    fn spawn_relay(
        folder: &std::path::Path,
        launch: &str,
        process: &str,
        generation: u64,
        queue: &Arc<(Mutex<Seeds>, Condvar)>,
    ) -> std::io::Result<Child> {
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
        let context = json!({"folder":folder,"launch":launch,"process":process,"pid":std::process::id(),"generation":generation,"boot_pending":true});
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
        let mut seeds = queue.0.lock().unwrap_or_else(|p| p.into_inner());
        if unsafe { libc::dup2(input.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::last_os_error());
        }
        drop(input);
        drop(rx);
        PID.store(child.id(), Ordering::Relaxed);
        seeds.socket = Some(tx);
        seeds.withheld = true;
        for (secret, owner) in crate::redact::known_secrets() {
            seeds.enqueue(
                &secret,
                owner.as_ref().map(|(s, a)| (s.as_str(), a.as_str())),
            );
        }
        seeds.flush();
        queue.1.notify_one();
        Ok(child)
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
        let generation = context["generation"].as_u64().unwrap_or(0);
        let prefix = format!("{launch}-native-{generation:04}");
        let rx = unsafe { UnixDatagram::from_raw_fd(3) };
        rx.set_nonblocking(true)?;
        let mut segment = 0;
        let mut bytes = 0;
        let mut file = crate::private_file(&folder.join(format!("{prefix}-{segment:04}.jsonl")))?;
        let mut buffer = [0; 4096];
        let mut credential = Vec::new();
        let mut pending = Vec::new();
        let mut overlong = false;
        let mut closed = context["boot_pending"].as_bool().unwrap_or(false);
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
                    crate::private_file(&folder.join(format!("{prefix}-{next:04}.jsonl")))?;
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
                        if let Ok((service, account, key)) =
                            serde_json::from_str::<(String, String, String)>(secret)
                        {
                            crate::redact::remember_secret(&service, &account, &key);
                        }
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
pub use unix::{health, on_warning, pid, requested, run, seed, start};
#[cfg(not(unix))]
pub fn seed(_secret: &str, _owner: Option<(&str, &str)>) {}
#[cfg(not(unix))]
pub fn pid() -> Option<u32> {
    None
}
#[cfg(not(unix))]
pub fn health() -> serde_json::Value {
    serde_json::Value::Null
}
#[cfg(not(unix))]
pub fn on_warning(_notify: impl Fn(String) + Send + Sync + 'static) {}
