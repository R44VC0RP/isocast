fn main() {
    // The `live-slint` feature (development only): Slint generates a thin
    // wrapper that compiles ui/app.slint at run time instead of the whole
    // interface as Rust.
    let live = std::env::var_os("CARGO_FEATURE_LIVE_SLINT").is_some();
    if live {
        // SAFETY: the build script is single-threaded so far.
        unsafe { std::env::set_var("SLINT_LIVE_PREVIEW", "1") };
    }
    slint_build::compile_with_config(
        "ui/app.slint",
        slint_build::CompilerConfiguration::new()
            .with_style("fluent-dark".into())
            // Element names and types, which `--a11y` (Slint's element
            // search API) needs.
            .with_debug_info(true),
    )
    .expect("the Slint interface compiles");
    if live {
        add_live_reload();
    }

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

/// Gives the live wrapper's `AppWindow` a `live_reload()`: read ui/ again
/// and put the new interface in the same window, keeping what the Rust
/// side set (`--snapshot-watch`). Slint's own reload runs from a file
/// watcher on an event loop the offscreen window doesn't have, and its
/// component is a private field, so the method goes into the generated
/// module. Development builds only; Slint is pinned, so the marker holds
/// until an upgrade, which then fails here, plainly.
fn add_live_reload() {
    let path = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("app.rs");
    let code = std::fs::read_to_string(&path).expect("Slint wrote app.rs");
    let marker = "impl slint :: StrongHandle for r#AppWindow {";
    assert!(
        code.contains(marker),
        "live-slint: Slint's generated code changed; update add_live_reload in build.rs"
    );
    let method = "impl r#AppWindow {
        /// Read ui/ again into the same window; false (and the errors on
        /// stderr) when it doesn't compile.
        pub fn live_reload(&self) -> bool {
            let ok = self.0.borrow_mut().reload();
            if ok {
                self.0.borrow().reload_properties_and_callbacks();
            }
            ok
        }
    }
    ";
    let code = code.replacen(marker, &format!("{method}{marker}"), 1);
    std::fs::write(&path, code).expect("app.rs is writable");
}
