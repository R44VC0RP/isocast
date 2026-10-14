//! Embed Info.plist in the isocastd binary, so macOS can attribute privacy
//! permissions (camera, microphone, screen recording) to isocastd itself.

fn main() {
    let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
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
        let generated = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap())
            .join("isocastd-info.plist");
        let content = std::fs::read_to_string(&plist).unwrap().replace(
            "<key>CFBundleVersion</key><string>1</string>",
            &format!("<key>CFBundleVersion</key><string>{version}</string>"),
        );
        std::fs::write(&generated, content).unwrap();
        println!(
            "cargo:rustc-link-arg-bin=isocastd=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            generated.display()
        );
    }
}
