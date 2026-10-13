//! `isocast-ui`: the Isocast app in Slint. A portable native window laid
//! out as OBS Studio's, a thin client of isocastd. How it is put together
//! (the areas, the kit, the snapshot states) is in src/areas/mod.rs.
//!
//! ```text
//! isocast-ui                         the app in a window, showing the
//!                                    daemon's program picture
//! isocast-ui --bench 30 [--check]    …measuring its CPU for 30 s (and
//!                                    checking each frame), then quit
//! isocast-ui --drag-test 6           …dragging the selected item for 6 s
//!                                    (--bench and --drag-test, and any run
//!                                    with ISOCAST_BACKGROUND set, show the
//!                                    window without taking focus)
//! isocast-ui --snapshot out.png [--size 1280x800] [--scale 2]
//!            [--state settings:output] [--state kit] [--a11y]
//!                                    drawn offscreen, nothing on screen
//! isocast-ui --soak 600 [--no-check] offscreen at 60 fps for 10 minutes,
//!                                    dragging the box and checking frames
//! ```

mod a11y;
mod app;
mod areas;
mod background;
mod daemon;
mod form;
mod frames;
mod gpu;
mod kit;
#[cfg(target_os = "macos")]
mod layers;
mod model;
mod offscreen;
mod picture;
mod platform;
mod prefs;
mod preview;
// Settings forms from the setup's schema; the views (Properties,
// Filters, Transition Properties) move onto it next.
#[allow(dead_code)]
mod session;
mod toasts;
mod usage;
mod window;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

slint::include_modules!();

pub struct Options {
    pub snapshot: Option<PathBuf>,
    pub size: (u32, u32),
    pub scale: f32,
    pub states: Vec<String>,
    pub a11y: bool,
    pub wait_ms: u64,
    pub soak: Option<f64>,
    pub check: bool,
    pub still: bool,
    pub bench: Option<f64>,
    pub fps: u32,
    pub preview: preview::Mode,
    pub drag_test: Option<f64>,
    /// The window reads each frame for black, stale or misplaced boxes.
    pub frame_check: bool,
    /// Measured windows float above others unless this is set (a window
    /// capture only sees windows at the normal level).
    pub no_float: bool,
}

const USAGE: &str = "usage: isocast-ui [--bench SECONDS] [--check] [--drag-test SECONDS] [--fps N]
                  [--preview layers|image] [--no-float]
       isocast-ui --snapshot OUT.png [--size WxH] [--scale N] [--state STATE]… [--a11y] [--wait-ms MS]
       isocast-ui --a11y [--state STATE]…
       isocast-ui --soak SECONDS [--no-check] [--still] [--size WxH] [--scale N]
states (in order, see src/offscreen.rs and each area's STATES):
        dialog:KIND[:ARG], menu:OWNER, menu-bar:ID, menu-bar-open:TITLE,
        toast:KIND:TITLE, kit,
        kit-dialog, kit-menu, kit-dropdown, type:TEXT, key:NAME,
        drag:X1,Y1,X2,Y2, hover:X,Y, press:LABEL, hide:DOCK, wait:MS; select:PATH, rename,
        cameras,
        studio, record, stop-record, go-live, stop-stream, stream:KIND,
        details:rec|live, toasts, toast-drops, transition:KIND, about,
        settings[:TAB], box:X,Y, canvas-hover:PATH,
        canvas-drag:X1,Y1,X2,Y2[,KEY], canvas-hold:…, preview-scale:MODE
ISOCAST_BACKGROUND=1 shows the window without taking focus (always on for
--bench and --drag-test)";

fn parse() -> Result<Options> {
    let mut options = Options {
        snapshot: None,
        size: (1280, 800),
        scale: 1.0,
        states: Vec::new(),
        a11y: false,
        wait_ms: 300,
        soak: None,
        check: true,
        still: false,
        bench: None,
        fps: 60,
        preview: preview::Mode::default_for_platform(),
        drag_test: None,
        frame_check: false,
        no_float: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .with_context(|| format!("{name} needs a value\n{USAGE}"))
        };
        match arg.as_str() {
            "--snapshot" => options.snapshot = Some(value("--snapshot")?.into()),
            "--size" => {
                let size = value("--size")?;
                let (w, h) = size
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                    .with_context(|| {
                        format!("--size takes WIDTHxHEIGHT, like 1280x800, not {size:?}")
                    })?;
                options.size = (w, h);
            }
            "--scale" => {
                options.scale = value("--scale")?
                    .parse()
                    .context("--scale takes a number, like 2")?
            }
            "--state" => options.states.push(value("--state")?),
            "--a11y" => options.a11y = true,
            "--wait-ms" => {
                options.wait_ms = value("--wait-ms")?
                    .parse()
                    .context("--wait-ms takes milliseconds")?
            }
            "--soak" => {
                options.soak = Some(value("--soak")?.parse().context("--soak takes seconds")?)
            }
            "--no-check" => options.check = false,
            "--still" => options.still = true,
            "--bench" => {
                options.bench = Some(value("--bench")?.parse().context("--bench takes seconds")?)
            }
            "--check" => options.frame_check = true,
            "--no-float" => options.no_float = true,
            "--drag-test" => {
                options.drag_test = Some(
                    value("--drag-test")?
                        .parse()
                        .context("--drag-test takes seconds")?,
                )
            }
            "--fps" => {
                options.fps = value("--fps")?
                    .parse::<u32>()
                    .ok()
                    .filter(|fps| (1..=60).contains(fps))
                    .context("--fps takes a whole number from 1 to 60")?
            }
            "--preview" => {
                options.preview = match value("--preview")?.as_str() {
                    "layers" => preview::Mode::Layers,
                    "image" => preview::Mode::Image,
                    other => bail!("--preview takes layers or image, not {other:?}"),
                }
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => bail!("unknown option {other:?}\n{USAGE}"),
        }
    }
    Ok(options)
}

fn main() {
    let result = parse().and_then(|options| {
        if options.snapshot.is_some() || options.a11y || options.soak.is_some() {
            offscreen::run(&options)
        } else {
            window::run(&options)
        }
    });
    let code = match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Error: {error:?}");
            1
        }
    };
    exit(code)
}

/// Leaves without running thread-local destructors. Slint keeps its
/// platform, and with it the GPU device, in a thread-local that is dropped
/// after wgpu's own; a debug build then aborts at exit, and macOS shows a
/// crash dialog that comes to the front over whatever Ryan is working in.
fn exit(code: i32) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    #[cfg(unix)]
    // SAFETY: ends the process; nothing runs after it.
    unsafe {
        libc::_exit(code)
    }
    #[cfg(not(unix))]
    std::process::exit(code)
}
