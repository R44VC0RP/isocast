fn main() {
    // A Windows main thread reserves 1 MiB of stack. Unoptimized builds of
    // the CLI overflow it while parsing arguments (even `--version`); reserve
    // the same 8 MiB macOS gives its main thread. Only the reservation grows.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bin=isocast=/STACK:8388608");
    }
}
