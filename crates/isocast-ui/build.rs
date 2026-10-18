fn main() {
    // The `live-slint` feature (development only): Slint generates a thin
    // wrapper that compiles ui/app.slint at run time instead of the whole
    // interface as Rust.
    let live = std::env::var_os("CARGO_FEATURE_LIVE_SLINT").is_some();
    if live {
        // SAFETY: the build script is single-threaded so far.
        unsafe { std::env::set_var("SLINT_LIVE_PREVIEW", "1") };
    }
    // The Windows build-script main thread has a 1 MiB stack. Compiling this
    // interface overflows it; use the same 8 MiB budget as macOS's main thread.
    // This is a host/build-time setting, not an app or renderer fallback.
    #[cfg(windows)]
    std::thread::Builder::new()
        .name("Slint compiler".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(compile_interface)
        .expect("the Slint compiler thread starts")
        .join()
        .expect("the Slint interface compiles");
    #[cfg(not(windows))]
    compile_interface();
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

/// `ISOCAST_UI_NO_GALLERY=1` builds the interface without the kit gallery
/// (`--state kit`, a design board of 1832 lines): rustc then needs about
/// 3 GiB less of the roughly 13.5 GiB the one generated module takes, so
/// GitHub's 16 GB Windows runner can compile it (.github/workflows/
/// windows.yml). Only that workflow sets it; every other build, and every
/// shipped one, has the board.
const NO_GALLERY: &str = "ISOCAST_UI_NO_GALLERY";

/// The interface to compile: ui/app.slint, or with `NO_GALLERY` a copy of
/// ui/ in OUT_DIR whose kit-gallery.slint is an empty `KitGallery` with the
/// same properties (Slint resolves imports next to the importing file, so
/// a copy is the only way to swap it without touching the sources).
fn interface_root() -> std::path::PathBuf {
    println!("cargo:rerun-if-env-changed={NO_GALLERY}");
    if std::env::var_os(NO_GALLERY).is_none() {
        return "ui/app.slint".into();
    }
    // Slint reports the copy's files as inputs; the sources are what change.
    println!("cargo:rerun-if-changed=ui");
    let out = std::path::Path::new(&std::env::var_os("OUT_DIR").unwrap()).join("ui-no-gallery");
    // A file the sources no longer have must not survive in the copy.
    let _ = std::fs::remove_dir_all(&out);
    copy_dir("ui".as_ref(), &out);
    std::fs::write(
        out.join("kit-gallery.slint"),
        "export component KitGallery inherits Rectangle {\n    \
         in property <bool> show-dialog;\n    \
         in-out property <int> open-dropdown;\n}\n",
    )
    .expect("the stub gallery is writable");
    out.join("app.slint")
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("the interface copy is writable");
    for entry in std::fs::read_dir(from).expect("ui/ is readable") {
        let entry = entry.expect("ui/ is readable");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("ui/ is readable").is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("the interface copy is writable");
        }
    }
}

fn compile_interface() {
    slint_build::compile_with_config(
        interface_root(),
        slint_build::CompilerConfiguration::new()
            .with_style("fluent-dark".into())
            // Element names and types, which `--a11y` (Slint's element
            // search API) needs.
            .with_debug_info(true),
    )
    .expect("the Slint interface compiles");
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
