//! The Linux secret store. Each case reruns this binary as a child with its
//! own `HOME`, `XDG_CONFIG_HOME` and session bus; `child_entry` does the work.
//! The `#[ignore]` cases start a private bus with a throwaway keyring: run
//! them with `cargo test -p isocast-platform-credentials --test keyring -- --include-ignored`.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use isocast_platform_credentials::{delete_secret, has_secret, secret, set_secret};

const ROLE: &str = "ISOCAST_KEYRING_TEST_ROLE";
const ACCOUNT: &str = "test-account";
const VALUE: &str = "live_0123456789abcdef";
const OLD: &str = "live_old";
const NEWER: &str = "live_newer";

/// The service name every case uses: unique to this run, so a leftover
/// item can never be mistaken for a real one.
fn service() -> String {
    std::env::var("ISOCAST_KEYRING_TEST_SERVICE").expect("set by the parent")
}

fn files(config: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(config.join("isocast/credentials")) {
        Ok(dir) => dir
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn config() -> PathBuf {
    PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").expect("set by the parent"))
}

fn lookup(service: &str) -> String {
    let out = Command::new("secret-tool")
        .args(["lookup", "application", "isocast", "service", service])
        .args(["account", ACCOUNT])
        .output()
        .expect("secret-tool");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The child side. Passes trivially when this binary runs as an ordinary
/// test.
#[test]
fn child_entry() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let service = service();
    match role.as_str() {
        "files" => {
            set_secret(&service, ACCOUNT, VALUE).unwrap();
            let kept = files(&config());
            assert_eq!(kept.len(), 1, "one plaintext file: {kept:?}");
            let mode = std::fs::metadata(&kept[0]).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            assert_eq!(secret(&service, ACCOUNT).unwrap().as_deref(), Some(VALUE));
            assert!(has_secret(&service, ACCOUNT).unwrap());
            assert!(delete_secret(&service, ACCOUNT).unwrap());
            assert!(!has_secret(&service, ACCOUNT).unwrap());
            assert_eq!(secret(&service, ACCOUNT).unwrap(), None);
            let kept = files(&config());
            assert_eq!(kept.len(), 1, "an empty file marks the delete: {kept:?}");
            assert_eq!(std::fs::metadata(&kept[0]).unwrap().len(), 0);
        }
        "keyring" => {
            set_secret(&service, ACCOUNT, VALUE).unwrap();
            assert!(
                files(&config()).is_empty(),
                "no plaintext file with a keyring"
            );
            assert_eq!(lookup(&service), VALUE, "secret-tool sees the item");
            assert_eq!(secret(&service, ACCOUNT).unwrap().as_deref(), Some(VALUE));
            assert!(has_secret(&service, ACCOUNT).unwrap());
            assert!(delete_secret(&service, ACCOUNT).unwrap());
            assert_eq!(lookup(&service), "");
            assert_eq!(secret(&service, ACCOUNT).unwrap(), None);
        }
        "write-file" => {
            set_secret(&service, ACCOUNT, VALUE).unwrap();
            assert_eq!(files(&config()).len(), 1);
        }
        "keyring-old" => {
            set_secret(&service, ACCOUNT, OLD).unwrap();
            assert_eq!(lookup(&service), OLD);
        }
        "delete-file" => {
            delete_secret(&service, ACCOUNT).unwrap();
            assert!(!has_secret(&service, ACCOUNT).unwrap());
            assert_eq!(files(&config()).len(), 1);
        }
        "deleted-stays" => {
            assert_eq!(lookup(&service), OLD, "the keyring keeps its older item");
            assert!(!has_secret(&service, ACCOUNT).unwrap());
            assert_eq!(lookup(&service), OLD, "has_secret changes nothing");
            assert_eq!(secret(&service, ACCOUNT).unwrap(), None);
            assert_eq!(lookup(&service), "", "a read finishes the delete");
            assert!(files(&config()).is_empty(), "and removes the mark");
            set_secret(&service, ACCOUNT, NEWER).unwrap();
            assert_eq!(secret(&service, ACCOUNT).unwrap().as_deref(), Some(NEWER));
            assert!(delete_secret(&service, ACCOUNT).unwrap());
            assert!(
                files(&config()).is_empty(),
                "a keyring delete leaves no mark"
            );
            assert_eq!(lookup(&service), "");
        }
        "file-wins" => {
            assert_eq!(lookup(&service), OLD, "the keyring keeps its older item");
            assert!(has_secret(&service, ACCOUNT).unwrap());
            assert_eq!(lookup(&service), OLD, "has_secret changes nothing");
            assert_eq!(secret(&service, ACCOUNT).unwrap().as_deref(), Some(VALUE));
            assert_eq!(lookup(&service), VALUE, "a read moves the file's value in");
            assert!(files(&config()).is_empty(), "and removes the file");
            set_secret(&service, ACCOUNT, NEWER).unwrap();
            assert!(
                files(&config()).is_empty(),
                "a keyring write removes the file"
            );
            assert_eq!(lookup(&service), NEWER);
            assert_eq!(secret(&service, ACCOUNT).unwrap().as_deref(), Some(NEWER));
            assert!(delete_secret(&service, ACCOUNT).unwrap());
        }
        "locked" => {
            set_secret(&service, ACCOUNT, VALUE).unwrap();
            let lock = Command::new("secret-tool")
                .args([
                    "lock",
                    "--collection=/org/freedesktop/secrets/collection/login",
                ])
                .status()
                .expect("secret-tool");
            assert!(lock.success());
            let started = Instant::now();
            assert!(has_secret(&service, ACCOUNT).unwrap(), "seen while locked");
            let errors = [
                secret(&service, ACCOUNT).map(drop).unwrap_err(),
                set_secret(&service, ACCOUNT, NEWER).unwrap_err(),
                delete_secret(&service, ACCOUNT).map(drop).unwrap_err(),
            ];
            let took = started.elapsed();
            for error in &errors {
                assert!(
                    format!("{error:#}").contains("keyring is locked"),
                    "{error:#}"
                );
            }
            assert!(
                took < Duration::from_secs(2),
                "no prompt to wait on: {took:?}"
            );
            assert!(files(&config()).is_empty(), "never a file instead");
            println!("locked keyring: four calls in {took:?}: {:#}", errors[0]);
        }
        "file-store" => {
            set_secret(&service, ACCOUNT, VALUE).unwrap();
            assert_eq!(files(&config()).len(), 1);
            assert_eq!(secret(&service, ACCOUNT).unwrap().as_deref(), Some(VALUE));
            assert_eq!(lookup(&service), "", "the keyring is never used");
            assert!(delete_secret(&service, ACCOUNT).unwrap());
            assert!(!has_secret(&service, ACCOUNT).unwrap());
        }
        "refused" => {
            let expected = std::env::var("ISOCAST_KEYRING_TEST_EXPECT").unwrap();
            let error = format!("{:#}", set_secret(&service, ACCOUNT, VALUE).unwrap_err());
            assert!(error.contains(&expected), "{error}");
            assert!(files(&config()).is_empty(), "never a file instead");
            println!("refused: {error}");
        }
        "file-while-hung" => {
            let started = Instant::now();
            assert_eq!(secret(&service, ACCOUNT).unwrap().as_deref(), Some(VALUE));
            let first = started.elapsed();
            assert_eq!(secret(&service, ACCOUNT).unwrap().as_deref(), Some(VALUE));
            assert_eq!(files(&config()).len(), 1, "the file stays");
            println!("file read with a hung keyring: first {first:?}");
        }
        "hung" => {
            let started = Instant::now();
            let error = format!("{:#}", has_secret(&service, ACCOUNT).unwrap_err());
            let waited = started.elapsed();
            assert!(error.contains("did not answer within 5 seconds"), "{error}");
            assert!(
                waited >= Duration::from_secs(4) && waited < Duration::from_secs(12),
                "the first call waits for the time limit: {waited:?}"
            );
            let started = Instant::now();
            assert!(set_secret(&service, ACCOUNT, VALUE).is_err());
            assert!(delete_secret(&service, ACCOUNT).is_err());
            let later = started.elapsed();
            assert!(
                later < Duration::from_secs(1),
                "later calls fail at once: {later:?}"
            );
            assert!(files(&config()).is_empty(), "never a file instead");
            println!("hung keyring: first call {waited:?}, next two {later:?}: {error}");
        }
        other => panic!("unknown role {other}"),
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("isocast-keyring-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["home", "config", "data", "runtime"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        Self(root)
    }

    /// A command that runs `role` in the child, with this scratch as its
    /// world. `wrap` is a shell line that ends by running "$@".
    fn child(&self, role: &str, wrap: Option<&str>, bus: Option<&str>) -> Command {
        self.child_on(role, wrap.map(|line| (line, None)), bus)
    }

    /// Like `child`, on a private bus that starts no services on demand, so
    /// an installed keyring daemon is never activated.
    fn child_without_activation(&self, role: &str, wrap: &str) -> Command {
        let config = self.0.join("bus.conf");
        std::fs::write(&config, BUS_WITHOUT_ACTIVATION).unwrap();
        self.child_on(role, Some((wrap, Some(config))), None)
    }

    fn child_on(
        &self,
        role: &str,
        wrap: Option<(&str, Option<PathBuf>)>,
        bus: Option<&str>,
    ) -> Command {
        let exe = std::env::current_exe().unwrap();
        let mut command = match wrap {
            Some((line, config)) => {
                let mut c = Command::new("dbus-run-session");
                if let Some(config) = config {
                    c.arg("--config-file").arg(config);
                }
                c.args(["--", "sh", "-c", line, "sh"]).arg(&exe);
                c
            }
            None => Command::new(&exe),
        };
        command
            .args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.0.join("home"))
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_DATA_HOME", self.0.join("data"))
            .env("XDG_RUNTIME_DIR", self.0.join("runtime"))
            .env(
                "ISOCAST_KEYRING_TEST_SERVICE",
                format!("isocast-test-{}", std::process::id()),
            )
            .env(ROLE, role);
        if let Some(bus) = bus {
            command.env("DBUS_SESSION_BUS_ADDRESS", bus);
        }
        command
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(mut command: Command) {
    let out = command.output().expect("start the child");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    println!("{text}");
    assert!(out.status.success(), "child failed: {}\n{text}", out.status);
}

/// A throwaway keyring on the private bus, unlocked with a fixed password.
const WITH_KEYRING: &str = "printf test | gnome-keyring-daemon --unlock --components=secrets \
                            >/dev/null && exec \"$@\"";
/// A session bus with no service directories.
const BUS_WITHOUT_ACTIVATION: &str = r#"<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#;
/// Waits for the Secret Service's name on the private bus, then runs "$@".
macro_rules! then_run {
    () => {
        " gdbus wait --session --timeout 5 org.freedesktop.secrets && exec \"$@\""
    };
}
/// A name owner on the private bus that takes every call and never answers.
const WITH_HUNG_KEYRING: &str = concat!(
    "dbus-test-tool black-hole --session --name=org.freedesktop.secrets &",
    then_run!()
);
/// A name owner that answers every call with an empty reply.
const WITH_BROKEN_KEYRING: &str = concat!(
    "dbus-test-tool echo --session --name=org.freedesktop.secrets &",
    then_run!()
);
/// A keyring with only its in-memory "session" collection: no "default" or
/// "login" alias.
const WITH_EMPTY_KEYRING: &str = concat!(
    "gnome-keyring-daemon --start --components=secrets >/dev/null &&",
    then_run!()
);

#[test]
fn without_a_session_bus_secrets_are_owner_only_files() {
    let scratch = Scratch::new("files");
    run(scratch.child("files", None, Some("unix:path=/nonexistent")));
}

#[test]
fn an_unknown_secret_store_setting_is_an_error() {
    let scratch = Scratch::new("bad-store");
    let mut child = scratch.child("refused", None, Some("unix:path=/nonexistent"));
    child.env("ISOCAST_SECRET_STORE", "keyrings");
    child.env(
        "ISOCAST_KEYRING_TEST_EXPECT",
        "ISOCAST_SECRET_STORE is \"keyrings\"",
    );
    run(child);
}

#[test]
#[ignore = "starts dbus-run-session and gnome-keyring-daemon"]
fn with_secret_store_file_the_keyring_is_never_used() {
    let scratch = Scratch::new("file-store");
    let mut child = scratch.child("file-store", Some(WITH_KEYRING), None);
    child.env("ISOCAST_SECRET_STORE", "file");
    run(child);
}

#[test]
#[ignore = "starts dbus-run-session"]
fn with_a_bus_but_no_secret_service_secrets_are_files() {
    let scratch = Scratch::new("no-service");
    run(scratch.child_without_activation("files", "exec \"$@\""));
}

#[test]
#[ignore = "starts dbus-run-session and gnome-keyring-daemon"]
fn a_keyring_without_a_default_collection_is_an_error() {
    let scratch = Scratch::new("no-default");
    let mut child = scratch.child("refused", Some(WITH_EMPTY_KEYRING), None);
    child.env("ISOCAST_KEYRING_TEST_EXPECT", "has no default keyring");
    run(child);
}

#[test]
#[ignore = "starts dbus-run-session and dbus-test-tool"]
fn a_secret_service_that_answers_wrongly_is_an_error() {
    let scratch = Scratch::new("broken");
    let mut child = scratch.child("refused", Some(WITH_BROKEN_KEYRING), None);
    child.env("ISOCAST_KEYRING_TEST_EXPECT", "keyring:");
    run(child);
}

#[test]
#[ignore = "starts dbus-run-session and gnome-keyring-daemon"]
fn a_locked_keyring_is_an_error_without_a_prompt() {
    let scratch = Scratch::new("locked");
    run(scratch.child("locked", Some(WITH_KEYRING), None));
}

#[test]
#[ignore = "starts dbus-run-session and gnome-keyring-daemon"]
fn with_a_keyring_secrets_never_reach_a_file() {
    let scratch = Scratch::new("keyring");
    run(scratch.child("keyring", Some(WITH_KEYRING), None));
}

#[test]
#[ignore = "starts dbus-run-session and gnome-keyring-daemon"]
fn a_file_written_without_the_keyring_wins_over_its_older_item() {
    let scratch = Scratch::new("file-wins");
    run(scratch.child("keyring-old", Some(WITH_KEYRING), None));
    run(scratch.child("write-file", None, Some("unix:path=/nonexistent")));
    run(scratch.child("file-wins", Some(WITH_KEYRING), None));
}

/// A socket that takes connections and never says a word, as a wedged bus
/// does: the connection itself must be time-limited too.
#[test]
fn a_silent_session_bus_is_an_error_after_its_time_limit() {
    let scratch = Scratch::new("silent-bus");
    let socket = scratch.0.join("bus");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let address = format!("unix:path={}", socket.display());
    run(scratch.child("hung", None, Some(&address)));
}

#[test]
#[ignore = "starts dbus-run-session and gnome-keyring-daemon"]
fn a_secret_deleted_without_the_keyring_stays_deleted() {
    let scratch = Scratch::new("deleted-stays");
    run(scratch.child("keyring-old", Some(WITH_KEYRING), None));
    run(scratch.child("delete-file", None, Some("unix:path=/nonexistent")));
    run(scratch.child("deleted-stays", Some(WITH_KEYRING), None));
}

#[test]
#[ignore = "starts dbus-run-session and dbus-test-tool; takes about 5 s"]
fn a_file_is_read_even_while_the_keyring_hangs() {
    let scratch = Scratch::new("file-while-hung");
    run(scratch.child("write-file", None, Some("unix:path=/nonexistent")));
    run(scratch.child("file-while-hung", Some(WITH_HUNG_KEYRING), None));
}

#[test]
#[ignore = "starts dbus-run-session and dbus-test-tool; takes about 5 s"]
fn a_hung_keyring_is_an_error_after_its_time_limit() {
    let scratch = Scratch::new("hung");
    run(scratch.child("hung", Some(WITH_HUNG_KEYRING), None));
}
