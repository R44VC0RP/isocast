//! `hello`: what an engine says about itself, and the version rule
//! (docs/remote-engines.md, decisions 7 and 20).
//!
//! `hello` is an ordinary method: `isocast-client` calls it after a remote
//! connect and caches the answer; local clients may call it but don't have
//! to. Its shape, and [`compatibility`], must never change across versions,
//! so any two builds can tell each other what's wrong.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The protocol family's version. Equal builds speak the same method
/// table; this changes only if `hello`, pairing or the mismatch error ever
/// have to (they're meant never to).
pub const PROTOCOL: u32 = 1;

/// [`Method::Hello`](crate::api::Method::Hello)'s answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Hello {
    /// The engine machine's operating system: its words (Finder or
    /// Explorer, Keychain or Credential Manager) follow this, not the
    /// app's.
    pub os: EngineOs,
    /// The engine's identity (the SHA-256 of its public key), once it has
    /// one for remote connections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_id: Option<String>,
    /// The engine machine's name, as people know it ("exon-desk").
    pub name: String,
    /// Which build answered: only what the version rule compares (no
    /// paths or process ids).
    pub build: BuildIdentity,
    /// [`PROTOCOL`].
    pub protocol: u32,
    /// Every method this engine offers, as requests spell them.
    pub capabilities: Vec<String>,
}

/// An engine machine's operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EngineOs {
    Macos,
    Windows,
    Linux,
    /// One this client doesn't know (from a newer engine).
    #[serde(other)]
    Unknown,
}

impl EngineOs {
    /// The operating system this was built for.
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Unknown
        }
    }
}

/// How a build was made, from the build's own metadata (never guessed from
/// the shape of its build number): `release` when the release pipeline
/// gave it a release build number (macOS: `ISOCAST_BUILD_VERSION`, set by
/// scripts/release.sh; Windows: `ISOCAST_WINDOWS_BUILD_VERSION`, set by the
/// packaging workflow), `dev` otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Release,
    Dev,
    /// One this client doesn't know (from a newer engine).
    #[serde(other)]
    Unknown,
}

impl Provenance {
    /// From the build-time variables a crate's build script passes on
    /// (`ISOCAST_BUILD_PROVENANCE`, `release` or anything else).
    pub fn from_build(value: Option<&str>) -> Self {
        match value {
            Some("release") => Self::Release,
            _ => Self::Dev,
        }
    }
}

/// A build's identity, as `hello` gives it and [`compatibility`] compares
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BuildIdentity {
    /// The product version (`0.0.9`).
    pub version: String,
    /// The source commit it was built from (`-dirty` if from uncommitted
    /// changes; `unknown …` when the build couldn't tell). OS packaging
    /// build numbers differ by platform and aren't part of the identity
    /// (decision 7).
    pub commit: String,
    pub provenance: Provenance,
}

/// Whether a client and an engine may work together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compatibility {
    /// The same version and commit.
    Same,
    /// The same version from different commits, with a development build
    /// on either side: allowed, with this warning.
    CommitDiffers(String),
    /// A different version, or two release builds from different (or
    /// unknown) commits: refused (only `hello`, a read-only
    /// `status` and asking the engine to update work), with this message.
    Mismatch(String),
}

impl BuildIdentity {
    fn describe(&self) -> String {
        match self.provenance {
            Provenance::Release => format!("{} (commit {})", self.version, self.commit),
            _ => format!(
                "{} (development build, commit {})",
                self.version, self.commit
            ),
        }
    }

    /// The commit is known (a source archive's build can't tell).
    fn commit_known(&self) -> bool {
        !self.commit.is_empty() && !self.commit.starts_with("unknown")
    }
}

/// The version rule (decision 7): the same product `version` and source
/// `commit` on both sides, whatever the operating systems. When either side
/// is a development build, a different commit is only a warning. Two
/// release builds whose commit is unknown can't be told apart, so they
/// don't match. The one place this is decided.
pub fn compatibility(client: &BuildIdentity, engine: &BuildIdentity) -> Compatibility {
    let mismatch = || {
        Compatibility::Mismatch(format!(
            "this Isocast is {} and the engine is {}: they must be the same version; update the \
             older one",
            client.describe(),
            engine.describe()
        ))
    };
    if client.version != engine.version {
        return mismatch();
    }
    let known = |p: Provenance| p != Provenance::Unknown;
    match (client.provenance, engine.provenance) {
        (Provenance::Release, Provenance::Release) => {
            if client.commit == engine.commit && client.commit_known() {
                Compatibility::Same
            } else {
                mismatch()
            }
        }
        (a, b) if known(a) && known(b) => {
            if client.commit == engine.commit {
                Compatibility::Same
            } else {
                Compatibility::CommitDiffers(format!(
                    "this Isocast ({}) and the engine ({}) are {} from different commits; they \
                     may not understand each other",
                    client.describe(),
                    engine.describe(),
                    client.version
                ))
            }
        }
        _ => mismatch(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(version: &str, commit: &str, provenance: Provenance) -> BuildIdentity {
        BuildIdentity {
            version: version.into(),
            commit: commit.into(),
            provenance,
        }
    }

    #[test]
    fn version_and_commit_decide() {
        use Provenance::{Dev, Release};
        // A macOS and a Windows release of the same commit match: OS build
        // numbers aren't compared.
        let release = id("0.0.9", "abc123", Release);
        assert_eq!(
            compatibility(&release, &release.clone()),
            Compatibility::Same
        );
        assert!(matches!(
            compatibility(&release, &id("0.0.9", "def456", Release)),
            Compatibility::Mismatch(_)
        ));
        assert!(matches!(
            compatibility(&release, &id("0.0.8", "abc123", Release)),
            Compatibility::Mismatch(_)
        ));
        let archive = id("0.0.9", "unknown (source archive)", Release);
        assert!(matches!(
            compatibility(&archive, &archive.clone()),
            Compatibility::Mismatch(_)
        ));
        // A dev build on either side: a commit difference is a warning.
        let dev = id("0.0.9", "def456-dirty", Dev);
        assert!(matches!(
            compatibility(&dev, &release),
            Compatibility::CommitDiffers(_)
        ));
        assert!(matches!(
            compatibility(&release, &dev),
            Compatibility::CommitDiffers(_)
        ));
        assert_eq!(compatibility(&dev, &dev.clone()), Compatibility::Same);
        assert!(matches!(
            compatibility(&dev, &id("0.0.8", "def456-dirty", Dev)),
            Compatibility::Mismatch(_)
        ));
    }

    #[test]
    fn unknown_systems_still_read() {
        assert_eq!(
            serde_json::from_str::<EngineOs>("\"freebsd\"").unwrap(),
            EngineOs::Unknown
        );
        assert_eq!(
            serde_json::to_string(&EngineOs::Linux).unwrap(),
            "\"linux\""
        );
    }
}
