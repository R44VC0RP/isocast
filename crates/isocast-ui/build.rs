fn main() {
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .with_style("fluent-dark".into())
            // Element names and types, which `--a11y` (Slint's element
            // search API) needs.
            .with_debug_info(true),
    )
    .expect("the Slint interface compiles");

    // On macOS, an Info.plist linked into the binary names the app
    // ("Isocast" in the menu bar's app menu and the Dock) when it runs
    // outside a bundle, as `cargo run` does.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let plist =
            std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("Info.plist");
        println!("cargo:rerun-if-changed={}", plist.display());
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }
}
