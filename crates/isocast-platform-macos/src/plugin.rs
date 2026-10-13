//! Running process plugins: spawning them in a Seatbelt sandbox without
//! isocastd's privacy grants, and telling which process is at the other end
//! of a socket connection.
//!
//! The sandbox is a `sandbox-exec` profile. Apple deprecated sandbox-exec
//! but still ships and uses it; if it goes, plugins run unsandboxed and
//! `doctor` says so.

use std::ffi::{CString, c_char, c_int};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// The program that applies a sandbox profile and runs a command in it.
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Whether plugins can be sandboxed on this Mac.
pub fn sandbox_available() -> bool {
    Path::new(SANDBOX_EXEC).exists()
}

/// What a plugin's process may touch. Every path must be canonical
/// (`/private/tmp`, not `/tmp`): Seatbelt matches resolved paths.
#[derive(Debug, Clone, Default)]
pub struct Sandbox {
    /// The user's home folder, unreadable but for `readable`.
    pub home: PathBuf,
    /// Folders and files it may read inside the home folder: its own
    /// folders and the files it was granted.
    pub readable: Vec<PathBuf>,
    /// Folders it may write in (its data folder).
    pub writable: Vec<PathBuf>,
    /// Programs it may run, and folders whose programs it may run (its own
    /// folder, its interpreter's installation).
    pub programs: Vec<PathBuf>,
    pub program_folders: Vec<PathBuf>,
    /// The socket it reaches isocastd on: the only one it may connect to.
    pub socket: PathBuf,
    /// It may connect anywhere on the network. Without it, it can only
    /// serve (accept connections), and reach isocastd on its socket.
    pub network: bool,
}

/// Mach services a plugin may not look up: privacy-protected devices and
/// data (whose servers would check isocastd's grants, or prompt), the
/// screen and windows, other apps (Launch Services, Apple events), the
/// clipboard and the keychain.
const DENIED_SERVICES: &[&str] = &[
    "com.apple.tccd",
    "com.apple.tccd.system",
    "com.apple.windowserver.active",
    "com.apple.replayd",
    "com.apple.coreservices.launchservicesd",
    "com.apple.lsd.mapdb",
    "com.apple.lsd.modifydb",
    "com.apple.coreservices.appleevents",
    "com.apple.pasteboard.1",
    "com.apple.SecurityServer",
    "com.apple.audio.audiohald",
    "com.apple.audio.coreaudiod",
    "com.apple.cmio.registerassistantservice",
    "com.apple.locationd.registration",
    "com.apple.contactsd.persistence",
];

impl Sandbox {
    /// The Seatbelt profile.
    pub fn profile(&self) -> String {
        let mut out = String::from("(version 1)\n(allow default)\n");
        let list = |paths: &[PathBuf], filter: &str| -> String {
            paths
                .iter()
                .map(|path| format!(" ({filter} {})", quote(path)))
                .collect()
        };
        // The home folder: unreadable but for its own folders and granted
        // files; their parent folders can be looked at (not listed), so
        // paths to them resolve.
        out.push_str(&format!(
            ";; The home folder is unreadable but for the plugin's folders.\n\
             (deny file-read* (subpath {}))\n",
            quote(&self.home)
        ));
        let mut parents: Vec<PathBuf> = Vec::new();
        for path in &self.readable {
            for parent in path.ancestors().skip(1) {
                if parent.starts_with(&self.home) && !parents.iter().any(|p| p == parent) {
                    parents.push(parent.to_path_buf());
                }
            }
        }
        if !self.readable.is_empty() {
            out.push_str(&format!(
                "(allow file-read*{})\n",
                list(&self.readable, "subpath")
            ));
        }
        if !parents.is_empty() {
            out.push_str(&format!(
                "(allow file-read-metadata{})\n",
                list(&parents, "literal")
            ));
        }
        out.push_str(&format!(
            ";; Writes only in its data folder.\n\
             (deny file-write*)\n\
             (allow file-write*{} (literal \"/dev/null\") (literal \"/dev/dtracehelper\") \
             (literal \"/dev/tty\"))\n",
            list(&self.writable, "subpath")
        ));
        out.push_str(&format!(
            ";; Programs: only its own command.\n\
             (deny process-exec*)\n\
             (allow process-exec*{}{})\n",
            list(&self.programs, "literal"),
            list(&self.program_folders, "subpath")
        ));
        out.push_str(&format!(
            ";; Network: serving pages, and its own socket to isocastd; connecting out only if \
             granted. (Seatbelt can't hold serving to loopback: `localhost` here also admits \
             a bind to 0.0.0.0, so a plugin binds 127.0.0.1 itself.)\n\
             (deny network*)\n\
             (allow network-bind network-inbound (local ip \"localhost:*\"))\n\
             (allow network-outbound (literal {}))\n",
            quote(&self.socket)
        ));
        if self.network {
            out.push_str(
                "(allow network-outbound (remote ip))\n\
                 (allow network-outbound (literal \"/private/var/run/mDNSResponder\"))\n",
            );
        }
        out.push_str(";; No camera, microphone, screen, other apps, clipboard or keychain.\n");
        out.push_str("(deny device-camera device-microphone)\n(deny mach-lookup");
        for service in DENIED_SERVICES {
            out.push_str(&format!(" (global-name \"{service}\")"));
        }
        out.push_str(")\n");
        out.push_str(
            ";; Signals only to its own processes.\n\
             (deny signal (target others))\n",
        );
        out
    }
}

/// A path as an SBPL string.
fn quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// How to start a plugin's process.
pub struct Spawn<'a> {
    /// The program, as an absolute path.
    pub program: &'a Path,
    /// Its arguments (not the program itself).
    pub args: &'a [String],
    /// Its whole environment.
    pub env: &'a [(String, String)],
    /// The folder it runs in.
    pub cwd: &'a Path,
    /// Where its output goes (appended).
    pub log: &'a Path,
    /// Run it in this sandbox profile; unsandboxed if absent.
    pub profile: Option<&'a str>,
}

/// Start a plugin's process: in its own process group (so stopping it
/// stops what it started), with only stdin (/dev/null), stdout and stderr
/// (the log) open, default signal handling, and isocastd's responsibility
/// for it disclaimed, so macOS's privacy checks see the plugin itself
/// rather than isocastd and its camera, microphone and screen grants.
/// Returns its process id.
pub fn spawn(spawn: &Spawn) -> io::Result<u32> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(spawn.log)?;
    let null = std::fs::File::open("/dev/null")?;
    let cstring = |text: &[u8]| {
        CString::new(text).map_err(|_| io::Error::other("an argument or variable has a NUL byte"))
    };
    let mut argv: Vec<CString> = Vec::new();
    let path = match spawn.profile {
        Some(profile) => {
            argv.push(cstring(SANDBOX_EXEC.as_bytes())?);
            argv.push(cstring(b"-p")?);
            argv.push(cstring(profile.as_bytes())?);
            cstring(SANDBOX_EXEC.as_bytes())?
        }
        None => cstring(spawn.program.as_os_str().as_bytes())?,
    };
    argv.push(cstring(spawn.program.as_os_str().as_bytes())?);
    for arg in spawn.args {
        argv.push(cstring(arg.as_bytes())?);
    }
    let env: Vec<CString> = spawn
        .env
        .iter()
        .map(|(key, value)| cstring(format!("{key}={value}").as_bytes()))
        .collect::<io::Result<_>>()?;
    let cwd = cstring(spawn.cwd.as_os_str().as_bytes())?;
    let mut argv_ptrs: Vec<*mut c_char> = argv.iter().map(|a| a.as_ptr().cast_mut()).collect();
    argv_ptrs.push(std::ptr::null_mut());
    let mut env_ptrs: Vec<*mut c_char> = env.iter().map(|e| e.as_ptr().cast_mut()).collect();
    env_ptrs.push(std::ptr::null_mut());

    // SAFETY: the attribute and file-action objects are initialised before
    // use and destroyed on every path below; every pointer handed to
    // posix_spawn (paths, argv, envp) outlives the call.
    unsafe {
        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
        check(libc::posix_spawn_file_actions_init(&mut actions))?;
        if let Err(err) = check(libc::posix_spawnattr_init(&mut attr)) {
            libc::posix_spawn_file_actions_destroy(&mut actions);
            return Err(err);
        }
        let result = (|| -> io::Result<u32> {
            check(libc::posix_spawn_file_actions_adddup2(
                &mut actions,
                null.as_raw_fd(),
                0,
            ))?;
            check(libc::posix_spawn_file_actions_adddup2(
                &mut actions,
                log.as_raw_fd(),
                1,
            ))?;
            check(libc::posix_spawn_file_actions_adddup2(
                &mut actions,
                log.as_raw_fd(),
                2,
            ))?;
            check(libc::posix_spawn_file_actions_addchdir_np(
                &mut actions,
                cwd.as_ptr(),
            ))?;
            let flags = libc::POSIX_SPAWN_SETPGROUP
                | libc::POSIX_SPAWN_CLOEXEC_DEFAULT
                | libc::POSIX_SPAWN_SETSIGDEF
                | libc::POSIX_SPAWN_SETSIGMASK;
            check(libc::posix_spawnattr_setflags(&mut attr, flags as _))?;
            check(libc::posix_spawnattr_setpgroup(&mut attr, 0))?;
            let mut all: libc::sigset_t = 0;
            libc::sigfillset(&mut all);
            check(libc::posix_spawnattr_setsigdefault(&mut attr, &all))?;
            let mut none: libc::sigset_t = 0;
            libc::sigemptyset(&mut none);
            check(libc::posix_spawnattr_setsigmask(&mut attr, &none))?;
            disclaim(&mut attr)?;
            let mut pid: libc::pid_t = 0;
            check(libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                &actions,
                &attr,
                argv_ptrs.as_ptr(),
                env_ptrs.as_ptr(),
            ))?;
            Ok(pid as u32)
        })();
        libc::posix_spawnattr_destroy(&mut attr);
        libc::posix_spawn_file_actions_destroy(&mut actions);
        result
    }
}

/// Mark the process `attr` starts as responsible for itself, so macOS's
/// privacy checks (TCC) don't count it as isocastd. Private API in
/// libSystem, stable since macOS 10.14 and what browsers use for their
/// helpers (`isocast-client` launches isocastd the same way).
unsafe fn disclaim(attr: &mut libc::posix_spawnattr_t) -> io::Result<()> {
    unsafe extern "C" {
        fn responsibility_spawnattrs_setdisclaim(
            attrs: *mut libc::posix_spawnattr_t,
            disclaim: c_int,
        ) -> c_int;
    }
    // SAFETY: an initialised attribute object.
    check(unsafe { responsibility_spawnattrs_setdisclaim(attr, 1) })
}

fn check(code: c_int) -> io::Result<()> {
    match code {
        0 => Ok(()),
        code => Err(io::Error::from_raw_os_error(code)),
    }
}

/// The process at the other end of a Unix socket connection.
pub fn peer_pid(fd: RawFd) -> Option<u32> {
    let mut pid: libc::pid_t = 0;
    let mut size = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `size` bytes into `pid`.
    let code = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut size,
        )
    };
    (code == 0 && pid > 0).then_some(pid as u32)
}

/// A process's parent and when it started (seconds and microseconds since
/// 1970, which tell a process from a later one given the same id).
pub fn process_info(pid: u32) -> Option<(u32, (u64, u64))> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as c_int;
    // SAFETY: proc_pidinfo fills at most `size` bytes of `info`, and
    // returns how many it filled; the struct is only read when it is all
    // filled in.
    let filled = unsafe {
        libc::proc_pidinfo(
            pid as c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if filled != size {
        return None;
    }
    // SAFETY: filled in by proc_pidinfo above.
    let info = unsafe { info.assume_init() };
    Some((info.pbi_ppid, (info.pbi_start_tvsec, info.pbi_start_tvusec)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_quote_paths_and_grant_only_what_is_given() {
        let sandbox = Sandbox {
            home: "/Users/ryan".into(),
            readable: vec!["/Users/ryan/Library/Application Support/Isocast/plugins/deck".into()],
            writable: vec![
                "/Users/ryan/Library/Application Support/Isocast/plugins/deck/data".into(),
            ],
            programs: vec!["/opt/homebrew/bin/python3".into()],
            program_folders: vec!["/opt/homebrew/Cellar/python@3.14/3.14.7".into()],
            socket: "/private/tmp/a \"b\".sock".into(),
            network: false,
        };
        let profile = sandbox.profile();
        assert!(profile.contains("(deny file-read* (subpath \"/Users/ryan\"))"));
        assert!(profile.contains(
            "(allow file-read-metadata (literal \"/Users/ryan/Library/Application Support/Isocast/plugins\")"
        ));
        assert!(profile.contains("(literal \"/Users/ryan\")"));
        assert!(
            profile
                .contains("(allow network-outbound (literal \"/private/tmp/a \\\"b\\\".sock\"))")
        );
        assert!(!profile.contains("(remote ip)"));
        assert!(
            Sandbox {
                network: true,
                ..sandbox
            }
            .profile()
            .contains("(allow network-outbound (remote ip))")
        );
    }

    #[test]
    fn a_process_knows_its_parent() {
        let me = std::process::id();
        let (parent, started) = process_info(me).unwrap();
        // SAFETY: getppid has no preconditions.
        assert_eq!(parent, unsafe { libc::getppid() } as u32);
        assert!(started.0 > 0);
    }
}
