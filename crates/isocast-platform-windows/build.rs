fn main() {
    println!("cargo:rerun-if-changed=native/nvenc.cpp");
    println!("cargo:rerun-if-changed=native/nvEncodeAPI.h");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file("native/nvenc.cpp")
        .warnings_into_errors(true)
        .compile("isocast_nvenc");
}
