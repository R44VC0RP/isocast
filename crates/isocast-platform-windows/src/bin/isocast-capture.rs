#[cfg(windows)]
fn main() {
    isocast_platform_windows::capture_helper_main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("isocast-capture is only available on Windows");
    std::process::exit(1);
}
