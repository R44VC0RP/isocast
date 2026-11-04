//! OpenH264, loaded at run time on Linux for encoding and preview decoding.
//!
//! Cisco's patent licence for H.264 covers only the OpenH264 binary Cisco
//! builds and serves, downloaded by each user. Isocast compiles and ships
//! none. `install.sh --openh264` downloads only Cisco's pinned binary.
//!
//! [`api`] uses an explicit absolute `$ISOCAST_OPENH264` path first. Otherwise
//! it checks Cisco's per-user binary in [`install_dir`] against its pinned
//! SHA-256. If absent, the dynamic loader looks up `libopenh264.so.8`.
//! The override and system paths trust the user or distro to supply native
//! code: they check soname 8, version 2.6 or later in major 2, and encoder
//! creation. These checks establish compatibility, not provenance.
#![cfg(target_os = "linux")]

mod loaded_elf;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub use openh264;
pub use openh264::OpenH264API;

/// The OpenH264 release the `openh264` 0.9.9 crate's ABI expects.
pub const VERSION: &str = "2.6.0";
/// Overrides where the library is looked for (a path to the `.so`).
pub const ENV: &str = "ISOCAST_OPENH264";
const SONAME: &str = "libopenh264.so.8";

/// One of Cisco's builds: its file, where Cisco serves it (bzip2), and the
/// SHA-256 of the download and of the library inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Build {
    /// `std::env::consts::ARCH`.
    pub arch: &'static str,
    pub file: &'static str,
    pub url: &'static str,
    pub bz2_sha256: &'static str,
    pub so_sha256: &'static str,
}

/// Cisco's OpenH264 2.6.0 for Linux. packaging/linux/install.sh and the CI
/// jobs pin the same files and hashes.
pub const BUILDS: [Build; 2] = [
    Build {
        arch: "x86_64",
        file: "libopenh264-2.6.0-linux64.8.so",
        url: "https://ciscobinary.openh264.org/libopenh264-2.6.0-linux64.8.so.bz2",
        bz2_sha256: "27ab53323c110b76214c1c72222f459d17febbcd1e252136cadc292b0308d75b",
        so_sha256: "2f0cde7c6a6abcf5cae76942894ea42897fa677bce4ed6c91a24dd1b041d5f04",
    },
    Build {
        arch: "aarch64",
        file: "libopenh264-2.6.0-linux-arm64.8.so",
        url: "https://ciscobinary.openh264.org/libopenh264-2.6.0-linux-arm64.8.so.bz2",
        bz2_sha256: "a78aea7970150f46bcd3bb7994c9e6dd90bd7a9ea785920f5a73f6964e3fcda7",
        so_sha256: "12e7b33623667cdab0e575170c147b1b36eadb77d0d2aa7ceb5afd3e58902140",
    },
];

/// This machine's build, if Cisco makes one for it.
pub fn build() -> Option<&'static Build> {
    BUILDS.iter().find(|b| b.arch == std::env::consts::ARCH)
}

/// Why H.264 can't be used here, and the command that fixes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub reason: String,
    pub fix: String,
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}; fix: {}", self.reason, self.fix)
    }
}

impl std::error::Error for Unavailable {}

/// Where the installer puts Isocast and Cisco's library, per user.
pub fn install_dir(home: &Path) -> PathBuf {
    home.join(".local/lib/isocast")
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, PartialEq)]
enum Source {
    Override(PathBuf),
    Cisco(PathBuf, &'static Build),
    System,
}

fn locate(
    override_path: Option<PathBuf>,
    home: Option<PathBuf>,
    build: Option<&'static Build>,
    fix: &str,
) -> Result<Source, Unavailable> {
    if let Some(path) = override_path.filter(|path| !path.as_os_str().is_empty()) {
        if !path.is_absolute() {
            return Err(Unavailable {
                reason: format!(
                    "${ENV} is {}, a relative path; it must be the library's absolute path",
                    path.display()
                ),
                fix: format!("set ${ENV} to an absolute path, or unset it to use {fix}"),
            });
        }
        return Ok(Source::Override(path));
    }
    if let (Some(home), Some(build)) = (home, build) {
        let path = install_dir(&home).join(build.file);
        // Missing is a fallback; an installed but damaged Cisco copy is an
        // error, never an unchecked load of that file.
        if !matches!(std::fs::symlink_metadata(&path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(Source::Cisco(path, build));
        }
    }
    Ok(Source::System)
}

/// The command that (re)installs the library: the Debian package's
/// `isocast-openh264-install` beside the running program, else the
/// installer beside it (the unpacked download, or the installed copy),
/// else the installed copy's place.
pub fn fix_command() -> String {
    let dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    fix_command_in(dir.as_deref(), home())
}

fn fix_command_in(dir: Option<&Path>, home: Option<PathBuf>) -> String {
    if let Some(dir) = dir {
        let packaged = dir.join("isocast-openh264-install");
        if packaged.is_file() {
            return packaged.display().to_string();
        }
        let installer = dir.join("install.sh");
        if installer.is_file() {
            return format!("{} --openh264", installer.display());
        }
    }
    let home = home.unwrap_or_else(|| PathBuf::from("~"));
    format!(
        "{} --openh264",
        install_dir(&home).join("install.sh").display()
    )
}

/// Check `bytes` (read from `path`) are `build`'s library.
fn verify(path: &Path, bytes: &[u8], build: &Build) -> Result<(), String> {
    let digest = Sha256::digest(bytes);
    let got: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    if got == build.so_sha256 {
        return Ok(());
    }
    Err(format!(
        "{} is not Cisco's OpenH264 {VERSION} for {} (SHA-256 {got}, expected {})",
        path.display(),
        build.arch,
        build.so_sha256
    ))
}

/// The command that removes Cisco's per-user copy, from the one that
/// installs it.
fn remove_command(fix: &str) -> String {
    match fix.strip_suffix(" --openh264") {
        Some(installer) => format!("{installer} --remove-openh264"),
        None => format!("{fix} --remove"),
    }
}

/// `path` opened, read and checked through the one open file, which is
/// returned: loading it through that file (`/proc/self/fd/N`) loads the
/// very bytes checked, whatever replaces the path meanwhile.
fn open_verified(path: &Path, build: &Build, fix: &str) -> Result<File, Unavailable> {
    let unavailable = |reason: String| Unavailable {
        reason,
        fix: fix.to_string(),
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        // `locate` found something at the path: a link to a missing file,
        // or a file removed since. Isocast uses only this file while it
        // is there, never the system library instead.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let remove = remove_command(fix);
            return Err(Unavailable {
                reason: format!(
                    "Cisco's OpenH264 {VERSION} is missing from {} (a broken link?)",
                    path.display()
                ),
                fix: format!(
                    "{fix} to download it again, or {remove} to use your system's OpenH264"
                ),
            });
        }
        Err(error) => {
            return Err(unavailable(format!(
                "could not open {}: {error}",
                path.display()
            )));
        }
    };
    let mut bytes = Vec::new();
    if let Err(error) = file.read_to_end(&mut bytes) {
        return Err(unavailable(format!(
            "could not read {}: {error}",
            path.display()
        )));
    }
    verify(path, &bytes, build).map_err(|reason| Unavailable {
        reason,
        fix: format!(
            "{fix} to replace it, or {} to use the system library",
            remove_command(fix)
        ),
    })?;
    Ok(file)
}

fn cisco(path: &Path, build: &Build, fix: &str) -> Result<OpenH264API, Unavailable> {
    let file = open_verified(path, build, fix)?;
    // The file just hashed, by its descriptor, held open until loaded.
    let held = format!("/proc/self/fd/{}", file.as_raw_fd());
    // SAFETY: the bytes behind `held` are exactly Cisco's pinned OpenH264
    // 2.6.0 build for this architecture (checked above through this open
    // file), the release whose ABI the `openh264` 0.9.9 bindings were
    // generated for (and on the crate's own list of Cisco builds).
    let api = unsafe { OpenH264API::from_blob_path_unchecked(&held) };
    drop(file);
    // The loader's message names the descriptor's path: name the file.
    api.map_err(|error| Unavailable {
        reason: format!(
            "could not load {}: {}",
            path.display(),
            error
                .to_string()
                .replace(&held, &path.display().to_string())
        ),
        fix: fix.to_owned(),
    })
}

fn compatible(major: u32, minor: u32, revision: u32) -> Result<String, String> {
    let version = format!("{major}.{minor}.{revision}");
    if major != 2 || minor < 6 {
        return Err(format!(
            "OpenH264 {version} is incompatible; need version 2.6 or later in major 2"
        ));
    }
    Ok(version)
}

fn supplied(path: &Path, provider: &str) -> Result<(OpenH264API, String), String> {
    // SAFETY: the loader selects trusted native code from the distro's search
    // path or the user's explicit override. This constructor only resolves
    // symbols. Check the mapped ABI before calling any codec functions.
    let api = unsafe { OpenH264API::from_blob_path_unchecked(path) }.map_err(|e| {
        // dlopen supplies text, not an errno. Only summarize a missing soname
        // itself (glibc/musl); a missing dependency or another loader error is
        // a rejection whose explanation belongs in the wizard too.
        let loader = e.to_string();
        let absent = path == Path::new(SONAME)
            && (loader
                == format!("{SONAME}: cannot open shared object file: No such file or directory")
                || loader
                    == format!("Error loading shared library {SONAME}: No such file or directory"));
        let summary = if absent { "No OpenH264 found.\n" } else { "" };
        format!("{summary}could not load {}: {loader}", path.display())
    })?;
    let OpenH264API::Libloading(symbols) = &api;
    let version = symbols
        .WelsGetCodecVersion
        .as_ref()
        .map_err(ToString::to_string)?;
    let (loaded, soname) = loaded_elf::identify(*version as usize)
        .ok_or("cannot identify the loaded OpenH264 library")?;
    if soname.as_deref() != Some(SONAME) {
        return Err(format!(
            "{} has soname {}; need {SONAME}",
            loaded.display(),
            soname.as_deref().unwrap_or("(none)")
        ));
    }
    // SAFETY: the ABI-8 version function returns four unsigned integers.
    let v = unsafe { version() };
    let version = compatible(v.uMajor, v.uMinor, v.uRevision)?;
    // bindgen stores missing symbols as Results and panics if invoked. Check
    // the entry points the encoder and decoder need before exposing the API.
    let create = symbols
        .WelsCreateSVCEncoder
        .as_ref()
        .map_err(ToString::to_string)?;
    let destroy = symbols
        .WelsDestroySVCEncoder
        .as_ref()
        .map_err(ToString::to_string)?;
    symbols
        .WelsCreateDecoder
        .as_ref()
        .map_err(ToString::to_string)?;
    symbols
        .WelsDestroyDecoder
        .as_ref()
        .map_err(ToString::to_string)?;
    let mut encoder = std::ptr::null_mut();
    // SAFETY: the verified ABI matches these bindings; no vtable is used
    // until a real encoder exists (noopenh264 cannot create one).
    let rc = unsafe { create(&raw mut encoder) };
    if rc != 0 || encoder.is_null() {
        return Err(format!(
            "OpenH264 {version} cannot create an encoder (code {rc}); it may be a stub such as noopenh264"
        ));
    }
    // SAFETY: successful creation transfers ownership to the matching destroy.
    unsafe { destroy(encoder) };
    Ok((
        api,
        format!("{provider} OpenH264 {version} ({})", loaded.display()),
    ))
}

fn load() -> Result<(OpenH264API, String), Unavailable> {
    let fix = if build().is_some() {
        fix_command()
    } else {
        format!("install your distro's OpenH264 2.x (2.6 or later, {SONAME})")
    };
    match locate(std::env::var_os(ENV).map(Into::into), home(), build(), &fix)? {
        Source::Cisco(path, build) => Ok((
            cisco(&path, build, &fix)?,
            format!("Cisco's OpenH264 {VERSION} ({})", path.display()),
        )),
        Source::Override(path) => {
            supplied(&path, "$ISOCAST_OPENH264:").map_err(|why| Unavailable {
                reason: format!("${ENV} ({}): {why}", path.display()),
                fix: format!("correct or unset ${ENV} to use the installed library"),
            })
        }
        Source::System => supplied(Path::new(SONAME), "your system's").map_err(|why| Unavailable {
            reason: format!("{why} (system {SONAME}; no per-user Cisco download)"),
            fix,
        }),
    }
}

/// Human-readable provider, version and path of the usable codec.
pub fn check() -> Result<String, Unavailable> {
    load().map(|(_, status)| status)
}

/// The same checked library for encoding or preview decoding. Look again on
/// each call so an installation while Isocast is running takes effect.
pub fn api() -> Result<OpenH264API, Unavailable> {
    load().map(|(api, _)| api)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires the per-user Cisco download; run by both Linux app CI architectures"]
    fn supplied_accepts_ciscos_abi_8_library() {
        let path = install_dir(&home().unwrap()).join(build().unwrap().file);
        let (api, status) = supplied(&path, "test").unwrap();
        assert_eq!(status, format!("test OpenH264 2.6.0 ({})", path.display()));
        let OpenH264API::Libloading(symbols) = api;
        let version = symbols.WelsGetCodecVersion.as_ref().unwrap();
        let (_, soname) = loaded_elf::identify(*version as usize).unwrap();
        assert_eq!(soname.as_deref(), Some("libopenh264.so.8"));
    }

    #[cfg(target_env = "gnu")]
    #[test]
    fn identifies_the_libc_containing_getpid() {
        let (_, soname) = loaded_elf::identify(libc::getpid as *const () as usize).unwrap();
        assert_eq!(soname.as_deref(), Some("libc.so.6"));
    }

    #[test]
    fn builds_are_cisco_2_6_0_for_both_architectures() {
        for build in BUILDS {
            assert!(build.file.starts_with("libopenh264-2.6.0-linux"));
            assert_eq!(
                build.url,
                format!("https://ciscobinary.openh264.org/{}.bz2", build.file)
            );
            assert_eq!(build.so_sha256.len(), 64);
            assert_eq!(build.bz2_sha256.len(), 64);
        }
        assert!(BUILDS.iter().any(|b| b.arch == "x86_64"));
        assert!(BUILDS.iter().any(|b| b.arch == "aarch64"));
    }

    /// The installer downloads exactly what is pinned here.
    #[test]
    fn installer_pins_the_same_files_and_hashes() {
        let installer = include_str!("../../../packaging/linux/install.sh");
        assert!(installer.contains(&format!("openh264_version={VERSION}")));
        for build in BUILDS {
            for pin in [
                format!("openh264_file={}", build.file),
                format!("openh264_bz2_sha256={}", build.bz2_sha256),
                format!("openh264_sha256={}", build.so_sha256),
            ] {
                assert!(installer.contains(&pin), "install.sh lacks {pin}");
            }
        }
        assert!(
            installer.contains("openh264_url=https://ciscobinary.openh264.org/$openh264_file.bz2")
        );
        // Every OpenH264 cache in CI names the same version (key and path).
        let ci = include_str!("../../../.github/workflows/windows.yml");
        let caches = ci.matches("key: openh264-").count();
        assert!(caches >= 2, "windows.yml has {caches} OpenH264 caches");
        for pin in [
            format!("key: openh264-{VERSION}-${{{{ runner.arch }}}}"),
            format!("path: ~/.local/lib/isocast/libopenh264-{VERSION}-*.so"),
        ] {
            assert_eq!(
                ci.matches(&pin).count(),
                caches,
                "a windows.yml OpenH264 cache lacks {pin}"
            );
        }
    }

    #[test]
    fn prefers_an_explicit_override_then_cisco_then_the_system() {
        let dir = scratch("lookup");
        let home = Some(dir.clone());
        let build = &BUILDS[1];
        let at = |env: Option<&str>, home: Option<PathBuf>| {
            locate(env.map(PathBuf::from), home, Some(build), "FIX")
        };
        assert_eq!(at(None, home.clone()).unwrap(), Source::System);
        let path = install_dir(&dir).join(build.file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "present but not Cisco's library").unwrap();
        assert_eq!(at(None, home.clone()).unwrap(), Source::Cisco(path, build));
        assert_eq!(
            at(Some(""), home.clone()).unwrap(),
            at(None, home.clone()).unwrap()
        );
        assert_eq!(
            at(Some("/ci/libopenh264.so"), home.clone()).unwrap(),
            Source::Override("/ci/libopenh264.so".into())
        );
        let relative = at(Some("lib/libopenh264.so"), home.clone()).unwrap_err();
        assert!(relative.reason.contains("relative path"), "{relative}");
        assert_eq!(at(None, None).unwrap(), Source::System);
        assert_eq!(locate(None, home, None, "FIX").unwrap(), Source::System);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn interprets_the_version_returned_by_the_codec() {
        assert_eq!(compatible(2, 6, 0).unwrap(), "2.6.0");
        assert_eq!(compatible(2, 7, 12).unwrap(), "2.7.12");
        for (major, minor) in [(1, 7), (2, 4), (2, 5), (3, 0)] {
            assert!(
                compatible(major, minor, 1)
                    .unwrap_err()
                    .contains(&format!("{major}.{minor}.1"))
            );
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("isocast-openh264-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_fix_is_the_packaged_command_or_the_installer_beside_the_program() {
        let dir = scratch("fix");
        let home = Some(PathBuf::from("/home/me"));
        assert_eq!(
            fix_command_in(Some(&dir), home.clone()),
            "/home/me/.local/lib/isocast/install.sh --openh264"
        );
        std::fs::write(dir.join("install.sh"), "").unwrap();
        assert_eq!(
            fix_command_in(Some(&dir), home.clone()),
            format!("{} --openh264", dir.join("install.sh").display())
        );
        std::fs::write(dir.join("isocast-openh264-install"), "").unwrap();
        assert_eq!(
            fix_command_in(Some(&dir), home),
            dir.join("isocast-openh264-install").display().to_string()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_unreadable_and_wrong_files_name_the_path_and_fix() {
        let build = &BUILDS[0];
        let dir = scratch("files");
        let reason = |path: &Path| open_verified(path, build, "FIX").unwrap_err();
        let missing = reason(&dir.join("absent.so"));
        assert!(
            missing.reason.contains("is missing from") && missing.reason.contains("absent.so"),
            "{missing}"
        );
        assert_eq!(
            missing.fix,
            "FIX to download it again, or FIX --remove to use your system's OpenH264"
        );
        // A folder opens but can't be read.
        let unreadable = reason(&dir);
        assert!(
            unreadable.reason.starts_with("could not read"),
            "{unreadable}"
        );
        let wrong = dir.join("wrong.so");
        std::fs::write(&wrong, b"not a library").unwrap();
        let wrong = open_verified(&wrong, build, "/usr/bin/isocast-openh264-install").unwrap_err();
        assert!(
            wrong
                .fix
                .contains("/usr/bin/isocast-openh264-install --remove")
        );
        assert!(
            wrong.reason.contains("is not Cisco's OpenH264 2.6.0"),
            "{wrong}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_anything_but_the_pinned_build() {
        let build = &BUILDS[0];
        let error = verify(Path::new("/x/lib.so"), b"not a library", build).unwrap_err();
        assert!(
            error.contains("/x/lib.so is not Cisco's OpenH264 2.6.0 for x86_64"),
            "{error}"
        );
        assert!(error.contains(build.so_sha256), "{error}");
    }

    #[test]
    fn unavailable_names_the_fix() {
        let error = Unavailable {
            reason: "Cisco's OpenH264 2.6.0 is missing from /p (a broken link?)".into(),
            fix: "/i/install.sh --openh264".into(),
        };
        assert_eq!(
            error.to_string(),
            "Cisco's OpenH264 2.6.0 is missing from /p (a broken link?); fix: /i/install.sh --openh264"
        );
        assert!(fix_command().ends_with("install.sh --openh264"));
    }

    /// With the library installed (or `$ISOCAST_OPENH264` set, as CI
    /// does), it loads and encodes; without it, says how to fix that.
    #[test]
    fn loads_the_installed_library_or_says_how_to_install_it() {
        match api() {
            Ok(api) => {
                let mut encoder = openh264::encoder::Encoder::with_api_config(
                    api,
                    openh264::encoder::EncoderConfig::new(),
                )
                .unwrap();
                let frame = openh264::formats::YUVBuffer::new(64, 64);
                assert!(!encoder.encode(&frame).unwrap().to_vec().is_empty());
            }
            Err(error) => {
                assert!(
                    error.fix.contains("openh264") || error.fix.contains(ENV),
                    "{error}"
                );
                if std::env::var_os("ISOCAST_OPENH264_REQUIRED").is_some() {
                    panic!("{error}");
                }
            }
        }
    }
}
