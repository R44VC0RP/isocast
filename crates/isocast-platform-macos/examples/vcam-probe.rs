//! Non-installing reader authorized by the G.1 brief. Pass SOCKET [FRAME.ppm].
#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: vcam-probe SOCKET [FRAME.ppm]"))?;
    let picture = args.next();
    isocast_platform_macos::virtual_camera::probe::read(
        std::path::Path::new(&path),
        picture.as_deref().map(std::path::Path::new),
    )
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("camera probes require macOS");
}
