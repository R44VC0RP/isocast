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
//!            [--drag-mode resize]    (or its corner, or a dock splitter:
//!                                    columns, rows)
//!                                    (--bench and --drag-test, and any run
//!                                    with ISOCAST_BACKGROUND set, show the
//!                                    window without taking focus)
//! isocast-ui --snapshot out.png [--size 1280x800] [--scale 2]
//!            [--state settings:output] [--state kit] [--a11y]
//!            [--crop controls]       drawn offscreen, nothing on screen
//!                                    (several --snapshot: one picture
//!                                    after each one's states, one app)
//! isocast-ui --snapshot-watch DIR [--state record] [--crop controls]
//!                                    …redrawn on every ui/*.slint edit
//!                                    (built with --features live-slint)
//! isocast-ui --soak 600 [--no-check] offscreen at 60 fps for 10 minutes,
//!                                    dragging the box and checking frames
//! isocast-ui --motion SUITE.toml …   replay scripted pointer input
//!                                    offscreen and check each frame
//!                                    (src/motion, docs/motion-testing.md)
//! ```

mod a11y;
mod app;
mod areas;
mod background;
mod clock;
mod daemon;
mod form;
mod frames;
mod gpu;
mod helper;
mod keys;
mod kit;
#[cfg(target_os = "macos")]
mod layers;
mod model;
// The motion harness (`--motion`): docs/motion-testing.md.
mod motion;
mod offscreen;
mod picture;
mod platform;
mod prefs;
mod preview;
mod quit;
// Settings forms from the setup's schema; the views (Properties,
// Filters, Transition Properties) move onto it next.
#[allow(dead_code)]
mod session;
mod thumbs;
mod toasts;
mod update;
mod usage;
mod window;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

slint::include_modules!();

pub struct Options {
    /// Each picture, after the first N of `states` (the states given
    /// before the next `--snapshot`).
    pub snapshots: Vec<(PathBuf, usize)>,
    /// `--snapshot-watch DIR`: draw DIR/snapshot.png, then again whenever
    /// a .slint file changes (a `live-slint` build reads them at run time).
    pub watch: Option<PathBuf>,
    /// `--crop`: only this part of the window in each picture.
    pub crop: Option<String>,
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
    /// What `--drag-test` drags.
    pub drag_mode: window::DragMode,
    /// The window reads each frame for black, stale or misplaced boxes.
    pub frame_check: bool,
    /// Measured windows float above others unless this is set (a window
    /// capture only sees windows at the normal level).
    pub no_float: bool,
}

const USAGE: &str = "usage: isocast-ui [--bench SECONDS] [--check] [--drag-test SECONDS] [--fps N]
                  [--drag-mode move|resize|resize-select|columns|rows] [--preview layers|image] [--no-float]
       isocast-ui --snapshot OUT.png [--size WxH] [--scale N] [--state STATE]… [--a11y] [--wait-ms MS]
                  [--crop AREA|X,Y,W,H] [--snapshot OUT2.png [--state STATE]…]…
       isocast-ui --snapshot-watch DIR [--size WxH] [--scale N] [--state STATE]… [--crop AREA|X,Y,W,H]
                  (a `--features live-slint` build: redraws DIR/snapshot.png on every ui/*.slint edit)
crop areas: an element id in ui/app.slint (scenes, sources, mixer, transitions, controls, stage,
        bar), the first shown element of a component (ControlsDock), or X,Y,W,H in window points
       isocast-ui --a11y [--state STATE]…
       isocast-ui --soak SECONDS [--no-check] [--still] [--size WxH] [--scale N]
states (in order, see src/offscreen.rs and each area's STATES):
        dialog:KIND[:ARG], menu:OWNER, menu-bar:ID, menu-bar-open:TITLE,
        toast:KIND:TITLE, kit,
        kit-dialog, kit-menu, kit-dropdown, type:TEXT, key:NAME,
        drag:X1,Y1,X2,Y2, hover:X,Y, press:LABEL, hide:DOCK, wait:MS, events:FILE; select:PATH, rename,
        cameras,
        studio, record, stop-record, go-live, stop-stream, stream:KIND,
        details:rec|live, toasts, toast-drops, transition:KIND, about,
        settings[:TAB], box:X,Y, canvas-hover:PATH,
        canvas-drag:X1,Y1,X2,Y2[,KEY], canvas-hold:…, preview-scale:MODE
ISOCAST_BACKGROUND=1 shows the window without taking focus (always on for
--bench and --drag-test)";

fn parse() -> Result<Options> {
    let mut options = Options {
        snapshots: Vec::new(),
        watch: None,
        crop: None,
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
        drag_mode: window::DragMode::Move,
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
            // A later picture is taken after the states that follow it;
            // the first after every state before the second (so one
            // `--snapshot` anywhere comes after all the states).
            "--snapshot" => {
                let path = value("--snapshot")?.into();
                let states = options.states.len();
                if let Some(last) = options.snapshots.last_mut() {
                    last.1 = states;
                }
                options.snapshots.push((path, usize::MAX));
            }
            "--snapshot-watch" => options.watch = Some(value("--snapshot-watch")?.into()),
            "--crop" => options.crop = Some(value("--crop")?),
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
            "--drag-mode" => {
                options.drag_mode = match value("--drag-mode")?.as_str() {
                    "move" => window::DragMode::Move,
                    "resize" => window::DragMode::Resize,
                    "resize-select" => window::DragMode::ResizeSelect,
                    "columns" => window::DragMode::Columns,
                    "rows" => window::DragMode::Rows,
                    other => {
                        bail!(
                            "--drag-mode takes move, resize, resize-select, columns or rows, not {other:?}"
                        )
                    }
                }
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
                isocast_diagnostics::clean_exit();
                std::process::exit(0);
            }
            other => bail!("unknown option {other:?}\n{USAGE}"),
        }
    }
    let states = options.states.len();
    if let Some(last) = options.snapshots.last_mut() {
        last.1 = states;
    }
    Ok(options)
}

fn main() {
    let _logs = isocast_diagnostics::init("isocast-ui", true);
    if std::env::args().nth(1).as_deref() == Some("--motion") {
        exit(match motion::main(std::env::args().skip(1).collect()) {
            Ok(true) => 0,
            Ok(false) => 3,
            Err(error) => {
                isocast_diagnostics::diagnostic!("error"; "Error: {error:?}");
                1
            }
        });
    }
    let result = parse().and_then(|options| {
        if !options.snapshots.is_empty()
            || options.watch.is_some()
            || options.a11y
            || options.soak.is_some()
        {
            refuse_real_engine()?;
            offscreen::run(&options)
        } else {
            window::run(&options)
        }
    });
    let code = match result {
        Ok(()) => 0,
        Err(error) => {
            isocast_diagnostics::diagnostic!("error"; "Error: {error:?}");
            1
        }
    };
    exit(code)
}

/// Offscreen runs drive the app's states for real (`record` starts a
/// recording, `go-live` a stream), so they must never reach the user's own
/// engine: one did, and recorded Ryan's camera, mic and screen for three
/// hours. They need `ISOCAST_SOCKET` pointing at a throwaway engine, as
/// scripts/snapshot.sh and scripts/visual-shots.sh set up.
fn refuse_real_engine() -> Result<()> {
    let socket = isocast_protocol::api::socket_path();
    let own = isocast_protocol::api::default_socket_path();
    // The same folder reached through a symlink still counts as the same.
    let folder = |p: &std::path::Path| p.parent().and_then(|p| p.canonicalize().ok());
    let same = socket == own
        || (socket.file_name() == own.file_name()
            && folder(&socket).is_some()
            && folder(&socket) == folder(&own));
    let default = std::env::var_os("ISOCAST_SOCKET").is_none() || same;
    if default {
        bail!(
            "offscreen runs (--snapshot, --snapshot-watch, --a11y, --soak) never use your own \
             engine at {}: their states start recordings and streams for real. Use \
             scripts/snapshot.sh, which starts a throwaway engine with synthetic sources, or set \
             ISOCAST_SOCKET to an engine of your own in a scratch folder",
            socket.display()
        );
    }
    Ok(())
}

/// Leaves without running thread-local destructors. Slint keeps its
/// platform, and with it the GPU device, in a thread-local that is dropped
/// after wgpu's own; a debug build then aborts at exit, and macOS shows a
/// crash dialog that comes to the front over whatever Ryan is working in.
fn exit(code: i32) -> ! {
    isocast_diagnostics::clean_exit();
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
