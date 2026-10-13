//! Embed Info.plist in the isocastd binary, so macOS can attribute privacy
//! permissions (camera, microphone, screen recording) to isocastd itself.

fn main() {
    let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
    println!("cargo:rerun-if-changed={}", plist.display());
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!(
            "cargo:rustc-link-arg-bin=isocastd=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
