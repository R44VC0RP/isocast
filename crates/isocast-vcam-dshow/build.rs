fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        for file in ["native/filter.cpp", "native/server.cpp", "native/camera.h"] {
            println!("cargo:rerun-if-changed={file}");
        }
        cc::Build::new()
            .cpp(true)
            .std("c++17")
            .define("NOMINMAX", None)
            .files(["native/filter.cpp", "native/server.cpp"])
            .compile("camera");
        // regsvr32/COM look these up by undecorated name. PRIVATE excludes
        // them from the import library (they must never be statically called).
        for (name, stack) in [
            ("DllGetClassObject", 12),
            ("DllCanUnloadNow", 0),
            ("DllRegisterServer", 0),
            ("DllUnregisterServer", 0),
        ] {
            let alias = if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("x86") {
                format!("=_{name}@{stack}")
            } else {
                String::new()
            };
            println!("cargo:rustc-cdylib-link-arg=/EXPORT:{name}{alias},PRIVATE");
        }
        for lib in ["ole32", "oleaut32", "advapi32", "strmiids", "uuid"] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
}
