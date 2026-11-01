//! The helper on Linux: see main.rs.

use std::cell::{Cell, RefCell};
use std::io::{BufRead, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use gtk4::prelude::*;
use gtk4::{gdk, gio, glib};
use webkit6::prelude::*;

/// The script that routes a page's sound to `isocastAudio` (shared with
/// the macOS browser source).
const AUDIO_SCRIPT: &str = include_str!("../../isocast-platform-macos/src/browser/audio.js");
const AUDIO_HANDLER: &str = "isocastAudio";
/// How long the private display may take to start.
const DISPLAY_START: Duration = Duration::from_secs(10);
/// Pictures waiting for isocastd; more are dropped, so memory stays bounded.
const QUEUE: usize = 2;
/// While a page loads (no pictures yet), how often isocastd hears it's
/// still alive, so a slow page isn't taken for a hung one.
const LOADING_BEAT: Duration = Duration::from_secs(1);
/// A page still loading after this fails, and isocastd restarts it.
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);

struct Args {
    url: String,
    width: u32,
    height: u32,
    fps: u32,
    css: String,
    audio: bool,
}

/// The page and its CSS: `page url|file <hex> <hex css>`, the first line
/// isocastd writes to stdin (never arguments, which anyone can read).
fn parse_page(line: &str) -> Result<(String, String), String> {
    let bad = || {
        format!(
            "expected \"page url|file <hex> <hex>\" on stdin, got {:.40?}",
            line
        )
    };
    let unhex = |word: &str| -> Result<Vec<u8>, String> {
        if !word.len().is_multiple_of(2) {
            return Err(bad());
        }
        (0..word.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(word.get(i..i + 2).ok_or_else(bad)?, 16).map_err(|_| bad()))
            .collect()
    };
    let words: Vec<&str> = line.trim_end_matches(['\n', '\r']).split(' ').collect();
    let [page, kind, location, css] = words[..] else {
        return Err(bad());
    };
    if page != "page" {
        return Err(bad());
    }
    let location = unhex(location)?;
    let css = String::from_utf8(unhex(css)?).map_err(|_| bad())?;
    let url = match kind {
        "url" => String::from_utf8(location).map_err(|_| bad())?,
        "file" => {
            let path = PathBuf::from(
                <std::ffi::OsString as std::os::unix::ffi::OsStringExt>::from_vec(location),
            );
            gio::File::for_path(&path).uri().to_string()
        }
        _ => return Err(bad()),
    };
    Ok((url, css))
}

fn parse_args() -> Result<Args, String> {
    let mut first = String::new();
    std::io::stdin()
        .read_line(&mut first)
        .map_err(|e| format!("reading the page from stdin: {e}"))?;
    let (url, css) = parse_page(&first)?;
    let mut args = std::env::args().skip(1);
    let (mut width, mut height, mut fps, mut audio) = (0, 0, 0, false);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--width" => width = value("--width")?.parse().map_err(|_| "--width: a number")?,
            "--height" => {
                height = value("--height")?
                    .parse()
                    .map_err(|_| "--height: a number")?
            }
            "--fps" => fps = value("--fps")?.parse().map_err(|_| "--fps: a number")?,
            "--audio" => audio = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) || !(1..=240).contains(&fps) {
        return Err(format!("bad size or rate: {width}x{height} at {fps} fps"));
    }
    Ok(Args {
        url,
        width,
        height,
        fps,
        css,
        audio,
    })
}

/// Writes records to stdout from its own thread; the main loop never blocks
/// on isocastd.
struct Out {
    send: mpsc::SyncSender<Vec<u8>>,
    /// Picture buffers back from the writer, reused (no 4-byte-per-pixel
    /// allocation per frame).
    spare: mpsc::Receiver<Vec<u8>>,
    /// A `FAIL` record has been written out.
    failed: mpsc::Receiver<()>,
}

impl Out {
    /// Take over stdout for records: they go to a private close-on-exec
    /// copy of it, and fd 1 becomes /dev/null, so the processes WebKit and
    /// GTK start never hold isocastd's pipe open (it would outlive this
    /// helper) and nothing else can write into the record stream.
    fn start() -> Self {
        use std::os::fd::FromRawFd;
        let copy = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 3) };
        let records = if copy >= 0 {
            if let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
                unsafe { libc::dup2(std::os::fd::AsRawFd::as_raw_fd(&null), libc::STDOUT_FILENO) };
            }
            copy
        } else {
            // Can't copy it: records stay on fd 1.
            libc::STDOUT_FILENO
        };
        // Owned by the writer thread, which lives as long as the process.
        let mut stdout = unsafe { std::fs::File::from_raw_fd(records) };
        let (send, receive) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
        let (give_back, spare) = mpsc::sync_channel::<Vec<u8>>(QUEUE + 1);
        let (wrote_fail, failed) = mpsc::sync_channel::<()>(1);
        std::thread::Builder::new()
            .name("isocast-browser-out".into())
            .spawn(move || {
                for record in receive {
                    if stdout
                        .write_all(&record)
                        .and_then(|_| stdout.flush())
                        .is_err()
                    {
                        // isocastd is gone.
                        std::process::exit(0);
                    }
                    if record.starts_with(b"FRAM") {
                        let _ = give_back.try_send(record);
                    } else if record.starts_with(b"FAIL") {
                        let _ = wrote_fail.try_send(());
                    }
                }
            })
            .expect("the output thread starts");
        Self {
            send,
            spare,
            failed,
        }
    }

    /// A buffer for a picture record, reused when one is free.
    fn buffer(&self) -> Vec<u8> {
        self.spare.try_recv().unwrap_or_default()
    }

    fn record(tag: &[u8; 4], payload_len: usize) -> Vec<u8> {
        let mut record = Vec::with_capacity(8 + payload_len);
        record.extend_from_slice(tag);
        record.extend_from_slice(&(payload_len as u32).to_le_bytes());
        record
    }

    /// Queue a record; false if the queue is full (it is dropped).
    fn send(&self, record: Vec<u8>) -> bool {
        self.send.try_send(record).is_ok()
    }

    /// Queue a record, waiting for room (rare, small records).
    fn send_wait(&self, record: Vec<u8>) {
        let _ = self.send.send(record);
    }

    /// Send why the helper is about to exit, and wait (up to a second)
    /// until isocastd has it.
    fn fail(&self, reason: &str) {
        let reason = &reason.as_bytes()[..reason.len().min(4000)];
        let mut record = Self::record(b"FAIL", reason.len());
        record.extend_from_slice(reason);
        self.send_wait(record);
        let _ = self.failed.recv_timeout(Duration::from_secs(1));
    }
}

/// Ends the main loop with an exit status.
struct Exit {
    main: glib::MainLoop,
    code: Cell<i32>,
}

impl Exit {
    /// Tell isocastd why, then end with `code`.
    fn fail(&self, out: &Out, reason: &str, code: i32) {
        if self.code.get() == 0 {
            out.fail(reason);
            self.code.set(code);
        }
        self.main.quit();
    }
}

/// A private Broadway display for this page; stopped with the helper.
struct Display {
    child: Child,
    /// GTK's socket for the display, in the runtime folder.
    socket: PathBuf,
    _folder: TempFolder,
}

impl Drop for Display {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

const FOLDER_PREFIX: &str = "isocast-browser-";

/// Where helpers keep their private folders: the user's runtime folder
/// when it is usable (private to them), else the temporary folder.
fn private_base() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| usable_folder(path))
        .unwrap_or_else(std::env::temp_dir);
    std::fs::canonicalize(&base).unwrap_or(base)
}

/// Remove what killed helpers couldn't clean up: their private folders
/// (this user's, named for a pid no helper runs as) and, in the runtime
/// folder, GTK display sockets nothing answers on.
fn sweep(base: &std::path::Path) {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    let me = unsafe { libc::getuid() };
    for entry in entries.flatten() {
        let name = entry.file_name();
        // A GTK display socket nothing answers on (its helper was killed).
        let display_socket = name.to_str().is_some_and(|n| {
            n.strip_prefix("broadway")
                .and_then(|n| n.strip_suffix(".socket"))
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        });
        if display_socket {
            if stale_socket(&entry.path()) {
                let _ = std::fs::remove_file(entry.path());
            }
            continue;
        }
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(FOLDER_PREFIX))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let alive = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .is_ok_and(|comm| comm.trim() == "isocast-browser");
        let ours = entry
            .path()
            .symlink_metadata()
            .is_ok_and(|m| m.is_dir() && m.uid() == me);
        if ours && !alive {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Create this helper's private folder (0700) in `base`. One already
/// there is replaced only if it is this user's own folder: a symlink or
/// someone else's (planted in a shared /tmp) is refused.
fn private_folder(base: &std::path::Path, pid: u32) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let folder = base.join(format!("{FOLDER_PREFIX}{pid}"));
    let create = || std::fs::DirBuilder::new().mode(0o700).create(&folder);
    match create() {
        Ok(()) => Ok(folder),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let ours = folder
                .symlink_metadata()
                .is_ok_and(|m| m.is_dir() && m.uid() == unsafe { libc::getuid() });
            if !ours {
                return Err(format!(
                    "{} exists and isn't this user's folder; refusing to use it",
                    folder.display()
                ));
            }
            std::fs::remove_dir_all(&folder)
                .and_then(|_| create())
                .map_err(|e| format!("{}: {e}", folder.display()))?;
            Ok(folder)
        }
        Err(err) => Err(format!("{}: {err}", folder.display())),
    }
}

/// Whether process `pid` holds the listening Unix socket bound at `path`
/// (its inode, from /proc/net/unix, among the process's open files).
fn listens_on(pid: u32, path: &std::path::Path) -> bool {
    let Ok(table) = std::fs::read_to_string("/proc/net/unix") else {
        return false;
    };
    let path = path.to_string_lossy();
    // Num RefCount Protocol Flags Type St Inode [Path]
    let inodes: Vec<String> = table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            (fields.len() >= 8 && fields[7] == path).then(|| fields[6].to_string())
        })
        .collect();
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    fds.flatten().any(|fd| {
        std::fs::read_link(fd.path()).is_ok_and(|target| {
            let target = target.to_string_lossy();
            inodes
                .iter()
                .any(|inode| target == format!("socket:[{inode}]"))
        })
    })
}

/// A GTK display socket left by a killed helper: nothing answers on it.
fn stale_socket(socket: &std::path::Path) -> bool {
    matches!(
        std::os::unix::net::UnixStream::connect(socket),
        Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused
    )
}

struct TempFolder(PathBuf);

impl Drop for TempFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Start `gtk4-broadwayd` on a free display number. Its web server listens
/// on a Unix socket in a private folder, never a TCP port.
fn start_display() -> Result<Display, String> {
    let base = private_base();
    sweep(&base);
    let folder = TempFolder(private_folder(&base, std::process::id())?);
    // GTK's sockets live in the runtime folder: when $XDG_RUNTIME_DIR is
    // unset, missing or not writable (a service, sudo, a stale session),
    // this helper's private folder stands in for it.
    // Decided from the variable itself, before GLib is asked: GLib caches
    // the runtime folder the first time, and GTK would keep a bad one.
    let given = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    // Canonical, and given back to GTK as such: the display's socket is
    // matched by its path in /proc/net/unix (listens_on), which a symlinked
    // or relative spelling wouldn't match.
    let runtime = match given
        .filter(|path| usable_folder(path))
        .map(std::fs::canonicalize)
    {
        Some(Ok(path)) => {
            unsafe { std::env::set_var("XDG_RUNTIME_DIR", &path) };
            path
        }
        _ => {
            eprintln!(
                "{} isn't a usable runtime folder; the page's display uses a private one",
                std::env::var_os("XDG_RUNTIME_DIR")
                    .map_or("$XDG_RUNTIME_DIR (unset)".into(), |p| {
                        PathBuf::from(p).display().to_string()
                    })
            );
            unsafe { std::env::set_var("XDG_RUNTIME_DIR", &folder.0) };
            folder.0.clone()
        }
    };
    // A daemon that exits at once, again and again, won't start on any
    // number: say why instead of trying them all.
    let mut quick_exits = 0;
    let start = std::process::id() % 800;
    for n in (0..800).map(|i| 100 + (start + i) % 800) {
        // GTK's display :n is the socket broadway{n+1}.socket.
        let socket = runtime.join(format!("broadway{}.socket", n + 1));
        if socket.exists() {
            if !stale_socket(&socket) {
                continue;
            }
            let _ = std::fs::remove_file(&socket);
        }
        let mut command = Command::new("gtk4-broadwayd");
        command
            .arg(format!(":{n}"))
            .arg("--unixsocket")
            .arg(folder.0.join("http.socket"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // It dies with this process, even if this one is killed.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                "gtk4-broadwayd is not installed: install libgtk-4-bin \
                 (Debian/Ubuntu), gtk4 (Fedora) or gtk4 (Arch)"
                    .to_string()
            }
            _ => format!("gtk4-broadwayd: {e}"),
        })?;
        let deadline = Instant::now() + DISPLAY_START;
        while Instant::now() < deadline {
            // Only a socket our own daemon listens on: a helper racing for
            // the same number may have made it, with ours about to exit
            // (and removing the other's socket later would break it).
            let alive = matches!(child.try_wait(), Ok(None));
            if alive && socket.exists() && listens_on(child.id(), &socket) {
                unsafe { std::env::set_var("BROADWAY_DISPLAY", format!(":{n}")) };
                return Ok(Display {
                    child,
                    socket,
                    _folder: folder,
                });
            }
            if let Ok(Some(status)) = child.try_wait() {
                // Taken meanwhile, or it can't start at all.
                quick_exits += 1;
                if quick_exits >= 3 {
                    let mut why = String::new();
                    if let Some(mut err) = child.stderr.take() {
                        let _ = std::io::Read::read_to_string(&mut err, &mut why);
                    }
                    let why = why
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("")
                        .trim();
                    return Err(format!(
                        "gtk4-broadwayd exits at once ({status}){}",
                        if why.is_empty() {
                            String::new()
                        } else {
                            format!(": {why}")
                        }
                    ));
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    Err("no free GTK Broadway display (gtk4-broadwayd did not start)".into())
}

/// A folder this user can create sockets in.
fn usable_folder(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).is_ok_and(|m| {
        m.is_dir() && m.uid() == unsafe { libc::getuid() } && m.mode() & 0o700 == 0o700
    })
}

/// WebKit's sandbox needs unprivileged user namespaces, which Ubuntu 24.04
/// restricts to programs with an AppArmor profile. Installed with the .deb,
/// this helper runs under its own (packaging/deb/apparmor/isocast-browser)
/// and keeps the sandbox; elsewhere on such a system (the tarball) the page
/// runs without it, in this helper's separate processes.
fn sandbox_note() -> Option<&'static str> {
    let restricted =
        std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .is_ok_and(|v| v.trim() == "1");
    let label = std::fs::read_to_string("/proc/self/attr/apparmor/current")
        .or_else(|_| std::fs::read_to_string("/proc/self/attr/current"))
        .unwrap_or_default();
    sandbox_off(restricted, &label).then_some(
        "this system restricts unprivileged user namespaces \
         (kernel.apparmor_restrict_unprivileged_userns=1) and isocast-browser has no AppArmor \
         profile allowing them",
    )
}

/// Whether to run pages without WebKit's sandbox: user namespaces are
/// restricted and this process's AppArmor label isn't its own profile.
fn sandbox_off(restricted: bool, label: &str) -> bool {
    restricted && label.split_whitespace().next() != Some(APPARMOR_PROFILE)
}

/// The AppArmor profile's name (packaging/deb/apparmor/isocast-browser).
const APPARMOR_PROFILE: &str = "isocast-browser";

pub fn main() -> i32 {
    // For isocastd's doctor: this helper's sandbox decision, as it runs.
    if std::env::args().nth(1).as_deref() == Some("--sandbox-status") {
        match sandbox_note() {
            Some(reason) => println!("sandbox off: {reason}"),
            None => println!("sandbox on"),
        }
        return 0;
    }
    let out = Out::start();
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            out.fail(&err);
            return 2;
        }
    };
    if let Some(note) = sandbox_note() {
        eprintln!("warning: WebKit's sandbox is off: {note}; the page still runs outside isocastd");
        unsafe { std::env::set_var("WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS", "1") };
    }
    // Software rendering throughout: no GPU is assumed or touched.
    unsafe {
        std::env::set_var("GDK_BACKEND", "broadway");
        std::env::set_var("GSK_RENDERER", "cairo");
    }
    let _display = match start_display() {
        Ok(display) => display,
        Err(err) => {
            out.fail(&err);
            return 3;
        }
    };
    if let Err(err) = gtk4::init() {
        out.fail(&format!(
            "GTK could not start on its private display: {err}"
        ));
        return 3;
    }
    run(args, out)
}

fn run(args: Args, out: Out) -> i32 {
    let out = Rc::new(out);
    // Pages play media and run Web Audio without a click, as in OBS: the
    // autoplay policy decides that in WebKitGTK (the "user gesture"
    // setting alone isn't enough), and without it Web Audio stays
    // "interrupted", so no page sound reaches the source either.
    let policies = webkit6::WebsitePolicies::builder()
        .autoplay(webkit6::AutoplayPolicy::Allow)
        .build();
    let view = webkit6::WebView::builder()
        .website_policies(&policies)
        .build();
    view.set_background_color(&gdk::RGBA::new(0.0, 0.0, 0.0, 0.0));
    view.set_size_request(args.width as i32, args.height as i32);
    if let Some(settings) = webkit6::prelude::WebViewExt::settings(&view) {
        settings.set_hardware_acceleration_policy(webkit6::HardwareAccelerationPolicy::Never);
        // Media plays without a click, and a local page may load the
        // files next to it, as in OBS.
        settings.set_media_playback_requires_user_gesture(false);
        settings.set_allow_file_access_from_file_urls(true);
    }
    let content = view
        .user_content_manager()
        .expect("a WebView has a content manager");
    if !args.css.trim().is_empty() {
        content.add_style_sheet(&webkit6::UserStyleSheet::new(
            &args.css,
            webkit6::UserContentInjectedFrames::TopFrame,
            webkit6::UserStyleLevel::User,
            &[],
            &[],
        ));
    }
    if args.audio {
        content.add_script(&webkit6::UserScript::new(
            AUDIO_SCRIPT,
            webkit6::UserContentInjectedFrames::TopFrame,
            webkit6::UserScriptInjectionTime::Start,
            &[],
            &[],
        ));
        content.register_script_message_handler(AUDIO_HANDLER, None);
        let out = out.clone();
        content.connect_script_message_received(Some(AUDIO_HANDLER), move |_, value| {
            if let Some((rate, samples)) = decode_audio(&value.to_str()) {
                let mut record = Out::record(b"AUDI", 4 + samples.len());
                record.extend_from_slice(&rate.to_le_bytes());
                record.extend_from_slice(&samples);
                out.send_wait(record);
            }
        });
    }
    let window = gtk4::Window::new();
    window.set_default_size(args.width as i32, args.height as i32);
    window.set_decorated(false);
    window.set_child(Some(&view));
    window.present();

    let main = glib::MainLoop::new(None, false);
    let exit = Rc::new(Exit {
        main: main.clone(),
        code: Cell::new(0),
    });
    let pacer = Rc::new(Pacer {
        start: Instant::now(),
        fps: args.fps,
        tick: Cell::new(0),
        busy: Cell::new(false),
        cleared: Cell::new(false),
        frame: RefCell::new(Vec::new()),
        warned: Cell::new(false),
        load_started: Cell::new(Instant::now()),
        last_beat: Cell::new(Instant::now()),
    });
    // WebKit's web process ending leaves a dead view that would never
    // draw again: say why and exit, and isocastd restarts the page.
    {
        let (exit, out) = (exit.clone(), out.clone());
        view.connect_web_process_terminated(move |_, reason| {
            let why = match reason {
                webkit6::WebProcessTerminationReason::Crashed => "crashed",
                webkit6::WebProcessTerminationReason::ExceededMemoryLimit => {
                    "exceeded WebKit's memory limit"
                }
                _ => "was terminated",
            };
            exit.fail(&out, &format!("the page's web process {why}"), 4);
        });
    }
    let loaded = Rc::new(Cell::new(true));
    // Every navigation restarts the load clock: isocastd's loads and
    // reloads, and the page's own (location.reload(), a meta refresh, a
    // link), which are "loading" too.
    {
        let pacer = pacer.clone();
        view.connect_load_changed(move |_, event| {
            if event == webkit6::LoadEvent::Started {
                pacer.load_started.set(Instant::now());
            }
        });
    }
    let load = {
        let (view, url) = (view.clone(), args.url.clone());
        move || view.load_uri(&url)
    };
    load();
    out.send_wait(Out::record(b"REDY", 0));

    // Commands from isocastd, polled on the main loop.
    let (send, commands) = mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("isocast-browser-in".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    return;
                }
            }
            let _ = send.send("quit".into());
        })
        .expect("the input thread starts");
    {
        let (view, loaded, main) = (view.clone(), loaded.clone(), main.clone());
        glib::timeout_add_local(Duration::from_millis(20), move || {
            while let Ok(command) = commands.try_recv() {
                match command.trim() {
                    "reload" if loaded.get() => view.reload_bypass_cache(),
                    "unload" => {
                        loaded.set(false);
                        view.load_html("", None);
                    }
                    "load" if !loaded.get() => {
                        loaded.set(true);
                        load();
                    }
                    "quit" => {
                        main.quit();
                        return glib::ControlFlow::Break;
                    }
                    _ => {}
                }
            }
            glib::ControlFlow::Continue
        });
    }

    // Snapshots at the frame rate, on a schedule that doesn't drift; a tick
    // is skipped while one is in flight.
    schedule(
        pacer,
        view.clone(),
        loaded,
        out,
        exit.clone(),
        args.width,
        args.height,
    );
    main.run();
    exit.code.get()
}

struct Pacer {
    start: Instant,
    fps: u32,
    tick: Cell<u64>,
    busy: Cell<bool>,
    /// An empty picture has been sent since the page was unloaded.
    cleared: Cell<bool>,
    /// The picture being sent's bytes (reused).
    frame: RefCell<Vec<u8>>,
    warned: Cell<bool>,
    /// When the page (re)started loading.
    load_started: Cell<Instant>,
    /// When isocastd last heard the page is still loading.
    last_beat: Cell<Instant>,
}

impl Pacer {
    fn next_delay(&self) -> Duration {
        let n = self.tick.get() + 1;
        self.tick.set(n);
        let due = self.start + Duration::from_nanos(n * 1_000_000_000 / u64::from(self.fps));
        due.saturating_duration_since(Instant::now())
    }
}

fn schedule(
    pacer: Rc<Pacer>,
    view: webkit6::WebView,
    loaded: Rc<Cell<bool>>,
    out: Rc<Out>,
    exit: Rc<Exit>,
    w: u32,
    h: u32,
) {
    let delay = pacer.next_delay();
    glib::timeout_add_local_once(delay, move || {
        tick(&pacer, &view, &loaded, &out, &exit, w, h);
        schedule(pacer, view, loaded, out, exit, w, h);
    });
}

fn tick(
    pacer: &Rc<Pacer>,
    view: &webkit6::WebView,
    loaded: &Cell<bool>,
    out: &Rc<Out>,
    exit: &Exit,
    w: u32,
    h: u32,
) {
    if !loaded.get() {
        clear_once(&pacer.cleared, || {
            out.send(frame_record(w, h, &vec![0; (w * h * 4) as usize]))
        });
        return;
    }
    pacer.cleared.set(false);
    // Nothing until the page has loaded (as on macOS): the first picture a
    // source shows is the page, not an empty view. Meanwhile isocastd
    // hears that it is loading, and a page that never finishes fails.
    if view.is_loading() {
        if pacer.load_started.get().elapsed() > LOAD_TIMEOUT {
            exit.fail(
                out,
                &format!(
                    "the page didn't finish loading within {} s",
                    LOAD_TIMEOUT.as_secs()
                ),
                5,
            );
        } else if pacer.last_beat.get().elapsed() >= LOADING_BEAT {
            pacer.last_beat.set(Instant::now());
            out.send(Out::record(b"REDY", 0));
        }
        return;
    }
    if pacer.busy.replace(true) {
        return;
    }
    let (pacer, out) = (pacer.clone(), out.clone());
    view.snapshot(
        webkit6::SnapshotRegion::Visible,
        webkit6::SnapshotOptions::TRANSPARENT_BACKGROUND,
        None::<&gio::Cancellable>,
        move |result| {
            pacer.busy.set(false);
            match result {
                Ok(texture) => {
                    let mut scratch = pacer.frame.borrow_mut();
                    let record = picture(&texture, w, h, out.buffer(), &mut scratch);
                    // A full queue drops the picture: isocastd is behind.
                    out.send(record);
                }
                Err(err) => {
                    if !pacer.warned.replace(true) {
                        eprintln!("snapshot failed: {err}");
                    }
                }
            }
        },
    );
}

/// Send one empty picture once the page is unloaded, so the source shows
/// nothing rather than the page's last picture. `send` returns false when
/// the queue was full (isocastd is behind): then it's tried again on the
/// next tick, not counted as sent.
fn clear_once(cleared: &Cell<bool>, send: impl FnOnce() -> bool) {
    if !cleared.get() && send() {
        cleared.set(true);
    }
}

/// A `FRAM` record of `texture`, straight-alpha BGRA at exactly w × h (a
/// smaller or larger snapshot is cropped or padded with transparency), in
/// `record` (a reused buffer). The snapshot normally is w × h: then it is
/// downloaded straight into the record, the only copy of its pixels.
fn picture(
    texture: &gdk::Texture,
    w: u32,
    h: u32,
    mut record: Vec<u8>,
    scratch: &mut Vec<u8>,
) -> Vec<u8> {
    let (tw, th) = (
        texture.width().max(0) as u32,
        texture.height().max(0) as u32,
    );
    let len = (w * h * 4) as usize;
    record.clear();
    record.extend_from_slice(b"FRAM");
    record.extend_from_slice(&(8 + len as u32).to_le_bytes());
    record.extend_from_slice(&w.to_le_bytes());
    record.extend_from_slice(&h.to_le_bytes());
    record.resize(16 + len, 0);
    let pixels = &mut record[16..];
    // Cairo's ARGB32: premultiplied, BGRA in memory on little-endian.
    if (tw, th) == (w, h) {
        texture.download(pixels, w as usize * 4);
    } else {
        let stride = tw as usize * 4;
        scratch.resize(stride * th as usize, 0);
        texture.download(scratch, stride);
        pixels.fill(0);
        for y in 0..h.min(th) as usize {
            let row = &scratch[y * stride..y * stride + w.min(tw) as usize * 4];
            pixels[y * w as usize * 4..y * w as usize * 4 + row.len()].copy_from_slice(row);
        }
    }
    unpremultiply(pixels);
    record
}

fn frame_record(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
    let mut record = Out::record(b"FRAM", 8 + pixels.len());
    record.extend_from_slice(&w.to_le_bytes());
    record.extend_from_slice(&h.to_le_bytes());
    record.extend_from_slice(pixels);
    record
}

/// Premultiplied BGRA to straight alpha, in place.
fn unpremultiply(pixels: &mut [u8]) {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        let a = u32::from(pixel[3]);
        if a != 0 && a != 255 {
            for c in &mut pixel[..3] {
                *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// Audio rates passed on (isocastd refuses others): the page's script
/// picks the rate, so a page could ask for any.
const AUDIO_RATES: std::ops::RangeInclusive<u32> = 8_000..=192_000;

/// `rate:base64` from audio.js: the rate and the samples' raw bytes
/// (interleaved stereo f32, little-endian). The page can post anything
/// there, so a rate outside [`AUDIO_RATES`] or more than a second of
/// sound in one message is dropped, before decoding.
fn decode_audio(message: &str) -> Option<(u32, Vec<u8>)> {
    let (rate, data) = message.split_once(':')?;
    let rate: u32 = rate.parse().ok()?;
    let second = rate as usize * 8;
    if !AUDIO_RATES.contains(&rate) || data.len() > second.div_ceil(3) * 4 {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    (bytes.len().is_multiple_of(8) && bytes.len() <= second).then_some((rate, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpremultiplies_partly_transparent_pixels_only() {
        let mut pixels = [64, 32, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0];
        unpremultiply(&mut pixels);
        assert_eq!(pixels, [128, 64, 0, 128, 10, 20, 30, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn the_sandbox_stays_on_under_the_apparmor_profile() {
        assert!(!sandbox_off(false, "unconfined\n"));
        assert!(sandbox_off(true, "unconfined\n"));
        assert!(sandbox_off(true, ""));
        assert!(sandbox_off(true, "isocast-browser-other (enforce)\n"));
        assert!(!sandbox_off(true, "isocast-browser (unconfined)\n"));
    }

    #[test]
    fn private_folders_are_made_safely_and_swept() {
        use std::os::unix::fs::PermissionsExt;
        let base =
            std::env::temp_dir().join(format!("isocast-browser-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // Fresh: 0700.
        let folder = private_folder(&base, 7).unwrap();
        assert_eq!(
            folder.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Our own leftover (pid reuse) is replaced, emptied.
        std::fs::write(folder.join("http.socket"), b"").unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
        let again = private_folder(&base, 7).unwrap();
        assert!(!again.join("http.socket").exists());
        assert_eq!(
            again.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        // A planted symlink isn't followed or used.
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, base.join("isocast-browser-8")).unwrap();
        assert!(private_folder(&base, 8).is_err());
        assert!(elsewhere.exists());
        // The sweep removes dead helpers' folders only (pid 7 isn't an
        // isocast-browser), never what a symlink points at.
        let stale = base.join("broadway901.socket");
        // Make the node a dead listener leaves behind, without opening an fd
        // that another test's concurrently forked child could keep alive.
        let path = std::ffi::CString::new(stale.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: path is NUL-terminated; a socket node needs no device number.
        let rc = unsafe { libc::mknod(path.as_ptr(), libc::S_IFSOCK | 0o600, 0) };
        assert_eq!(rc, 0, "mknod: {}", std::io::Error::last_os_error());
        let live = base.join("broadway902.socket");
        let _listening = std::os::unix::net::UnixListener::bind(&live).unwrap();
        sweep(&base);
        assert!(!base.join("isocast-browser-7").exists());
        assert!(!stale.exists(), "a dead display's socket is removed");
        assert!(live.exists(), "a live display's socket is kept");
        assert!(elsewhere.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_display_socket_counts_only_for_the_process_listening_on_it() {
        let base =
            std::env::temp_dir().join(format!("isocast-browser-sock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join("broadway901.socket");
        let _listening = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(listens_on(std::process::id(), &path));
        // Another live process (a racing helper's daemon) doesn't own it.
        let mut other = Command::new("sleep").arg("5").spawn().unwrap();
        assert!(!listens_on(other.id(), &path));
        let _ = other.kill();
        let _ = other.wait();
        assert!(!listens_on(
            std::process::id(),
            &base.join("broadway902.socket")
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_empty_picture_after_unload_is_retried_until_it_is_queued() {
        let cleared = Cell::new(false);
        let tries = Cell::new(0);
        let full = || {
            tries.set(tries.get() + 1);
            false
        };
        clear_once(&cleared, full);
        clear_once(&cleared, full);
        assert_eq!(
            (tries.get(), cleared.get()),
            (2, false),
            "a full queue: tried again"
        );
        clear_once(&cleared, || {
            tries.set(tries.get() + 1);
            true
        });
        assert_eq!((tries.get(), cleared.get()), (3, true));
        clear_once(&cleared, || panic!("sent once only"));
    }

    #[test]
    fn reads_the_page_line() {
        let line = format!(
            "page url {} {}\n",
            hexed("https://x.test/?t=1 2"),
            hexed("body{}")
        );
        assert_eq!(
            parse_page(&line).unwrap(),
            ("https://x.test/?t=1 2".into(), "body{}".into())
        );
        let (url, css) = parse_page(&format!("page file {} \n", hexed("/tmp/a b.html"))).unwrap();
        assert_eq!((url.as_str(), css.as_str()), ("file:///tmp/a%20b.html", ""));
        for bad in [
            "",
            "page url zz \n",
            "page ftp 61 \n",
            "page url 6 \n",
            "page url 61\n",
        ] {
            assert!(parse_page(bad).is_err(), "{bad:?}");
        }
    }

    fn hexed(text: &str) -> String {
        text.bytes().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn decodes_audio_messages() {
        let samples: Vec<u8> = [0.5f32, -0.5]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let message = format!(
            "44100:{}",
            base64::engine::general_purpose::STANDARD.encode(&samples)
        );
        assert_eq!(decode_audio(&message), Some((44100, samples)));
        assert_eq!(decode_audio("48000:@@"), None);
        // Rates a page may claim but isocastd won't resample, and more
        // than a second of sound in one message.
        let pair = base64::engine::general_purpose::STANDARD.encode([0u8; 8]);
        assert_eq!(decode_audio(&format!("1:{pair}")), None);
        assert_eq!(decode_audio(&format!("7999:{pair}")), None);
        assert_eq!(decode_audio(&format!("192001:{pair}")), None);
        assert_eq!(decode_audio(&format!("4294967295:{pair}")), None);
        assert!(decode_audio(&format!("8000:{pair}")).is_some());
        let second = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 8000 * 8]);
        assert!(decode_audio(&format!("8000:{second}")).is_some());
        let over = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 8000 * 8 + 8]);
        assert_eq!(decode_audio(&format!("8000:{over}")), None);
        assert_eq!(decode_audio("no-colon"), None);
        // Not whole stereo f32 frames.
        assert_eq!(
            decode_audio(&format!(
                "48000:{}",
                base64::engine::general_purpose::STANDARD.encode([1, 2, 3])
            )),
            None
        );
    }

    #[test]
    fn frame_records_carry_size_and_pixels() {
        let record = frame_record(2, 1, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&record[..4], b"FRAM");
        assert_eq!(u32::from_le_bytes(record[4..8].try_into().unwrap()), 16);
        assert_eq!(u32::from_le_bytes(record[8..12].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(record[12..16].try_into().unwrap()), 1);
        assert_eq!(&record[16..], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
