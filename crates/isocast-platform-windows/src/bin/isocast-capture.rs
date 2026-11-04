#[cfg(windows)]
fn main() {
    if std::env::args().nth(1).as_deref() == Some("--process-loopback") {
        isocast_platform_windows::process_audio_helper_main();
    }
    isocast_platform_windows::capture_helper_main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("isocast-capture is only available on Windows");
    std::process::exit(1);
}
