//! Embed the privacy-attribution plist and immutable build provenance.
fn main() {
    // Keep the embedded plist: permission attribution must survive this hook.
    let plist = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("Info.plist");
    println!("cargo:rerun-if-changed={}", plist.display());
    println!("cargo:rerun-if-env-changed=ISOCAST_BUILD_VERSION");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let version = std::env::var("ISOCAST_BUILD_VERSION").unwrap_or_else(|_| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                .to_string()
        });
        assert!(
            !version.is_empty() && version.bytes().all(|c| c.is_ascii_digit()),
            "ISOCAST_BUILD_VERSION must be numeric"
        );
        println!("cargo:rustc-env=ISOCAST_BUILD_NUMBER={version}");
        let generated = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
            .join("isocastd-info.plist");
        // Preserve camera-extension build numbering while deriving the product
        // version from Cargo, as the distribution and dev bundles do.
        let content = std::fs::read_to_string(&plist)
            .unwrap()
            .replace(
                "<key>CFBundleVersion</key><string>1</string>",
                &format!("<key>CFBundleVersion</key><string>{version}</string>"),
            )
            .replace(
                "</dict>",
                &format!(
                    "<key>CFBundleShortVersionString</key><string>{}</string></dict>",
                    env!("CARGO_PKG_VERSION")
                ),
            );
        std::fs::write(&generated, content).unwrap();
        println!(
            "cargo:rustc-link-arg-bin=isocastd=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            generated.display()
        );
    }
    let commit = std::env::var("ISOCAST_BUILD_COMMIT")
        .ok()
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| {
                    let mut commit = String::from_utf8_lossy(&o.stdout).trim().to_owned();
                    if std::process::Command::new("git")
                        .args(["status", "--porcelain", "--untracked-files=no"])
                        .output()
                        .is_ok_and(|s| s.status.success() && !s.stdout.is_empty())
                    {
                        commit.push_str("-dirty");
                    }
                    commit
                })
        })
        .unwrap_or_else(|| "unknown (source archive)".into());
    println!("cargo:rustc-env=ISOCAST_BUILD_COMMIT={commit}");
    println!(
        "cargo:rustc-env=ISOCAST_BUILD_PROFILE={}",
        std::env::var("PROFILE").unwrap_or_default()
    );
    println!("cargo:rerun-if-env-changed=ISOCAST_BUILD_COMMIT");
    // In worktrees .git is a file; resolve the actual HEAD/ref paths.
    let branch = std::process::Command::new("git")
        .args(["symbolic-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    for item in ["HEAD", branch.as_str()] {
        if let Ok(out) = std::process::Command::new("git")
            .args(["rev-parse", "--git-path", item])
            .output()
            && out.status.success()
        {
            println!(
                "cargo:rerun-if-changed={}",
                String::from_utf8_lossy(&out.stdout).trim()
            );
        }
    }
}
