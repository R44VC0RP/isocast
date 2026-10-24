//! Cisco's OpenH264, loaded at run time on Linux.
//!
//! Cisco's patent licence for H.264 covers only the OpenH264 binary Cisco
//! builds and serves, downloaded by each user. So Isocast compiles none:
//! its Linux installer (`install.sh --openh264`) downloads Cisco's binary
//! into [`install_dir`], and the engine (encoding) and the app (preview
//! decoding) load it from there through [`api`], after checking it is
//! exactly the build pinned here (the version whose ABI the `openh264`
//! crate was generated for). `$ISOCAST_OPENH264` points elsewhere, for
//! development and CI.
//!
//! When it is missing or not that build, [`api`] says so, naming the path
//! and the one command that fixes it; nothing else stops working.
#![cfg(target_os = "linux")]

use std::ffi::OsString;
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
/// Cisco's licence for its binary.
pub const LICENSE_URL: &str = "https://www.openh264.org/BINARY_LICENSE.txt";

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

/// Where the library is looked for: `override_path` (`$ISOCAST_OPENH264`)
/// when set and not empty, which must be absolute (it is hashed and loaded
/// by that path, whatever the working directory); else `build.file` in
/// [`install_dir`] under `home`. The `bool` says whether it was overridden.
fn locate(
    override_path: Option<OsString>,
    home: Option<PathBuf>,
    build: &Build,
    fix: &str,
) -> Result<(PathBuf, bool), Unavailable> {
    if let Some(path) = override_path.filter(|path| !path.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(Unavailable {
                reason: format!(
                    "${ENV} is {}, a relative path; it must be the library's absolute path",
                    path.display()
                ),
                fix: format!("set ${ENV} to an absolute path, or unset it to use {fix}"),
            });
        }
        return Ok((path, true));
    }
    let Some(home) = home else {
        return Err(Unavailable {
            reason: format!(
                "HOME isn't set, so Cisco's OpenH264 {VERSION} in ~/.local/lib/isocast can't be found"
            ),
            fix: format!("set HOME, or set ${ENV} to the library's absolute path"),
        });
    };
    Ok((install_dir(&home).join(build.file), false))
}

/// The command that (re)installs the library: the installer beside the
/// running program (the unpacked download, or the installed copy), else
/// the installed copy's place.
pub fn fix_command() -> String {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("install.sh")))
        .filter(|installer| installer.is_file());
    let installer = beside.unwrap_or_else(|| {
        install_dir(&home().unwrap_or_else(|| PathBuf::from("~"))).join("install.sh")
    });
    format!("{} --openh264", installer.display())
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

/// `path` opened, read and checked through the one open file, which is
/// returned: loading it through that file (`/proc/self/fd/N`) loads the
/// very bytes checked, whatever replaces the path meanwhile.
fn open_verified(path: &Path, from: &str, build: &Build, fix: &str) -> Result<File, Unavailable> {
    let unavailable = |reason: String| Unavailable {
        reason: format!("{reason}{from}"),
        fix: fix.to_string(),
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(unavailable(format!(
                "H.264 needs Cisco's OpenH264 {VERSION}, which isn't installed at {}",
                path.display()
            )));
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
    verify(path, &bytes, build).map_err(unavailable)?;
    Ok(file)
}

/// This machine's build and the library's path, opened and checked.
fn open() -> Result<(PathBuf, File), Unavailable> {
    let fix = fix_command();
    let Some(build) = build() else {
        return Err(Unavailable {
            reason: format!(
                "H.264 needs Cisco's OpenH264 {VERSION}, which Cisco doesn't build for {}",
                std::env::consts::ARCH
            ),
            fix: "none on this computer: H.264 needs an x86_64 or arm64 (aarch64) one".into(),
        });
    };
    let (path, overridden) = locate(std::env::var_os(ENV), home(), build, &fix)?;
    let from = if overridden {
        format!(" (from ${ENV})")
    } else {
        String::new()
    };
    let file = open_verified(&path, &from, build, &fix)?;
    Ok((path, file))
}

/// Where the library is, once it is there and is the pinned build.
pub fn check() -> Result<PathBuf, Unavailable> {
    open().map(|(path, _)| path)
}

/// Cisco's OpenH264, loaded: for `Encoder::with_api_config` and
/// `Decoder::with_api_config`. Checked again at each call, so installing
/// the library while Isocast runs is picked up by the next encoder.
pub fn api() -> Result<OpenH264API, Unavailable> {
    let (path, file) = open()?;
    // The file just hashed, by its descriptor, held open until loaded.
    let held = format!("/proc/self/fd/{}", file.as_raw_fd());
    // SAFETY: the bytes behind `held` are exactly Cisco's pinned OpenH264
    // 2.6.0 build for this architecture (checked above through this open
    // file), the release whose ABI the `openh264` 0.9.9 bindings were
    // generated for (and on the crate's own list of Cisco builds).
    let api = unsafe { OpenH264API::from_blob_path_unchecked(&held) };
    drop(file);
    api.map_err(|error| Unavailable {
        reason: format!("could not load {}: {error}", path.display()),
        fix: fix_command(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn looks_in_the_install_folder_unless_overridden() {
        let home = Some(PathBuf::from("/home/me"));
        let build = &BUILDS[1];
        let at = |env: Option<&str>, home: Option<PathBuf>| {
            locate(env.map(OsString::from), home, build, "FIX")
        };
        assert_eq!(
            at(None, home.clone()).unwrap(),
            (
                PathBuf::from("/home/me/.local/lib/isocast/libopenh264-2.6.0-linux-arm64.8.so"),
                false
            )
        );
        assert_eq!(
            at(Some(""), home.clone()).unwrap(),
            at(None, home.clone()).unwrap()
        );
        assert_eq!(
            at(Some("/ci/libopenh264.so"), home.clone()).unwrap(),
            (PathBuf::from("/ci/libopenh264.so"), true)
        );
        // Relative: hashed here but loaded elsewhere, so refused.
        let relative = at(Some("lib/libopenh264.so"), home.clone()).unwrap_err();
        assert!(relative.reason.contains("relative path"), "{relative}");
        // No HOME: no install folder, said so.
        let homeless = at(None, None).unwrap_err();
        assert!(homeless.reason.contains("HOME isn't set"), "{homeless}");
        assert_eq!(
            at(Some("/ci/l.so"), None).unwrap().0,
            PathBuf::from("/ci/l.so")
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("isocast-openh264-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_unreadable_and_wrong_files_name_the_path_and_fix() {
        let build = &BUILDS[0];
        let dir = scratch("files");
        let reason = |path: &Path| open_verified(path, "", build, "FIX").unwrap_err();
        let missing = reason(&dir.join("absent.so"));
        assert!(
            missing.reason.contains("isn't installed at") && missing.reason.contains("absent.so"),
            "{missing}"
        );
        assert_eq!(missing.fix, "FIX");
        // A folder opens but can't be read.
        let unreadable = reason(&dir);
        assert!(
            unreadable.reason.starts_with("could not read"),
            "{unreadable}"
        );
        let wrong = dir.join("wrong.so");
        std::fs::write(&wrong, b"not a library").unwrap();
        let wrong = open_verified(&wrong, " (from $ISOCAST_OPENH264)", build, "FIX").unwrap_err();
        assert!(
            wrong.reason.contains("is not Cisco's OpenH264 2.6.0"),
            "{wrong}"
        );
        assert!(
            wrong.reason.ends_with("(from $ISOCAST_OPENH264)"),
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
            reason: "H.264 needs Cisco's OpenH264 2.6.0, which isn't installed at /p".into(),
            fix: "/i/install.sh --openh264".into(),
        };
        assert_eq!(
            error.to_string(),
            "H.264 needs Cisco's OpenH264 2.6.0, which isn't installed at /p; fix: /i/install.sh --openh264"
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
                    error.fix.ends_with("--openh264") || error.fix.contains("x86_64"),
                    "{error}"
                );
                if std::env::var_os("ISOCAST_OPENH264_REQUIRED").is_some() {
                    panic!("{error}");
                }
            }
        }
    }
}
