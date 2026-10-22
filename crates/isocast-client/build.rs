//! Immutable client provenance for the remote hello compatibility check.
fn main() {
    println!("cargo:rerun-if-env-changed=ISOCAST_BUILD_COMMIT");
    println!("cargo:rerun-if-env-changed=ISOCAST_BUILD_VERSION");
    println!("cargo:rerun-if-env-changed=ISOCAST_WINDOWS_BUILD_VERSION");
    let commit = std::env::var("ISOCAST_BUILD_COMMIT")
        .ok()
        .or_else(|| {
            let output = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            let mut commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if std::process::Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=no"])
                .output()
                .is_ok_and(|status| status.status.success() && !status.stdout.is_empty())
            {
                commit.push_str("-dirty");
            }
            Some(commit)
        })
        .unwrap_or_else(|| "unknown (source archive)".into());
    println!("cargo:rustc-env=ISOCAST_CLIENT_COMMIT={commit}");
    // The same release-pipeline markers as the daemon's build script. A
    // numeric macOS development timestamp is not a release provenance.
    let marker = match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => "ISOCAST_BUILD_VERSION",
        Ok("windows") => "ISOCAST_WINDOWS_BUILD_VERSION",
        _ => "",
    };
    let provenance = if std::env::var(marker).is_ok_and(|value| !value.trim().is_empty()) {
        "release"
    } else {
        "dev"
    };
    println!("cargo:rustc-env=ISOCAST_CLIENT_PROVENANCE={provenance}");
    let branch = std::process::Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();
    for item in ["HEAD", branch.as_str()] {
        if let Ok(output) = std::process::Command::new("git")
            .args(["rev-parse", "--git-path", item])
            .output()
            && output.status.success()
        {
            println!(
                "cargo:rerun-if-changed={}",
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
    }
}
