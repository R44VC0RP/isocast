use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/virtual_camera.m");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let object = out.join("virtual_camera.o");
    assert!(
        Command::new("xcrun")
            .args([
                "clang",
                "-c",
                "-fobjc-arc",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-arch",
                "arm64",
                "-mmacosx-version-min=14.0",
                "src/virtual_camera.m",
                "-o"
            ])
            .arg(&object)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("xcrun")
            .args(["ar", "rcs"])
            .arg(out.join("libisocast_camera.a"))
            .arg(object)
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=isocast_camera");
    for framework in [
        "CoreMediaIO",
        "CoreMedia",
        "CoreVideo",
        "Foundation",
        "SystemExtensions",
        "IOSurface",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}
