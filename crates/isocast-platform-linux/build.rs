fn main() {
    println!("cargo:rerun-if-changed=native/nvenc.c");
    println!("cargo:rerun-if-changed=native/nvenc.h");
    println!("cargo:rerun-if-changed=../isocast-platform-windows/native/nvEncodeAPI.h");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    // Share the existing MIT-licensed NVENC 13.0 ABI, not the Windows backend.
    // CUDA and NVENC are loaded from the driver at runtime, never linked here.
    cc::Build::new()
        .std("c11")
        .flag("-isystem../isocast-platform-windows/native")
        .file("native/nvenc.c")
        .warnings_into_errors(true)
        .compile("isocast_linux_nvenc");
    println!("cargo:rustc-link-lib=dl");
}
