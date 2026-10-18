//! Area G, the Create view (C.2): the clip bar under the viewer and the
//! control bar, in place of the docks (View → Create, ui/areas/create/).
//! It shows the current timeline's clips in order on the Program track,
//! each a strip of frames from its take's program file (src/thumbs.rs),
//! with Record and Retake, Remove, drag to reorder, New and Open, and
//! Export with its progress.
//!
//! It holds nothing the engine owns: the timeline is read with
//! `timeline_list` and `timeline_get` when the link connects and whenever
//! the engine says a timeline changed (`timeline_changed`, from this
//! window, the CLI or MCP), and every change is a method on the API
//! (`retake`, `timeline_remove`, `timeline_move`, `timeline_new`,
//! `timeline_open`, `timeline_close`, `timeline_export`). A move or remove
//! shows at once and the event's read confirms it. Export follows
//! `export_progress` and `export_finished`.
//!
//! The Edit view (C.3) is the same area: every ISO source of the takes on
//! its own track under the Program track, the picture at the playhead (the
//! angle the edited video shows there, decoded exactly: `Preview`), and the
//! edits, each a method too (`timeline_trim`, `timeline_split`,
//! `timeline_delete_range`, `timeline_angle`, `timeline_undo`/`redo`):
//! trim handles, S splits at the playhead, I and O mark a span that Delete
//! takes out, 1–9 or a track's header choose the angle, J K L play the
//! preview, ⌘Z undoes the last edit.
//!
//! Round 2 (#87): crops of the picture (C, handles on the viewer, shape
//! presets; `timeline_crop`), rolls (⌥-drag of an edge; `timeline_roll`),
//! waveforms on the tracks (src/waves.rs), snapping (N) to clip edges,
//! markers, the marks and the playhead, edit-point and marker navigation
//! (↑ ↓, M ⇧M), Space to play, and zoom (⌘+ ⌘−, ⇧Z, ⌘-scroll, pinch).

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use isocast_protocol::PROGRAM;
use isocast_protocol::api::{EngineState, Event, Method};
use isocast_protocol::timeline::{
    ClipFile, ClipInfo, Crop, CropPreset, ExportStarted, NleApp, NleExported, NleFormat, Retaken,
    TimelineAngleParams, TimelineCropParams, TimelineExportNleParams, TimelineExportParams,
    TimelineInfo, TimelineList, TimelineMoveParams, TimelineNewParams, TimelineOpenParams,
    TimelineRangeParams, TimelineRemoveParams, TimelineRollParams, TimelineSplitParams,
    TimelineTarget, TimelineTrimParams,
};
use slint::{ComponentHandle, ModelRc, VecModel};

use super::projects::Raise;
use super::{Area, Changed};
use crate::app::App;
use crate::daemon::Link;
use crate::kit::{self, MenuBuilder};
use crate::thumbs::{Playback, Preview, Segment, Thumbs};
use crate::toasts::Toast;
use crate::waves::Waves;
use crate::{AppWindow, ClipRow, CreateArea, EditorWindow, SegmentRow, ToastKind, TrackRow};

/// Where edits are shown: the editor window (C.3), or before it opens the
/// studio window (whose CreateArea keeps the playhead, unseen there).
enum Surface {
    Studio(AppWindow),
    Editor(EditorWindow),
}

impl Surface {
    fn area(&self) -> CreateArea<'_> {
        match self {
            Surface::Studio(window) => window.global::<CreateArea>(),
            Surface::Editor(window) => window.global::<CreateArea>(),
        }
    }
}

/// Snapshot states (`--state`).
pub const STATES: &[&str] = &[
    "create (show the Create view)",
    "create-new[:NAME]",
    "create-open:ID",
    "create-close",
    "create-select:INDEX",
    "create-retake",
    "create-remove",
    "create-move:INDEX,TO",
    "create-export[:vertical]",
    "create-zoom:0..1",
    "create-scroll:POINTS",
    "create-thumbs (wait for the frames)",
    "create-playhead:SECONDS",
    "create-mark:IN,OUT",
    "create-split",
    "create-delete (the marked span)",
    "create-angle:SOURCE",
    "create-trim:INDEX,in|out,SECONDS",
    "create-undo",
    "create-redo",
    "create-layout:0|1",
    "create-preview (wait for the picture)",
    "create-crop:X,Y,W,H|PRESET|clear (crop the clip at the playhead)",
    "create-crop-mode:on|off",
    "create-roll:INDEX,SECONDS",
    "create-waves (wait for the waveforms)",
    "create-key:KEY (an editor key: space, up, down, marker:1, …)",
    "editor (open the editor window; the snapshot draws it)",
];

/// How often the playhead moves while playing (J/K/L, Space) without the
/// display's refresh to go by (offscreen, or the studio's Edit view):
/// about a quarter of a 60 Hz display's frame, so each played picture
/// shows within a few ms of when it is due (#94).
const PLAY_EVERY: Duration = Duration::from_millis(4);

#[derive(Default)]
pub struct Create {
    /// The current timeline as last read (none: no timeline is current).
    timeline: RefCell<Option<TimelineInfo>>,
    /// Every timeline, for New/Open's menu.
    list: RefCell<Option<TimelineList>>,
    /// The selected clip's id.
    selected: RefCell<Option<String>>,
    /// A read is on its way; another was asked for meanwhile.
    loading: Cell<bool>,
    again: Cell<bool>,
    link: Cell<Option<Link>>,
    /// The export running (its id) and how far it has got.
    export: RefCell<Option<(String, f64)>>,
    /// The last export's program file.
    exported: RefCell<Option<PathBuf>>,
    /// A retake was asked for; until its take begins (`started`).
    retake_since: Cell<Option<Instant>>,
    rows: Rc<VecModel<ClipRow>>,
    thumbs: Thumbs,
    /// Collects thumbnails while some are being made.
    thumbs_timer: slint::Timer,
    retake_timer: slint::Timer,
    /// The Edit view's ISO tracks.
    tracks: Rc<VecModel<TrackRow>>,
    preview: Preview,
    /// Collects the preview's picture while one is being made.
    preview_timer: slint::Timer,
    /// Playing (J/K/L): times real time, and when the last step was.
    shuttle: Cell<f64>,
    played: Cell<Option<Instant>>,
    play_timer: slint::Timer,
    /// Playing forward: the pictures decoded ahead (#94), and where it
    /// put the playhead last (moved elsewhere meanwhile: decode from there).
    playback: RefCell<Option<Playback>>,
    played_to: Cell<f64>,
    /// Playing in the editor window: its display's refresh, which asks
    /// for each frame and says when it shows (#94).
    refresh: RefCell<Option<crate::refresh::Refresh>>,
    /// Steps the playhead when the display link's frames aren't drawn.
    stall_timer: slint::Timer,
    /// Playing silently because a capture would take the sound in (#94).
    silenced: Cell<bool>,
    /// The editor window has no rendering notifier: its timer steps.
    no_notifier: Cell<bool>,
    /// When the preview's newest picture was asked for, for
    /// ISOCAST_PREVIEW_STATS=1 (scrub latency on a test Mac).
    asked_at: Cell<Option<Instant>>,
    /// The editor window (C.3), once opened (Window › Editor).
    editor: RefCell<Option<EditorWindow>>,
    /// The tracks' waveforms (#87).
    waves: Waves,
    /// A crop handle being dragged: what it holds, the crop when it was
    /// pressed, and the clip.
    crop_drag: RefCell<Option<(String, Crop, String)>>,
}

/// Each clip's take's number, in the order the takes were recorded (a
/// take split into several clips (C.3) keeps one number), and how many
/// takes there are.
fn take_numbers(clips: &[ClipInfo]) -> (Vec<usize>, usize) {
    let mut takes: Vec<(&str, &Path)> = clips
        .iter()
        .map(|c| (c.started_at.as_deref().unwrap_or(""), c.take.as_path()))
        .collect();
    takes.sort();
    takes.dedup();
    let numbers = clips
        .iter()
        .map(|c| {
            takes
                .iter()
                .position(|(_, take)| *take == c.take.as_path())
                .map_or(0, |i| i + 1)
        })
        .collect();
    (numbers, takes.len())
}

/// The clock ISOCAST_PREVIEW_STATS times played pictures by.
fn stats_clock() -> Instant {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// 125.4 → "02:05".
fn mm_ss(seconds: f64) -> String {
    let whole = seconds.max(0.0).round() as u64;
    format!("{:02}:{:02}", whole / 60, whole % 60)
}

fn place(index: usize) -> String {
    format!("{:02}", index + 1)
}

/// The clip's program file (else its first), if it is there.
fn program_file(clip: &ClipInfo) -> Option<&Path> {
    clip.files
        .iter()
        .find(|f| f.source == isocast_protocol::PROGRAM)
        .or_else(|| clip.files.first())
        .filter(|f| f.exists)
        .map(|f| f.path.as_path())
}

/// The clip's file of `source`, if it is there.
/// Whether the engine is taking in sound that could include the editor's
/// playback (#94): a recording (or one starting), a stream that isn't
/// offline or failed (either canvas) or a running replay buffer, with a
/// source that hears this Mac's output: system audio (its `exclude` list
/// isn't trusted to name this app), or an app's audio that could be
/// Isocast's.
fn capturing(session: &crate::session::Session) -> bool {
    use isocast_protocol::SourceKind;
    use isocast_protocol::replay::ReplayState;
    use isocast_protocol::stream::StreamState;
    let status = session.status.as_ref();
    let live = |s: &isocast_protocol::stream::StreamStatus| {
        !matches!(s.state, StreamState::Offline | StreamState::Failed)
    };
    // Streams and the main replay buffer as their events left them (the
    // status read lags them); the vertical buffer as last read.
    let running = status
        .is_some_and(|s| matches!(s.state, EngineState::Recording | EngineState::Starting))
        || session.stream.iter().any(live)
        || session.streams.iter().any(live)
        || session.vertical_streams.iter().any(live)
        || [
            session.replay.as_ref(),
            status.and_then(|s| s.vertical_replay.as_ref()),
        ]
        .iter()
        .any(|r| r.is_some_and(|r| r.state == ReplayState::Running));
    if !running {
        return false;
    }
    let isocast = |app: &str| {
        let app = app.trim().to_lowercase();
        !app.is_empty() && ("isocast".contains(&app) || app.contains("isocast"))
    };
    session.setup.as_ref().is_some_and(|setup| {
        setup.sources.iter().any(|s| match &s.kind {
            SourceKind::SystemAudio(_) => true,
            SourceKind::ApplicationAudio(a) => isocast(&a.app),
            _ => false,
        })
    })
}

fn file_of<'a>(clip: &'a ClipInfo, source: &str) -> Option<&'a ClipFile> {
    clip.files.iter().find(|f| f.source == source && f.exists)
}

/// Bring the editor window forward with the keyboard (it was asked for),
/// unless the app runs in the background (ISOCAST_BACKGROUND, test Macs).
fn focus(editor: &EditorWindow) {
    if std::env::var_os("ISOCAST_BACKGROUND").is_some() {
        return;
    }
    use slint::winit_030::WinitWindowAccessor;
    // Offscreen there is no winit window, and nothing to focus.
    editor
        .window()
        .with_winit_window(|window| window.focus_window());
}

/// After showing the editor: focus it, or leave it, or put it behind the
/// studio window (which keeps the keyboard), now and once its native
/// window surely exists.
fn raised(app: &Rc<App>, editor: &EditorWindow, raise: Raise) {
    match raise {
        Raise::Focus => focus(editor),
        Raise::Show => {}
        Raise::Behind => {
            let behind = |app: &Rc<App>| {
                let editor = app
                    .create
                    .editor
                    .borrow()
                    .as_ref()
                    .map(|e| e.clone_strong());
                if let (Some(editor), Some(studio)) = (editor, app.window()) {
                    crate::documents::order_behind(editor.window(), studio.window());
                }
            };
            behind(app);
            let weak = Rc::downgrade(app);
            slint::Timer::single_shot(Duration::from_millis(100), move || {
                if let Some(app) = weak.upgrade() {
                    behind(&app);
                }
            });
        }
    }
}

/// The take's length (its program file's, else the clip's out point).
fn take_length(clip: &ClipInfo) -> f64 {
    clip.files
        .iter()
        .find(|f| f.source == PROGRAM)
        .and_then(|f| f.duration_seconds)
        .unwrap_or(clip.out_seconds)
        .max(1e-3)
}

/// The part of `file` the clip covers, as fractions of the file's length.
fn fractions(clip: &ClipInfo, file: Option<&ClipFile>) -> (f32, f32) {
    let offset = file.and_then(|f| f.start_offset_seconds).unwrap_or(0.0);
    let length = file
        .and_then(|f| f.duration_seconds)
        .unwrap_or_else(|| take_length(clip))
        .max(1e-3);
    let frac = |t: f64| ((t - offset) / length).clamp(0.0, 1.0) as f32;
    (frac(clip.in_seconds), frac(clip.out_seconds))
}

/// A source's name for its track ("camera" → "Camera"), from the setup if
/// it names it.
fn source_name(app: &App, source: &str) -> String {
    if let Some(setup) = app.session.borrow().setup.as_ref()
        && let Some(found) = setup.sources.iter().find(|s| s.id == source)
        && let Some(name) = found.name.as_ref().filter(|n| !n.is_empty())
    {
        return name.clone();
    }
    let mut chars = source.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// 3.25 → "00:03.25".
fn mm_ss_cc(seconds: f64) -> String {
    let cs = (seconds.max(0.0) * 100.0).round() as u64;
    format!("{:02}:{:02}.{:02}", cs / 6000, cs / 100 % 60, cs % 100)
}

/// The takes' markers inside their clips, in seconds on the timeline, in
/// order.
fn marker_times(clips: &[ClipInfo]) -> Vec<f64> {
    let mut times: Vec<f64> = clips
        .iter()
        .flat_map(|clip| {
            clip.markers
                .iter()
                .filter(|m| m.time_seconds >= clip.in_seconds && m.time_seconds < clip.out_seconds)
                .map(|m| clip.start_seconds + m.time_seconds - clip.in_seconds)
        })
        .collect();
    times.sort_by(f64::total_cmp);
    times
}

/// Where a crop handle dragged by `dx`, `dy` (fractions of the picture)
/// puts the crop that was `start` when pressed: `handle` is "move" or the
/// edges it holds ("l", "r", "t", "b", "tl", …). With `aspect` (the shape
/// to keep, width over height in pixels, on a picture `picture` wide over
/// high) the crop keeps its shape, from the opposite corner or edge.
fn dragged_crop(
    handle: &str,
    start: Crop,
    dx: f64,
    dy: f64,
    aspect: Option<f64>,
    picture: f64,
) -> Crop {
    // The smallest crop while dragging: 5% of the picture each way.
    const MIN: f64 = 0.05;
    let mut c = start;
    if handle == "move" {
        c.x = (start.x + dx).clamp(0.0, 1.0 - start.width);
        c.y = (start.y + dy).clamp(0.0, 1.0 - start.height);
        return c;
    }
    let (left, right) = (handle.contains('l'), handle.contains('r'));
    let (top, bottom) = (handle.contains('t'), handle.contains('b'));
    let (x0, y0) = (start.x, start.y);
    let (x1, y1) = (start.x + start.width, start.y + start.height);
    let (mut nx0, mut ny0, mut nx1, mut ny1) = (x0, y0, x1, y1);
    if left {
        nx0 = (x0 + dx).clamp(0.0, x1 - MIN);
    }
    if right {
        nx1 = (x1 + dx).clamp(x0 + MIN, 1.0);
    }
    if top {
        ny0 = (y0 + dy).clamp(0.0, y1 - MIN);
    }
    if bottom {
        ny1 = (y1 + dy).clamp(y0 + MIN, 1.0);
    }
    let Some(aspect) = aspect else {
        return Crop {
            x: nx0,
            y: ny0,
            width: nx1 - nx0,
            height: ny1 - ny0,
        };
    };
    // In fractions: height = width * picture / aspect.
    let ratio = picture / aspect;
    let horizontal = left || right;
    let vertical = top || bottom;
    let (mut w, mut h) = (nx1 - nx0, ny1 - ny0);
    if horizontal && (!vertical || w * ratio >= h) {
        h = w * ratio;
    } else {
        w = h / ratio;
    }
    // Anchor: the opposite edge (or the centre, for an edge alone).
    let ax = match (left, right) {
        (true, false) => x1 - w,
        (false, true) => x0,
        _ => (x0 + x1 - w) / 2.0,
    };
    let ay = match (top, bottom) {
        (true, false) => y1 - h,
        (false, true) => y0,
        _ => (y0 + y1 - h) / 2.0,
    };
    // Shrink to fit inside the picture, keeping the shape and the anchor.
    let fit_w = match (left, right) {
        (true, false) => x1,
        (false, true) => 1.0 - x0,
        _ => 2.0 * ((x0 + x1) / 2.0).min(1.0 - (x0 + x1) / 2.0),
    };
    let fit_h = match (top, bottom) {
        (true, false) => y1,
        (false, true) => 1.0 - y0,
        _ => 2.0 * ((y0 + y1) / 2.0).min(1.0 - (y0 + y1) / 2.0),
    };
    let scale = (fit_w / w).min(fit_h / h).min(1.0);
    let (w2, h2) = (w * scale, h * scale);
    let x = match (left, right) {
        (true, false) => x1 - w2,
        (false, true) => x0,
        _ => ax + (w - w2) / 2.0,
    };
    let y = match (top, bottom) {
        (true, false) => y1 - h2,
        (false, true) => y0,
        _ => ay + (h - h2) / 2.0,
    };
    Crop {
        x: x.clamp(0.0, 1.0 - w2),
        y: y.clamp(0.0, 1.0 - h2),
        width: w2,
        height: h2,
    }
}

/// Clips' places and starts again after a local move or remove.
fn renumber(clips: &mut [ClipInfo]) {
    let mut start = 0.0;
    for (index, clip) in clips.iter_mut().enumerate() {
        clip.index = index;
        clip.start_seconds = start;
        start += clip.duration_seconds;
    }
}

impl Create {
    fn active(&self, app: &Rc<App>) -> bool {
        app.window()
            .is_some_and(|w| w.global::<CreateArea>().get_active())
    }

    /// Read the timelines and the current one, then show them. Reads
    /// asked for while one is on its way run once after it.
    pub fn load(&self, app: &Rc<App>) {
        if self.loading.replace(true) {
            self.again.set(true);
            return;
        }
        app.call_as::<TimelineList>(Method::TimelineList, |app, result| match result {
            Ok(list) => {
                let current = list.current.clone();
                *app.create.list.borrow_mut() = Some(list);
                match current {
                    Some(id) => app.call_as::<TimelineInfo>(
                        Method::TimelineGet(Some(TimelineTarget { timeline: Some(id) })),
                        |app, result| {
                            let info = match result {
                                Ok(info) => Some(info),
                                Err(error) => {
                                    eprintln!("create: {error}");
                                    None
                                }
                            };
                            app.create.loaded(app, info);
                        },
                    ),
                    None => app.create.loaded(app, None),
                }
            }
            Err(error) => {
                // An engine from before C.1 has no timelines.
                if !error.is_unknown_method() {
                    eprintln!("create: {error}");
                }
                *app.create.list.borrow_mut() = None;
                app.create.loaded(app, None);
            }
        });
    }

    fn loaded(&self, app: &Rc<App>, info: Option<TimelineInfo>) {
        {
            let mut selected = self.selected.borrow_mut();
            let kept = selected.as_ref().is_some_and(|id| {
                info.as_ref()
                    .is_some_and(|t| t.clips.iter().any(|c| &c.id == id))
            });
            if !kept {
                *selected = None;
            }
        }
        *self.timeline.borrow_mut() = info;
        self.loading.set(false);
        self.bump_version(app);
        self.show(app);
        if self.playback.borrow().is_some() {
            // Edited while playing: play the timeline as it is now.
            let origin = self.played.get().unwrap_or_else(Instant::now);
            self.start_playback(app, origin);
        }
        if self.again.replace(false) {
            self.load(app);
        }
    }

    /// Show everything: the clips and the header.
    fn show(&self, app: &Rc<App>) {
        self.show_rows(app);
        self.show_header(app);
    }

    /// The clips, row by row (a drag in progress keeps its rows).
    fn show_rows(&self, app: &Rc<App>) {
        let timeline = self.timeline.borrow();
        let selected = self.selected.borrow();
        let clips = timeline.as_ref().map(|t| t.clips.as_slice()).unwrap_or(&[]);
        let empty: ModelRc<slint::Image> = ModelRc::default();
        let mut asked = false;
        let (numbers, _) = take_numbers(clips);
        let rows: Vec<ClipRow> = clips
            .iter()
            .enumerate()
            .map(|(index, clip)| {
                let file = program_file(clip);
                let thumbs = match file {
                    Some(path) => match self.thumbs.get(path) {
                        Some(frames) => frames,
                        None => {
                            asked |= self.thumbs.request(path, take_length(clip));
                            empty.clone()
                        }
                    },
                    None => empty.clone(),
                };
                let (in_frac, out_frac) = fractions(clip, file_of(clip, PROGRAM));
                let (waves, wave_frac) = self.wave_of(clip, file_of(clip, PROGRAM), &mut asked);
                ClipRow {
                    id: clip.id.as_str().into(),
                    number: place(index).into(),
                    name: format!("Take {}", numbers[index]).into(),
                    length: mm_ss(clip.duration_seconds).into(),
                    start: clip.start_seconds as f32,
                    duration: clip.duration_seconds as f32,
                    selected: selected.as_deref() == Some(clip.id.as_str()),
                    problem: clip.problem.is_some() || clip.files.iter().any(|f| !f.exists),
                    dropped: clip.dropped_frames > 0,
                    thumbs,
                    in_frac,
                    out_frac,
                    angle: clip
                        .angle
                        .as_deref()
                        .map(|a| source_name(app, a))
                        .unwrap_or_default()
                        .into(),
                    cropped: clip.crop.is_some(),
                    waves,
                    wave_frac,
                }
            })
            .collect();
        kit::update_rows(&self.rows, rows);
        // The ISO tracks: every source the takes recorded but the program,
        // in the order they first appear.
        let mut sources: Vec<String> = Vec::new();
        for clip in clips {
            for file in &clip.files {
                if file.source != PROGRAM && !sources.contains(&file.source) {
                    sources.push(file.source.clone());
                }
            }
        }
        let tracks: Vec<TrackRow> = sources
            .iter()
            .enumerate()
            .map(|(i, source)| {
                let segments: Vec<SegmentRow> = clips
                    .iter()
                    .map(|clip| {
                        let file = file_of(clip, source);
                        let thumbs = match file {
                            Some(f) => match self.thumbs.get(&f.path) {
                                Some(frames) => frames,
                                None => {
                                    asked |= self.thumbs.request(
                                        &f.path,
                                        f.duration_seconds.unwrap_or_else(|| take_length(clip)),
                                    );
                                    empty.clone()
                                }
                            },
                            None => empty.clone(),
                        };
                        let (in_frac, out_frac) = fractions(clip, file);
                        let (waves, wave_frac) = self.wave_of(clip, file, &mut asked);
                        SegmentRow {
                            clip: clip.id.as_str().into(),
                            start: clip.start_seconds as f32,
                            duration: clip.duration_seconds as f32,
                            thumbs,
                            in_frac,
                            out_frac,
                            active: clip.angle.as_deref() == Some(source.as_str()),
                            missing: file.is_none(),
                            waves,
                            wave_frac,
                        }
                    })
                    .collect();
                TrackRow {
                    source: source.as_str().into(),
                    name: source_name(app, source).into(),
                    key: if i < 8 {
                        (i + 2).to_string().into()
                    } else {
                        "".into()
                    },
                    segments: ModelRc::new(VecModel::from(segments)),
                }
            })
            .collect();
        self.tracks.set_vec(tracks);
        drop(timeline);
        drop(selected);
        self.show_preview(app);
        if asked {
            self.collect_thumbs(app);
        }
    }

    /// `file`'s waveform pictures and the share of the file each covers,
    /// asked for if not made yet (only while the editor is open: the
    /// studio's clip bar shows none).
    fn wave_of(
        &self,
        clip: &ClipInfo,
        file: Option<&ClipFile>,
        asked: &mut bool,
    ) -> (ModelRc<slint::Image>, f32) {
        let Some(file) = file else {
            return (ModelRc::default(), 1.0);
        };
        if self.editor.borrow().is_none() {
            return (ModelRc::default(), 1.0);
        }
        match self.waves.get(&file.path) {
            Some(wave) => {
                let length = file
                    .duration_seconds
                    .unwrap_or_else(|| take_length(clip))
                    .max(1e-3);
                (wave.images, (wave.chunk_seconds / length) as f32)
            }
            None => {
                *asked |= self.waves.request(&file.path);
                (ModelRc::default(), 1.0)
            }
        }
    }

    /// Take in thumbnails (and waveforms) as they are made, every 50 ms
    /// until none are left (no timer runs otherwise).
    fn collect_thumbs(&self, app: &Rc<App>) {
        if self.thumbs_timer.running() {
            return;
        }
        let weak = Rc::downgrade(app);
        self.thumbs_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(50),
            move || {
                let Some(app) = weak.upgrade() else { return };
                let thumbs = app.create.thumbs.collect();
                if app.create.waves.collect() | thumbs {
                    app.create.show_rows(&app);
                }
                if app.create.thumbs.idle() && app.create.waves.idle() {
                    app.create.thumbs_timer.stop();
                }
            },
        );
    }

    /// The header: the take's line, what can be done, the export (in the
    /// studio's Create view and the editor window).
    fn show_header(&self, app: &Rc<App>) {
        if let Some(window) = app.window() {
            self.header_into(app, &window.global::<CreateArea>());
        }
        if let Some(editor) = self.editor.borrow().as_ref() {
            self.header_into(app, &editor.global::<CreateArea>());
        }
    }

    fn header_into(&self, app: &Rc<App>, area: &CreateArea) {
        let timeline = self.timeline.borrow();
        let (state, busy, has_setup) = {
            let s = app.session.borrow();
            (
                s.status.as_ref().map(|x| x.state),
                s.busy_record,
                s.setup.is_some(),
            )
        };
        let recording = state == Some(EngineState::Recording);
        let starting = state == Some(EngineState::Starting);
        let retaking = self.retake_since.get().is_some();
        let export = self.export.borrow().clone();
        let n = timeline.as_ref().map_or(0, |t| t.clips.len());

        area.set_has_timeline(timeline.is_some());
        // #114: whether the next recording adds a take here.
        let target = app
            .session
            .borrow()
            .status
            .as_ref()
            .and_then(|s| s.record_target.as_ref())
            .and_then(|t| t.timeline.clone());
        area.set_record_here(
            timeline
                .as_ref()
                .is_some_and(|t| target.as_deref() == Some(t.id.as_str())),
        );
        area.set_record_take(n as i32 + 1);
        area.set_timeline_name(
            timeline
                .as_ref()
                .map(|t| t.name.clone())
                .unwrap_or_default()
                .into(),
        );
        let total = timeline.as_ref().map_or(0.0, |t| t.duration_seconds);
        area.set_total_seconds(total as f32);
        let takes = timeline.as_ref().map_or(0, |t| take_numbers(&t.clips).1);
        area.set_summary(
            match takes == n {
                true => format!(
                    "{n} take{} · {}",
                    if n == 1 { "" } else { "s" },
                    mm_ss(total)
                ),
                false => format!(
                    "{takes} take{} · {n} clips · {}",
                    if takes == 1 { "" } else { "s" },
                    mm_ss(total)
                ),
            }
            .into(),
        );

        let (title, detail) = match &*timeline {
            None => (
                "No timeline".to_string(),
                " · takes are recorded but not added to one".to_string(),
            ),
            Some(t) if export.is_some() => (
                format!("Exporting {}", t.name),
                format!(
                    " · {n} clip{} · {}",
                    if n == 1 { "" } else { "s" },
                    mm_ss(total)
                ),
            ),
            Some(t) if retaking => (
                format!("Retake {}", place(n)),
                format!(" · recording it again for {}", t.name),
            ),
            Some(t) if recording || starting => (
                format!("Recording {}", place(n)),
                format!(" · added to {} when you stop", t.name),
            ),
            Some(t) => (
                format!("Next take: {}", place(n)),
                format!(" · Record adds it to {}", t.name),
            ),
        };
        area.set_status_title(title.into());
        area.set_status_detail(detail.into());
        area.set_retaking(retaking);
        area.set_can_retake(
            timeline.is_some() && has_setup && !busy && !retaking && (recording || n > 0),
        );
        area.set_can_remove(self.selected.borrow().is_some());
        area.set_can_export(n > 0);
        let markers: Vec<f32> = timeline
            .as_ref()
            .map(|t| marker_times(&t.clips))
            .unwrap_or_default()
            .into_iter()
            .map(|t| t as f32)
            .collect();
        area.set_markers(ModelRc::new(VecModel::from(markers)));
        area.set_can_undo(timeline.as_ref().is_some_and(|t| t.undo > 0));
        area.set_can_redo(timeline.as_ref().is_some_and(|t| t.redo > 0));
        area.set_shuttle(self.shuttle.get() as f32);
        area.set_exporting(export.is_some());
        area.set_export_progress(export.map_or(0.0, |(_, p)| p) as f32);
        area.set_export_path(
            self.exported
                .borrow()
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
                .into(),
        );
    }

    fn select(&self, app: &Rc<App>, id: &str) {
        let id = (!id.is_empty()).then(|| id.to_string());
        if *self.selected.borrow() == id {
            return;
        }
        *self.selected.borrow_mut() = id;
        self.show(app);
    }

    fn new_timeline(&self, app: &Rc<App>, name: Option<String>) {
        app.call(
            Method::TimelineNew(Some(TimelineNewParams { name })),
            |app, result| {
                if let Err(error) = result {
                    app.failed("Couldn’t make a timeline", &error);
                }
            },
        );
    }

    fn open(&self, app: &Rc<App>, id: &str) {
        app.call(
            Method::TimelineOpen(TimelineOpenParams {
                timeline: id.to_string(),
            }),
            |app, result| {
                if let Err(error) = result {
                    app.failed("Couldn’t open the timeline", &error);
                }
            },
        );
    }

    fn close(&self, app: &Rc<App>) {
        app.call(Method::TimelineClose, |app, result| {
            if let Err(error) = result {
                app.failed("Couldn’t close the timeline", &error);
            }
        });
    }

    fn retake(&self, app: &Rc<App>) {
        if self.retake_since.get().is_some() {
            return;
        }
        self.retake_since.set(Some(crate::clock::now()));
        self.show_header(app);
        // If the take's start never shows (a failure the reply reports),
        // the line goes back after a while.
        let weak = Rc::downgrade(app);
        self.retake_timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_secs(5),
            move || {
                if let Some(app) = weak.upgrade() {
                    app.create.retake_since.set(None);
                    app.create.show_header(&app);
                }
            },
        );
        app.call_as::<Retaken>(Method::Retake(None), |app, result| {
            match result {
                Ok(retaken) => {
                    for note in &retaken.notes {
                        eprintln!("retake: {note}");
                    }
                }
                Err(error) => {
                    app.create.retake_since.set(None);
                    app.create.retake_timer.stop();
                    app.failed("Couldn’t retake", &error);
                }
            }
            app.create.show_header(app);
            app.refresh_status(true);
        });
    }

    fn remove(&self, app: &Rc<App>, id: &str) {
        if let Some(t) = self.timeline.borrow_mut().as_mut() {
            t.clips.retain(|c| c.id != id);
            renumber(&mut t.clips);
            t.duration_seconds = t.clips.iter().map(|c| c.duration_seconds).sum();
        }
        if self.selected.borrow().as_deref() == Some(id) {
            *self.selected.borrow_mut() = None;
        }
        self.show(app);
        app.call(
            Method::TimelineRemove(TimelineRemoveParams {
                clip: id.to_string(),
                timeline: None,
            }),
            |app, result| {
                if let Err(error) = result {
                    app.failed("Couldn’t remove the clip", &error);
                    app.create.load(app);
                }
            },
        );
    }

    fn move_clip(&self, app: &Rc<App>, id: &str, index: usize) {
        if let Some(t) = self.timeline.borrow_mut().as_mut()
            && let Some(from) = t.clips.iter().position(|c| c.id == id)
        {
            let clip = t.clips.remove(from);
            t.clips.insert(index.min(t.clips.len()), clip);
            renumber(&mut t.clips);
        }
        self.show(app);
        app.call(
            Method::TimelineMove(TimelineMoveParams {
                clip: id.to_string(),
                index,
                timeline: None,
            }),
            |app, result| {
                if let Err(error) = result {
                    app.failed("Couldn’t move the clip", &error);
                    app.create.load(app);
                }
            },
        );
    }

    /// Export the current timeline; `vertical`: with the 9:16 file too.
    fn export(&self, app: &Rc<App>, vertical: bool) {
        if self.export.borrow().is_some() {
            return;
        }
        *self.export.borrow_mut() = Some((String::new(), 0.0));
        self.show_header(app);
        let params = vertical.then(|| TimelineExportParams {
            vertical: true,
            ..Default::default()
        });
        app.call_as::<ExportStarted>(Method::TimelineExport(params), |app, result| {
            match result {
                Ok(started) => {
                    let mut export = app.create.export.borrow_mut();
                    // Progress may have come before the reply.
                    let progress = export.as_ref().map_or(0.0, |(_, p)| *p);
                    if export.is_some() {
                        *export = Some((started.export, progress));
                    }
                }
                Err(error) => {
                    *app.create.export.borrow_mut() = None;
                    app.failed("Couldn’t export the timeline", &error);
                }
            }
            app.create.show_header(app);
        });
    }

    /// Export › For Final Cut Pro… (and Premiere Pro, Resolve,
    /// OpenTimelineIO): ask where, then write the project file
    /// (`timeline_export_nle`), which points at the takes' own files.
    fn export_nle(&self, app: &Rc<App>, target: &str) {
        let Some(name) = self.timeline.borrow().as_ref().map(|t| t.name.clone()) else {
            return;
        };
        let (format, nle_app) = match target {
            "final_cut" => (NleFormat::Fcpxml, NleApp::FinalCut),
            "premiere" => (NleFormat::Xmeml, NleApp::Premiere),
            "resolve" => (NleFormat::Xmeml, NleApp::Resolve),
            _ => (NleFormat::Otio, NleApp::Resolve),
        };
        let label = match format {
            NleFormat::Otio => String::new(),
            _ => format!(" {}", nle_app.title()),
        };
        let file = format!(
            "{}{label}.{}",
            name.replace(['/', ':'], "-"),
            format.extension()
        );
        let path = match crate::platform::save(&file, &[format.extension()], "Export") {
            Some(Some(path)) => path,
            Some(None) => return,
            None => {
                app.toast(
                    ToastKind::Info,
                    "No file panel here",
                    format!(
                        "Use `isocast timeline export --nle {}` instead.",
                        format.name()
                    ),
                );
                return;
            }
        };
        let params = TimelineExportNleParams {
            format,
            app: Some(nle_app),
            timeline: None,
            folder: None,
            path: Some(PathBuf::from(path)),
            // The save panel asked before replacing a file.
            replace: true,
        };
        app.call_as::<NleExported>(
            Method::TimelineExportNle(params),
            |app, result| match result {
                Ok(exported) => {
                    let shown = exported.path.clone();
                    let mut detail = format!(
                        "{} video and {} audio tracks · {}",
                        exported.video_tracks.len(),
                        exported.audio_tracks.len(),
                        crate::platform::abbreviate_home(&exported.path.display().to_string())
                    );
                    if !exported.notes.is_empty() {
                        detail.push_str(&format!(" · {}", exported.notes.join(" · ")));
                    }
                    app.show_toast(
                        Toast::new(
                            ToastKind::Success,
                            format!("Exported for {}", exported.app.title()),
                            detail,
                        )
                        .action("Show in Finder", move |_| {
                            crate::platform::reveal(&shown.display().to_string())
                        }),
                    );
                }
                Err(error) => app.failed("Couldn’t export the timeline", &error),
            },
        );
    }

    /// The menu of timelines: New, each one (the current one checked),
    /// Close.
    fn timelines_menu(&self) -> MenuBuilder {
        let mut menu = MenuBuilder::new().item("new", "New Timeline").icon("plus");
        let list = self.list.borrow();
        if let Some(list) = list.as_ref()
            && !list.timelines.is_empty()
        {
            menu = menu.separator().heading("Open");
            for t in &list.timelines {
                let label = format!(
                    "{} · {} take{} · {}",
                    t.name,
                    t.clips,
                    if t.clips == 1 { "" } else { "s" },
                    mm_ss(t.duration_seconds)
                );
                menu = menu
                    .item(&format!("open:{}", t.id), &label)
                    .checked(t.current);
            }
        }
        menu.separator()
            .item("close", "Close Timeline")
            .disabled(self.timeline.borrow().is_none())
    }

    fn clip_menu(&self, id: &str) -> Option<MenuBuilder> {
        let timeline = self.timeline.borrow();
        let clips = &timeline.as_ref()?.clips;
        let index = clips.iter().position(|c| c.id == id)?;
        let file = program_file(&clips[index]).is_some();
        // Its take isn't where the timeline says (#88).
        let offline = clips[index].problem.is_some();
        let menu = MenuBuilder::new()
            .item("editor", "Open in Editor")
            .separator()
            .item("reveal", "Show in Finder")
            .disabled(!file);
        let menu = match offline {
            true => menu.item("relink", "Relink…"),
            false => menu,
        };
        Some(
            menu.separator()
                .item("earlier", "Move Earlier")
                .disabled(index == 0)
                .item("later", "Move Later")
                .disabled(index + 1 >= clips.len())
                .separator()
                .item("remove", "Remove from Timeline")
                .danger(),
        )
    }

    fn clip_chosen(&self, app: &Rc<App>, clip: &str, id: &str) {
        let (index, count, file) = {
            let timeline = self.timeline.borrow();
            let Some(t) = timeline.as_ref() else { return };
            let Some(index) = t.clips.iter().position(|c| c.id == clip) else {
                return;
            };
            (
                index,
                t.clips.len(),
                program_file(&t.clips[index]).map(Path::to_path_buf),
            )
        };
        match id {
            "reveal" => {
                if let Some(file) = file {
                    crate::platform::reveal(&file.display().to_string());
                }
            }
            "earlier" if index > 0 => self.move_clip(app, clip, index - 1),
            "later" if index + 1 < count => self.move_clip(app, clip, index + 1),
            "remove" => self.remove(app, clip),
            "relink" => app.projects.relink(app, clip),
            "editor" => {
                if let Err(error) = self.open_editor(app) {
                    eprintln!("editor: {error:#}");
                }
            }
            _ => {}
        }
    }

    fn toggle(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<CreateArea>();
        let on = !area.get_active();
        area.set_active(on);
        if !on {
            // Leaving the view stops playing.
            self.shuttle.set(0.0);
            self.play_timer.stop();
            self.playback.take();
        }
        if !app.mode.offscreen {
            crate::prefs::save(&window);
        }
        if on {
            self.load(app);
        }
    }

    /// Where clip `from`, dragged with its middle at `seconds`, would go:
    /// after every other clip whose middle is before it.
    fn drop_index(&self, from: usize, seconds: f64) -> usize {
        let timeline = self.timeline.borrow();
        let Some(t) = timeline.as_ref() else {
            return from;
        };
        t.clips
            .iter()
            .enumerate()
            .filter(|(i, c)| *i != from && c.start_seconds + c.duration_seconds / 2.0 < seconds)
            .count()
    }

    /// Where that place starts on the timeline as it is now: the start of
    /// the clip it goes before, or the end of the last.
    fn drop_seconds(&self, from: usize, seconds: f64) -> f64 {
        let to = self.drop_index(from, seconds);
        let timeline = self.timeline.borrow();
        let Some(t) = timeline.as_ref() else {
            return 0.0;
        };
        let others: Vec<&ClipInfo> = t
            .clips
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != from)
            .map(|(_, c)| c)
            .collect();
        match others.get(to) {
            Some(c) => c.start_seconds,
            None => t.duration_seconds,
        }
    }

    /// Give a window's CreateArea (the studio's or the editor's) the
    /// clips and tracks, and connect its callbacks.
    fn wire(&self, app: &Rc<App>, area: &CreateArea) {
        area.set_clips(ModelRc::from(self.rows.clone()));
        area.set_tracks(ModelRc::from(self.tracks.clone()));
        let weak = Rc::downgrade(app);
        area.on_scrubbed(move || {
            if let Some(app) = weak.upgrade() {
                app.create.show_preview(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_split(move || {
            if let Some(app) = weak.upgrade() {
                app.create.split(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_delete_range(move || {
            if let Some(app) = weak.upgrade() {
                app.create.delete_range(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_set_angle(move |source| {
            if let Some(app) = weak.upgrade() {
                app.create.set_angle(&app, &source);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_trim(move |id, out, delta| {
            if let Some(app) = weak.upgrade() {
                app.create.trim(&app, &id, out, f64::from(delta));
            }
        });
        let weak = Rc::downgrade(app);
        area.on_undo(move || {
            if let Some(app) = weak.upgrade() {
                app.create.step(&app, false);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_redo(move || {
            if let Some(app) = weak.upgrade() {
                app.create.step(&app, true);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_toggle_view(move || {
            if let Some(app) = weak.upgrade() {
                app.create.toggle(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_select(move |id| {
            if let Some(app) = weak.upgrade() {
                app.create.select(&app, &id);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_move_clip(move |id, index| {
            if let Some(app) = weak.upgrade() {
                app.create.move_clip(&app, &id, index.max(0) as usize);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_remove_selected(move || {
            if let Some(app) = weak.upgrade() {
                let id = app.create.selected.borrow().clone();
                if let Some(id) = id {
                    app.create.remove(&app, &id);
                }
            }
        });
        let weak = Rc::downgrade(app);
        area.on_retake(move || {
            if let Some(app) = weak.upgrade() {
                app.create.retake(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_record_here_chosen(move |here| {
            if let Some(app) = weak.upgrade() {
                let id = app.create.timeline.borrow().as_ref().map(|t| t.id.clone());
                match (here, id) {
                    (true, Some(id)) => super::controls::choose_target(&app, &id),
                    _ => super::controls::choose_target(&app, "new"),
                }
            }
        });
        let weak = Rc::downgrade(app);
        area.on_export(move || {
            if let Some(app) = weak.upgrade() {
                app.create.export(&app, false);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_export_nle(move |target| {
            if let Some(app) = weak.upgrade() {
                app.create.export_nle(&app, &target);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_reveal_export(move || {
            if let Some(app) = weak.upgrade()
                && let Some(path) = app.create.exported.borrow().as_ref()
            {
                crate::platform::reveal(&path.display().to_string());
            }
        });
        let weak = Rc::downgrade(app);
        area.on_new_timeline(move || {
            if let Some(app) = weak.upgrade() {
                app.create.new_timeline(&app, None);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_drop_index(move |from, seconds| {
            weak.upgrade().map_or(from, |app| {
                app.create
                    .drop_index(from.max(0) as usize, f64::from(seconds)) as i32
            })
        });
        let weak = Rc::downgrade(app);
        area.on_drop_seconds(move |from, seconds| {
            weak.upgrade().map_or(0.0, |app| {
                app.create
                    .drop_seconds(from.max(0) as usize, f64::from(seconds)) as f32
            })
        });
        let weak = Rc::downgrade(app);
        area.on_key(move |key| {
            if let Some(app) = weak.upgrade() {
                app.create.edit_key(&app, &key);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_snap(move |seconds, pps, to_playhead| {
            weak.upgrade().map_or(seconds, |app| {
                app.create
                    .snap(&app, f64::from(seconds), f64::from(pps), to_playhead)
                    as f32
            })
        });
        let weak = Rc::downgrade(app);
        area.on_roll(move |id, seconds| {
            if let Some(app) = weak.upgrade() {
                app.create.roll(&app, &id, f64::from(seconds));
            }
        });
        let weak = Rc::downgrade(app);
        area.on_crop_press(move |handle| {
            if let Some(app) = weak.upgrade() {
                app.create.crop_press(&app, &handle);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_crop_drag(move |dx, dy| {
            if let Some(app) = weak.upgrade() {
                app.create.crop_move(&app, f64::from(dx), f64::from(dy));
            }
        });
        let weak = Rc::downgrade(app);
        area.on_crop_release(move || {
            if let Some(app) = weak.upgrade() {
                app.create.crop_release(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_crop_preset_chosen(move |preset| {
            if let Some(app) = weak.upgrade() {
                app.create.crop_preset(&app, &preset);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_crop_clear(move || {
            if let Some(app) = weak.upgrade() {
                app.create.crop_with(&app, None, None, true);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_open_editor(move || {
            if let Some(app) = weak.upgrade()
                && let Err(error) = app.create.open_editor(&app)
            {
                eprintln!("editor: {error:#}");
            }
        });
    }

    // ---- The Edit view (C.3)

    /// The playhead in seconds (at the end unless placed).
    fn playhead(&self, app: &Rc<App>) -> f64 {
        let total = self
            .timeline
            .borrow()
            .as_ref()
            .map_or(0.0, |t| t.duration_seconds);
        let at = self
            .surface(app)
            .map_or(-1.0, |s| f64::from(s.area().get_playhead()));
        if at < 0.0 { total } else { at.min(total) }
    }

    fn set_playhead(&self, app: &Rc<App>, seconds: f64) {
        if let Some(surface) = self.surface(app) {
            surface.area().set_playhead(seconds.max(0.0) as f32);
        }
        self.show_preview(app);
    }

    /// The clip at `seconds` on the timeline (the last at its end).
    fn clip_at(&self, seconds: f64) -> Option<ClipInfo> {
        let timeline = self.timeline.borrow();
        let clips = &timeline.as_ref()?.clips;
        clips
            .iter()
            .find(|c| seconds < c.start_seconds + c.duration_seconds)
            .or(clips.last())
            .cloned()
    }

    /// Ask for the picture at the playhead: the clip's angle there, at
    /// that moment of its take.
    fn show_preview(&self, app: &Rc<App>) {
        let Some(surface) = self.surface(app) else {
            return;
        };
        let area = surface.area();
        let at = self.playhead(app);
        let Some(clip) = self.clip_at(at) else {
            area.set_has_preview(false);
            area.set_preview_offline(false);
            area.set_preview_label("".into());
            self.preview.reset();
            return;
        };
        let source = self.show_label(app, at).unwrap_or_default();
        let into = (at - clip.start_seconds).clamp(0.0, clip.duration_seconds);
        // The frame on screen then: just inside the clip at its end.
        let take = clip.in_seconds + into.min(clip.duration_seconds - 1e-3).max(0.0);
        // Offline: not recorded, or gone since the timeline was read.
        let Some(file) = file_of(&clip, &source).filter(|f| f.path.exists()) else {
            area.set_has_preview(false);
            area.set_preview_offline(true);
            self.preview.reset();
            return;
        };
        area.set_preview_offline(false);
        let seconds = (take - file.start_offset_seconds.unwrap_or(0.0)).max(0.0);
        if self.preview.request(&file.path, seconds) {
            self.asked_at.set(Some(Instant::now()));
            self.collect_preview(app);
        }
    }

    /// The viewer's label and crop for the playhead at `at`; the source
    /// shown there.
    fn show_label(&self, app: &Rc<App>, at: f64) -> Option<String> {
        let surface = self.surface(app)?;
        let area = surface.area();
        let clip = self.clip_at(at)?;
        let source = clip.angle.clone().unwrap_or_else(|| PROGRAM.to_string());
        self.show_crop(&area, &clip, &source);
        let name = match source.as_str() {
            PROGRAM => "Program".to_string(),
            other => source_name(app, other),
        };
        let cropped = match clip.crop {
            Some(_) => " · cropped",
            None => "",
        };
        area.set_preview_label(format!("{name} · {}{cropped}", mm_ss_cc(at)).into());
        Some(source)
    }

    /// Take in the preview's picture once made (every 8 ms until then).
    fn collect_preview(&self, app: &Rc<App>) {
        if self.preview_timer.running() {
            return;
        }
        let weak = Rc::downgrade(app);
        self.preview_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(8),
            move || {
                let Some(app) = weak.upgrade() else { return };
                if let Some(frame) = app.create.preview.collect() {
                    app.create.preview_timer.stop();
                    app.create.shown(&app, frame);
                }
            },
        );
    }

    fn shown(&self, app: &Rc<App>, frame: Option<slint::Image>) {
        if let Some(asked) = self.asked_at.take()
            && std::env::var_os("ISOCAST_PREVIEW_STATS").is_some()
        {
            eprintln!(
                "preview: shown {:.1} ms after it was asked for",
                asked.elapsed().as_secs_f64() * 1000.0
            );
        }
        let Some(surface) = self.surface(app) else {
            return;
        };
        let area = surface.area();
        match frame {
            Some(image) => {
                area.set_preview(image);
                area.set_has_preview(true);
            }
            None => area.set_has_preview(false),
        }
    }

    /// The marked span, if both marks are set.
    fn marks(&self, app: &Rc<App>) -> Option<(f64, f64)> {
        let surface = self.surface(app)?;
        let area = surface.area();
        let (a, b) = (
            f64::from(area.get_mark_in()),
            f64::from(area.get_mark_out()),
        );
        (a >= 0.0 && b > a).then_some((a, b))
    }

    fn clear_marks(&self, app: &Rc<App>) {
        if let Some(surface) = self.surface(app) {
            let area = surface.area();
            area.set_mark_in(-1.0);
            area.set_mark_out(-1.0);
        }
    }

    /// An edit's reply: show the timeline it returns at once (the event's
    /// read confirms it), or say why it couldn't be done.
    fn edit(&self, app: &Rc<App>, method: Method, failed: &'static str) {
        app.call_as::<TimelineInfo>(method, move |app, result| match result {
            Ok(info) => {
                for note in &info.notes {
                    eprintln!("edit: {note}");
                }
                app.create.loaded(app, Some(info));
            }
            Err(error) => {
                // A dropped trim springs back.
                app.create.bump_version(app);
                app.failed(failed, &error)
            }
        });
    }

    /// The timeline was read again (or an edit failed): see
    /// CreateArea.timeline-version.
    fn bump_version(&self, app: &Rc<App>) {
        if let Some(window) = app.window() {
            let area = window.global::<CreateArea>();
            area.set_timeline_version(area.get_timeline_version().wrapping_add(1));
        }
        if let Some(editor) = self.editor.borrow().as_ref() {
            let area = editor.global::<CreateArea>();
            area.set_timeline_version(area.get_timeline_version().wrapping_add(1));
        }
    }

    fn split(&self, app: &Rc<App>) {
        let at = self.playhead(app);
        self.edit(
            app,
            Method::TimelineSplit(TimelineSplitParams {
                at_seconds: at,
                timeline: None,
            }),
            "Couldn’t split the clip",
        );
    }

    fn delete_range(&self, app: &Rc<App>) {
        let Some((start, end)) = self.marks(app) else {
            return;
        };
        self.clear_marks(app);
        self.set_playhead(app, start);
        self.edit(
            app,
            Method::TimelineDeleteRange(TimelineRangeParams {
                start_seconds: start,
                end_seconds: end,
                timeline: None,
            }),
            "Couldn’t delete the span",
        );
    }

    /// Show `source` over the marked span, else the selected clip, else the
    /// clip under the playhead.
    fn set_angle(&self, app: &Rc<App>, source: &str) {
        let span = self.marks(app).or_else(|| {
            let timeline = self.timeline.borrow();
            let clips = &timeline.as_ref()?.clips;
            let selected = self.selected.borrow();
            let clip = match selected.as_deref() {
                Some(id) => clips.iter().find(|c| c.id == id).cloned(),
                None => None,
            };
            drop(timeline);
            let clip = clip.or_else(|| self.clip_at(self.playhead(app)))?;
            Some((
                clip.start_seconds,
                clip.start_seconds + clip.duration_seconds,
            ))
        });
        let Some((start, end)) = span else { return };
        self.edit(
            app,
            Method::TimelineAngle(TimelineAngleParams {
                start_seconds: start,
                end_seconds: end,
                source: source.to_string(),
                timeline: None,
            }),
            "Couldn’t change the angle",
        );
    }

    /// Angle key `n`: 1 the program, 2 the first ISO track, and so on.
    fn angle_key(&self, app: &Rc<App>, n: usize) {
        let source = match n {
            0 | 1 => Some(PROGRAM.to_string()),
            n => slint::Model::row_data(&*self.tracks, n - 2).map(|track| track.source.to_string()),
        };
        if let Some(source) = source {
            self.set_angle(app, &source);
        }
    }

    /// Move clip `id`'s in (or out) point by `delta` seconds, within its
    /// take.
    fn trim(&self, app: &Rc<App>, id: &str, out: bool, delta: f64) {
        let params = {
            let timeline = self.timeline.borrow();
            let Some(clip) = timeline
                .as_ref()
                .and_then(|t| t.clips.iter().find(|c| c.id == id))
            else {
                return;
            };
            let frame = 1.0 / f64::from(clip.fps.max(1));
            let length = take_length(clip);
            match out {
                false => TimelineTrimParams {
                    clip: id.to_string(),
                    in_seconds: Some(
                        (clip.in_seconds + delta).clamp(0.0, clip.out_seconds - frame),
                    ),
                    out_seconds: None,
                    timeline: None,
                },
                true => TimelineTrimParams {
                    clip: id.to_string(),
                    in_seconds: None,
                    out_seconds: Some(
                        (clip.out_seconds + delta).clamp(clip.in_seconds + frame, length),
                    ),
                    timeline: None,
                },
            }
        };
        self.edit(app, Method::TimelineTrim(params), "Couldn’t trim the clip");
    }

    fn step(&self, app: &Rc<App>, redo: bool) {
        let method = match redo {
            false => Method::TimelineUndo(None),
            true => Method::TimelineRedo(None),
        };
        self.edit(app, method, "Couldn’t undo");
    }

    /// The editing surface: the editor window, else the studio window.
    fn surface(&self, app: &Rc<App>) -> Option<Surface> {
        if let Some(editor) = self.editor.borrow().as_ref() {
            return Some(Surface::Editor(editor.clone_strong()));
        }
        app.window().map(Surface::Studio)
    }

    /// Open the editor window on the current timeline (or bring it
    /// forward).
    fn open_editor(&self, app: &Rc<App>) -> anyhow::Result<()> {
        self.open_editor_as(app, Raise::Focus)
    }

    /// Open (or show) the editor window, raised as `raise` says (#88: a
    /// project opened, or a recording stopped).
    pub fn open_editor_as(&self, app: &Rc<App>, raise: Raise) -> anyhow::Result<()> {
        let opened = self.editor.borrow().is_some();
        if !opened {
            let editor = EditorWindow::new()?;
            {
                let area = editor.global::<CreateArea>();
                self.wire(app, &area);
                area.set_active(true);
            }
            if app.mode.offscreen {
                editor.set_native_menu(false);
            }
            // Its File menu (#88), and its own menus (#94).
            app.projects.connect_editor(app, &editor);
            app.connect_editor_menus(&editor);
            // Playing, each frame the display's refresh asks for shows the
            // picture due when it is on screen, so pictures change in step
            // with the display (#94): a timer's ticks drift against it (a
            // 30 fps picture held 1, 2 or 3 refreshes).
            let weak = Rc::downgrade(app);
            let notified = editor.window().set_rendering_notifier(move |state, _| {
                if !matches!(state, slint::RenderingState::BeforeRendering) {
                    return;
                }
                let Some(app) = weak.upgrade() else { return };
                let shown = app.create.refresh.borrow().as_ref().map(|r| r.drawing());
                if let Some(shown) = shown
                    && app.create.shuttle.get() != 0.0
                {
                    app.create.play_step(&app, shown);
                }
            });
            if let Err(error) = notified {
                // Then no display refresh drives it: the timer steps.
                eprintln!("editor: no rendering notifier ({error:?}); playing follows a timer");
                self.no_notifier.set(true);
            }
            *self.editor.borrow_mut() = Some(editor);
        }
        if let Some(editor) = self.editor.borrow().as_ref() {
            editor.show()?;
            raised(app, editor, raise);
        }
        if !opened {
            self.load(app);
        }
        Ok(())
    }

    /// The editor window, if it was opened (the motion harness drives it).
    pub fn editor_window(&self) -> Option<EditorWindow> {
        self.editor.borrow().as_ref().map(|e| e.clone_strong())
    }

    /// The current timeline's name.
    pub fn timeline_name(&self) -> Option<String> {
        self.timeline.borrow().as_ref().map(|t| t.name.clone())
    }

    /// Clips on the current timeline.
    pub fn clip_count(&self) -> usize {
        self.timeline.borrow().as_ref().map_or(0, |t| t.clips.len())
    }

    /// Clip `index`'s in and out points (the motion harness's fixed-length
    /// takes).
    pub fn clip_points(&self, index: usize) -> Option<(f64, f64)> {
        let timeline = self.timeline.borrow();
        let clip = timeline.as_ref()?.clips.get(index)?;
        Some((clip.in_seconds, clip.out_seconds))
    }

    /// Trim clip `index` to `in_seconds`–`out_seconds` of its take (the
    /// motion harness: every run's clips the same length, #94).
    pub fn trim_to(&self, app: &Rc<App>, index: usize, in_seconds: f64, out_seconds: f64) {
        let Some(id) = self
            .timeline
            .borrow()
            .as_ref()
            .and_then(|t| t.clips.get(index))
            .map(|c| c.id.clone())
        else {
            return;
        };
        self.edit(
            app,
            Method::TimelineTrim(TimelineTrimParams {
                clip: id,
                in_seconds: Some(in_seconds),
                out_seconds: Some(out_seconds),
                timeline: None,
            }),
            "Couldn’t trim the clip",
        );
    }

    /// Every timeline as last read (the record target's menu, #114).
    pub fn timelines(&self) -> Vec<isocast_protocol::timeline::TimelineSummary> {
        self.list
            .borrow()
            .as_ref()
            .map(|list| list.timelines.clone())
            .unwrap_or_default()
    }

    /// Whether the editor window is open (offscreen snapshots draw it).
    pub fn editor_open(&self) -> bool {
        self.editor
            .borrow()
            .as_ref()
            .is_some_and(|editor| editor.window().is_visible())
    }

    /// J (back), K (stop), L (forward): each J or L press plays faster in
    /// its direction (1, 2, 4 times), K stops.
    fn shuttle(&self, app: &Rc<App>, key: &str) {
        let now = self.shuttle.get();
        let next = match key {
            "l" if now > 0.0 => (now * 2.0).min(4.0),
            "l" => 1.0,
            "j" if now < 0.0 => (now * 2.0).max(-4.0),
            "j" => -1.0,
            _ => 0.0,
        };
        self.shuttle.set(next);
        let origin = Instant::now();
        self.played.set(Some(origin));
        if next > 0.0 {
            self.start_playback(app, origin);
        } else if self.playback.take().is_some() {
            // The exact picture where it stopped (or J's first).
            self.show_preview(app);
        }
        if next == 0.0 {
            self.stop_stepping();
        } else {
            self.keep_stepping(app);
        }
        self.show_header(app);
    }

    /// Step the playhead while playing: at the editor window's display
    /// refresh (#94), else on a timer.
    fn keep_stepping(&self, app: &Rc<App>) {
        let editor = self.editor.borrow().as_ref().map(|e| e.clone_strong());
        if let Some(editor) = editor
            && !app.mode.offscreen
            && !self.no_notifier.get()
            && editor.window().is_visible()
        {
            if self.refresh.borrow().is_none() {
                let weak = editor.as_weak();
                let refresh = crate::refresh::Refresh::start(move || {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(editor) = weak.upgrade() {
                            editor.window().request_redraw();
                        }
                    });
                });
                if refresh.is_none() {
                    eprintln!("editor: no display link; playing follows a timer");
                }
                *self.refresh.borrow_mut() = refresh;
            }
            if self.refresh.borrow().is_some() {
                self.play_timer.stop();
                // A frame asked for and not drawn (minimised, covered):
                // the playhead moves on anyway, and the link asks again.
                let weak = Rc::downgrade(app);
                self.stall_timer.start(
                    slint::TimerMode::Repeated,
                    Duration::from_millis(100),
                    move || {
                        let Some(app) = weak.upgrade() else { return };
                        let stalled = app
                            .create
                            .refresh
                            .borrow()
                            .as_ref()
                            .is_some_and(|r| r.stalled(Duration::from_millis(100)));
                        if stalled && app.create.shuttle.get() != 0.0 {
                            app.create.play_step(&app, Instant::now());
                        }
                    },
                );
                return;
            }
        }
        if !self.play_timer.running() {
            let weak = Rc::downgrade(app);
            self.play_timer
                .start(slint::TimerMode::Repeated, PLAY_EVERY, move || {
                    if let Some(app) = weak.upgrade() {
                        app.create.play_step(&app, Instant::now());
                    }
                });
        }
    }

    fn stop_stepping(&self) {
        self.play_timer.stop();
        self.stall_timer.stop();
        self.refresh.take();
    }

    /// Move the playhead to where it is at `now` (when the frame being
    /// drawn shows), and show the picture due then.
    fn play_step(&self, app: &Rc<App>, now: Instant) {
        let previous = self.played.replace(Some(now));
        let since = previous.map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
        let total = self
            .timeline
            .borrow()
            .as_ref()
            .map_or(0.0, |t| t.duration_seconds);
        let from = self.playhead(app);
        if self.playback.borrow().is_some() && (from - self.played_to.get()).abs() > 1e-3 {
            // The playhead was moved (a click, a key): play from there.
            self.start_playback(app, previous.unwrap_or(now));
        }
        let at = from + since * self.shuttle.get();
        let stop = at <= 0.0 || at >= total;
        if stop {
            self.shuttle.set(0.0);
            self.stop_stepping();
            self.show_header(app);
        }
        let at = at.clamp(0.0, total);
        if !stop && self.playback.borrow().is_some() {
            // Forward: the playhead, and the decoded picture due now.
            if let Some(surface) = self.surface(app) {
                surface.area().set_playhead(at as f32);
            }
            self.played_to.set(self.playhead(app));
            let frame = self
                .playback
                .borrow()
                .as_ref()
                .and_then(|playback| playback.frame_at(at));
            if let Some((shown, image)) = frame {
                self.show_label(app, at);
                if std::env::var_os("ISOCAST_PREVIEW_STATS").is_some() {
                    eprintln!(
                        "play: {:.3} ms picture {shown:.4} at {at:.4}",
                        stats_clock().elapsed().as_secs_f64() * 1000.0
                    );
                }
                // No picture: the clip's file is offline from here.
                let offline = image.is_none();
                self.shown(app, image);
                if let Some(surface) = self.surface(app) {
                    surface.area().set_preview_offline(offline);
                }
            }
            return;
        }
        self.playback.take();
        self.set_playhead(app, at);
    }

    /// Decode the timeline ahead from the playhead (forward play, #94):
    /// each clip's angle's file over the clip; and play its sound (the
    /// program's, as the edited file has) from `origin`, when the
    /// playhead was where it is.
    fn start_playback(&self, app: &Rc<App>, origin: Instant) {
        let from = self.playhead(app);
        let timeline = self.timeline.borrow();
        let clips: Vec<&ClipInfo> = timeline
            .as_ref()
            .map(|t| t.clips.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter(|clip| clip.start_seconds + clip.duration_seconds > from)
            .collect();
        let sound = self.sound_plan(app, &clips, from, origin);
        self.silenced
            .set(sound.is_none() && capturing(&app.session.borrow()));
        let segments: Vec<Segment> = clips
            .iter()
            .map(|clip| {
                let source = clip.angle.as_deref().unwrap_or(PROGRAM);
                let start = clip.start_seconds.max(from);
                let file = file_of(clip, source);
                Segment {
                    path: file.map(|f| f.path.clone()),
                    start,
                    end: clip.start_seconds + clip.duration_seconds,
                    file_seconds: clip.in_seconds + (start - clip.start_seconds)
                        - file.and_then(|f| f.start_offset_seconds).unwrap_or(0.0),
                }
            })
            .collect();
        drop(timeline);
        let sound = sound.and_then(crate::sound::Sound::start);
        *self.playback.borrow_mut() = Some(Playback::start(segments, sound));
        self.played_to.set(from);
    }

    /// A recording, stream or replay buffer started or stopped taking in
    /// sound that would include the editor's (#94): its sound stops at
    /// once, and comes back (from where the playhead is) once none does.
    fn follow_capture(&self, app: &Rc<App>) {
        let capturing = capturing(&app.session.borrow());
        if self.playback.borrow().is_none() {
            self.silenced.set(false);
            return;
        }
        if capturing {
            if let Some(playback) = self.playback.borrow_mut().as_mut() {
                playback.set_sound(None);
            }
            self.silenced.set(true);
        } else if self.silenced.replace(false) {
            let origin = self.played.get().unwrap_or_else(Instant::now);
            let from = self.playhead(app);
            let plan = {
                let timeline = self.timeline.borrow();
                let clips: Vec<&ClipInfo> = timeline
                    .as_ref()
                    .map(|t| t.clips.as_slice())
                    .unwrap_or(&[])
                    .iter()
                    .filter(|clip| clip.start_seconds + clip.duration_seconds > from)
                    .collect();
                self.sound_plan(app, &clips, from, origin)
            };
            let sound = plan.and_then(crate::sound::Sound::start);
            if let Some(playback) = self.playback.borrow_mut().as_mut() {
                playback.set_sound(sound);
            }
        }
    }

    /// What the editor plays as it plays forward: each clip's program
    /// sound through the monitoring device. None offscreen, with
    /// ISOCAST_MONITOR_SILENT (tests), outside the editor window, or while
    /// a recording, stream or replay buffer takes in sound that would
    /// include it ([`capturing`]).
    fn sound_plan(
        &self,
        app: &Rc<App>,
        clips: &[&ClipInfo],
        from: f64,
        origin: Instant,
    ) -> Option<crate::sound::Plan> {
        if app.mode.offscreen
            || std::env::var_os("ISOCAST_MONITOR_SILENT").is_some()
            || self.editor.borrow().is_none()
        {
            return None;
        }
        let session = app.session.borrow();
        let setup = session.setup.clone()?;
        if capturing(&session) {
            return None;
        }
        let stretches = clips
            .iter()
            .map(|clip| {
                let start = clip.start_seconds.max(from);
                let file = file_of(clip, PROGRAM);
                crate::sound::Stretch {
                    path: file.map(|f| f.path.clone()),
                    start,
                    end: clip.start_seconds + clip.duration_seconds,
                    file_seconds: clip.in_seconds + (start - clip.start_seconds)
                        - file.and_then(|f| f.start_offset_seconds).unwrap_or(0.0),
                }
            })
            .collect();
        Some(crate::sound::Plan {
            stretches,
            from,
            origin,
            speed: self.shuttle.get(),
            device: setup.audio.monitoring_device.clone(),
        })
    }

    /// Step the playhead by `frames` of the clip there.
    fn step_frames(&self, app: &Rc<App>, frames: i32) {
        let at = self.playhead(app);
        let fps = self.clip_at(at).map_or(30, |c| c.fps.max(1));
        let total = self
            .timeline
            .borrow()
            .as_ref()
            .map_or(0.0, |t| t.duration_seconds);
        let frame = 1.0 / f64::from(fps);
        let snapped = (at / frame).round() * frame;
        self.set_playhead(app, (snapped + f64::from(frames) * frame).clamp(0.0, total));
    }

    fn mark(&self, app: &Rc<App>, out: bool) {
        let at = self.playhead(app);
        if let Some(surface) = self.surface(app) {
            let area = surface.area();
            match out {
                false => {
                    area.set_mark_in(at as f32);
                    if f64::from(area.get_mark_out()) <= at {
                        area.set_mark_out(-1.0);
                    }
                }
                true => {
                    area.set_mark_out(at as f32);
                    if f64::from(area.get_mark_in()) >= at {
                        area.set_mark_in(-1.0);
                    }
                }
            }
        }
    }

    /// A key's command from the window (`edit:…`).
    fn edit_key(&self, app: &Rc<App>, key: &str) -> bool {
        if self.timeline.borrow().is_none() {
            return false;
        }
        match key {
            "s" => self.split(app),
            "i" => self.mark(app, false),
            "o" => self.mark(app, true),
            "j" | "k" | "l" => self.shuttle(app, key),
            "space" => {
                let key = if self.shuttle.get() != 0.0 { "k" } else { "l" };
                self.shuttle(app, key);
            }
            "up" | "down" => self.go_to(app, key == "down", false),
            "marker:1" | "marker:-1" => self.go_to(app, key == "marker:1", true),
            "home" => self.set_playhead(app, 0.0),
            "end" => {
                let total = self
                    .timeline
                    .borrow()
                    .as_ref()
                    .map_or(0.0, |t| t.duration_seconds);
                self.set_playhead(app, total);
            }
            "go-in" | "go-out" => {
                if let Some(surface) = self.surface(app) {
                    let area = surface.area();
                    let mark = match key {
                        "go-in" => area.get_mark_in(),
                        _ => area.get_mark_out(),
                    };
                    if mark >= 0.0 {
                        self.set_playhead(app, f64::from(mark));
                    }
                }
            }
            "snap" => {
                if let Some(surface) = self.surface(app) {
                    let area = surface.area();
                    area.set_snapping(!area.get_snapping());
                }
            }
            "crop" => {
                if let Some(surface) = self.surface(app) {
                    let area = surface.area();
                    area.set_crop_mode(!area.get_crop_mode());
                }
            }
            "clear" => {
                let crop_mode = self.surface(app).is_some_and(|s| s.area().get_crop_mode());
                match crop_mode {
                    true => {
                        if let Some(surface) = self.surface(app) {
                            surface.area().set_crop_mode(false);
                        }
                    }
                    false => self.clear_marks(app),
                }
            }
            "delete" => {
                if self.marks(app).is_some() {
                    self.delete_range(app);
                } else if let Some(id) = self.selected.borrow().clone() {
                    self.remove(app, &id);
                }
            }
            _ => {
                if let Some(n) = key.strip_prefix("angle:").and_then(|n| n.parse().ok()) {
                    self.angle_key(app, n);
                } else if let Some(n) = key.strip_prefix("step:").and_then(|n| n.parse().ok()) {
                    self.step_frames(app, n);
                } else {
                    return false;
                }
            }
        }
        true
    }

    // ---- Round 2 (#87)

    /// The crop of `clip` (showing `source`) on the viewer, unless a
    /// handle is being dragged.
    fn show_crop(&self, area: &CreateArea, clip: &ClipInfo, source: &str) {
        if self.crop_drag.borrow().is_some() {
            return;
        }
        let crop = clip.crop.unwrap_or(Crop::FULL);
        let same = (
            area.get_crop_x(),
            area.get_crop_y(),
            area.get_crop_w(),
            area.get_crop_h(),
        ) == (
            crop.x as f32,
            crop.y as f32,
            crop.width as f32,
            crop.height as f32,
        );
        area.set_has_crop(clip.crop.is_some());
        area.set_crop_x(crop.x as f32);
        area.set_crop_y(crop.y as f32);
        area.set_crop_w(crop.width as f32);
        area.set_crop_h(crop.height as f32);
        if let Some(file) = file_of(clip, source).filter(|f| f.width > 0 && f.height > 0) {
            let picture = f64::from(file.width) / f64::from(file.height);
            area.set_crop_aspect(picture as f32);
            // The shape it has, if it is a preset's (a crop made by the
            // CLI or MCP too), so drags keep it; else free.
            let shape = crop.width * picture / crop.height.max(1e-6);
            let preset = clip.crop.and_then(|_| {
                CropPreset::ALL
                    .into_iter()
                    .find(|p| (p.aspect() / shape - 1.0).abs() < 0.01)
            });
            // (A shape chosen for the crop shown stays chosen.)
            if !same {
                area.set_crop_preset(preset.map_or("free", CropPreset::name).into());
            }
        }
    }

    /// `seconds` snapped to the nearest clip edge, marker, mark (or the
    /// playhead, if `to_playhead`) within 8 points at `pps` points a
    /// second, when snapping is on.
    fn snap(&self, app: &Rc<App>, seconds: f64, pps: f64, to_playhead: bool) -> f64 {
        let Some(surface) = self.surface(app) else {
            return seconds;
        };
        let area = surface.area();
        if !area.get_snapping() || pps <= 0.0 {
            return seconds;
        }
        let mut points = self.edit_points();
        if let Some(t) = self.timeline.borrow().as_ref() {
            points.extend(marker_times(&t.clips));
        }
        for mark in [area.get_mark_in(), area.get_mark_out()] {
            if mark >= 0.0 {
                points.push(f64::from(mark));
            }
        }
        if to_playhead {
            points.push(self.playhead(app));
        }
        let reach = 8.0 / pps;
        points
            .into_iter()
            .map(|p| (p, (p - seconds).abs()))
            .filter(|(_, d)| *d <= reach)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map_or(seconds, |(p, _)| p)
    }

    /// Where clips meet, the timeline's start and its end.
    fn edit_points(&self) -> Vec<f64> {
        let timeline = self.timeline.borrow();
        let Some(t) = timeline.as_ref() else {
            return Vec::new();
        };
        let mut points: Vec<f64> = t.clips.iter().map(|c| c.start_seconds).collect();
        points.push(t.duration_seconds);
        points
    }

    /// Move the playhead to the next (`forward`) or previous edit point,
    /// or (`markers`) marker.
    fn go_to(&self, app: &Rc<App>, forward: bool, markers: bool) {
        let at = self.playhead(app);
        let points = match markers {
            true => self
                .timeline
                .borrow()
                .as_ref()
                .map(|t| marker_times(&t.clips))
                .unwrap_or_default(),
            false => self.edit_points(),
        };
        // Half a frame either way is "here".
        let near = 0.5 / f64::from(self.clip_at(at).map_or(30, |c| c.fps.max(1)));
        let next = match forward {
            true => points
                .into_iter()
                .filter(|p| *p > at + near)
                .reduce(f64::min),
            false => points
                .into_iter()
                .filter(|p| *p < at - near)
                .reduce(f64::max),
        };
        if let Some(next) = next {
            self.set_playhead(app, next);
        }
    }

    /// Roll the cut after clip `id` by `seconds` (an ⌥-drag of an edge).
    fn roll(&self, app: &Rc<App>, id: &str, seconds: f64) {
        self.edit(
            app,
            Method::TimelineRoll(TimelineRollParams {
                clip: id.to_string(),
                by_seconds: seconds,
                timeline: None,
            }),
            "Couldn’t move the cut",
        );
    }

    /// The clip a crop applies to: the one at the playhead.
    fn crop_target(&self, app: &Rc<App>) -> Option<ClipInfo> {
        self.clip_at(self.playhead(app))
    }

    /// Crop the clip at the playhead: to `crop`, shaped by `preset`, or
    /// (`clear`) not at all.
    fn crop_with(
        &self,
        app: &Rc<App>,
        crop: Option<Crop>,
        preset: Option<CropPreset>,
        clear: bool,
    ) {
        let Some(clip) = self.crop_target(app) else {
            return;
        };
        self.edit(
            app,
            Method::TimelineCrop(TimelineCropParams {
                start_seconds: clip.start_seconds,
                end_seconds: clip.start_seconds + clip.duration_seconds,
                crop,
                preset,
                clear,
                timeline: None,
            }),
            "Couldn’t crop the clip",
        );
    }

    fn crop_press(&self, app: &Rc<App>, handle: &str) {
        let Some(surface) = self.surface(app) else {
            return;
        };
        let Some(clip) = self.crop_target(app) else {
            return;
        };
        let area = surface.area();
        let start = Crop {
            x: f64::from(area.get_crop_x()),
            y: f64::from(area.get_crop_y()),
            width: f64::from(area.get_crop_w()),
            height: f64::from(area.get_crop_h()),
        };
        *self.crop_drag.borrow_mut() = Some((handle.to_string(), start, clip.id));
    }

    fn crop_move(&self, app: &Rc<App>, dx: f64, dy: f64) {
        let Some(surface) = self.surface(app) else {
            return;
        };
        let Some((handle, start, _)) = self.crop_drag.borrow().clone() else {
            return;
        };
        let area = surface.area();
        let aspect = CropPreset::parse(&area.get_crop_preset()).map(CropPreset::aspect);
        let crop = dragged_crop(
            &handle,
            start,
            dx,
            dy,
            aspect,
            f64::from(area.get_crop_aspect()),
        );
        area.set_crop_x(crop.x as f32);
        area.set_crop_y(crop.y as f32);
        area.set_crop_w(crop.width as f32);
        area.set_crop_h(crop.height as f32);
    }

    /// A handle let go: crop the clip it was pressed on to where it is.
    fn crop_release(&self, app: &Rc<App>) {
        let Some((_, start, id)) = self.crop_drag.borrow_mut().take() else {
            return;
        };
        let Some(surface) = self.surface(app) else {
            return;
        };
        let area = surface.area();
        let crop = Crop {
            x: f64::from(area.get_crop_x()),
            y: f64::from(area.get_crop_y()),
            width: f64::from(area.get_crop_w()),
            height: f64::from(area.get_crop_h()),
        };
        if crop == start {
            return;
        }
        let span = {
            let timeline = self.timeline.borrow();
            timeline
                .as_ref()
                .and_then(|t| t.clips.iter().find(|c| c.id == id))
                .map(|c| (c.start_seconds, c.start_seconds + c.duration_seconds))
        };
        let Some((start_seconds, end_seconds)) = span else {
            return;
        };
        self.edit(
            app,
            Method::TimelineCrop(TimelineCropParams {
                start_seconds,
                end_seconds,
                crop: (!crop.is_full()).then_some(crop),
                preset: None,
                clear: crop.is_full(),
                timeline: None,
            }),
            "Couldn’t crop the clip",
        );
    }

    /// A shape chosen: crop the clip at the playhead to the largest of it
    /// inside the picture, centred ("free" only lets drags change shape).
    fn crop_preset(&self, app: &Rc<App>, name: &str) {
        if let Some(surface) = self.surface(app) {
            surface.area().set_crop_preset(name.into());
        }
        if let Some(preset) = CropPreset::parse(name) {
            self.crop_with(app, None, Some(preset), false);
        }
    }

    fn exported(&self, app: &Rc<App>, event: &Event) {
        let Event::ExportFinished {
            timeline,
            files,
            error,
            manifest,
            ..
        } = event
        else {
            return;
        };
        *self.export.borrow_mut() = None;
        let program = files
            .iter()
            .find(|f| f.source == isocast_protocol::PROGRAM)
            .or_else(|| files.first())
            .map(|f| f.path.clone());
        match error {
            Some(error) => {
                app.toast(ToastKind::Error, "The export failed", error.clone());
            }
            None => {
                *self.exported.borrow_mut() = program.clone();
                let shown = program.unwrap_or_else(|| manifest.clone());
                let seconds = files
                    .iter()
                    .find_map(|f| f.duration_seconds)
                    .unwrap_or_default();
                let name = self
                    .list
                    .borrow()
                    .as_ref()
                    .and_then(|l| l.timelines.iter().find(|t| &t.id == timeline))
                    .map_or_else(|| timeline.clone(), |t| t.name.clone());
                let folder = shown
                    .parent()
                    .map(|p| crate::platform::abbreviate_home(&p.display().to_string()))
                    .unwrap_or_default();
                app.show_toast(
                    Toast::new(
                        ToastKind::Success,
                        format!("Exported {name}"),
                        format!(
                            "{} file{} · {} · in {folder}",
                            files.len(),
                            if files.len() == 1 { "" } else { "s" },
                            mm_ss(seconds)
                        ),
                    )
                    .action("Show in Finder", move |_| {
                        crate::platform::reveal(&shown.display().to_string())
                    }),
                );
            }
        }
        self.show_header(app);
    }
}

impl Area for Create {
    fn connect(&self, app: &Rc<App>, window: &AppWindow) {
        self.wire(app, &window.global::<CreateArea>());
    }

    fn update(&self, app: &Rc<App>, _window: &AppWindow, changed: Changed) {
        if changed.any(Changed::LINK) {
            let link = app.session.borrow().link;
            if self.link.replace(link) != link && link == Some(Link::Connected) {
                self.load(app);
            }
        }
        if changed.any(Changed::STATUS) {
            // An export running when the window opened (or started by
            // another client).
            let running = app
                .session
                .borrow()
                .status
                .as_ref()
                .and_then(|s| s.export.clone());
            if let Some(running) = running {
                let mut export = self.export.borrow_mut();
                if export
                    .as_ref()
                    .is_none_or(|(id, _)| id.is_empty() || *id == running.export)
                {
                    *export = Some((running.export, running.progress));
                }
            }
        }
        if changed.any(Changed::STATUS | Changed::SETUP | Changed::STREAM | Changed::REPLAY) {
            self.follow_capture(app);
        }
        if changed.any(Changed::STATUS | Changed::BUSY | Changed::SETUP | Changed::LINK) {
            self.show_header(app);
        }
    }

    fn menu(&self, app: &Rc<App>, id: &str) -> bool {
        match id {
            "create_view" => {
                self.toggle(app);
                true
            }
            // Delete in Create takes the selected clip off.
            "remove" if self.active(app) => {
                let id = self.selected.borrow().clone();
                if let Some(id) = id {
                    self.remove(app, &id);
                }
                true
            }
            // Window › Editor (C.3).
            "editor" => {
                if let Err(error) = self.open_editor(app) {
                    eprintln!("editor: {error:#}");
                }
                true
            }
            _ => false,
        }
    }

    fn context_menu(&self, _app: &Rc<App>, owner: &str) -> Option<MenuBuilder> {
        match owner {
            "create:timelines" => Some(self.timelines_menu()),
            "create:export" => Some(
                MenuBuilder::new()
                    .item("export", "Export")
                    .icon("download")
                    .item("vertical", "Export with Vertical (9:16)"),
            ),
            _ => self.clip_menu(owner.strip_prefix("create:clip:")?),
        }
    }

    fn menu_chosen(&self, app: &Rc<App>, owner: &str, id: &str) -> bool {
        if owner == "create:export" {
            self.export(app, id == "vertical");
            return true;
        }
        if owner == "create:timelines" {
            match id {
                "new" => self.new_timeline(app, None),
                "close" => self.close(app),
                _ => {
                    if let Some(timeline) = id.strip_prefix("open:") {
                        self.open(app, timeline);
                    }
                }
            }
            return true;
        }
        if let Some(clip) = owner.strip_prefix("create:clip:") {
            self.clip_chosen(app, clip, id);
            return true;
        }
        false
    }

    fn event(&self, app: &Rc<App>, event: &Event) {
        match event {
            Event::TimelineChanged { .. } => self.load(app),
            Event::ExportProgress {
                export, progress, ..
            } => {
                {
                    let mut running = self.export.borrow_mut();
                    match running.as_mut() {
                        Some((id, p)) if id.is_empty() || id == export => {
                            *id = export.clone();
                            *p = *progress;
                        }
                        Some(_) => {}
                        None => *running = Some((export.clone(), *progress)),
                    }
                }
                self.show_header(app);
            }
            Event::ExportFinished { .. } => self.exported(app, event),
            // A take began: a new recording, or a split's or retake's next
            // take while recording, which the LCD counts on from.
            Event::Started { .. } => {
                if let Some(window) = app.window() {
                    let recording = app
                        .session
                        .borrow()
                        .status
                        .as_ref()
                        .is_some_and(|s| s.state == EngineState::Recording);
                    let offset = if recording {
                        window.global::<crate::ControlsArea>().get_rec_seconds()
                    } else {
                        0
                    };
                    window.global::<CreateArea>().set_rec_offset(offset);
                }
                if self.retake_since.get().is_some() {
                    self.retake_since.set(None);
                    self.retake_timer.stop();
                    self.show_header(app);
                }
            }
            _ => {}
        }
    }

    fn state(
        &self,
        app: &Rc<App>,
        window: &AppWindow,
        name: &str,
        arg: &str,
    ) -> Option<anyhow::Result<()>> {
        let area = window.global::<CreateArea>();
        let index = |arg: &str| -> anyhow::Result<String> {
            let i: usize = arg
                .parse()
                .map_err(|_| anyhow::anyhow!("INDEX is a number from 0"))?;
            self.timeline
                .borrow()
                .as_ref()
                .and_then(|t| t.clips.get(i))
                .map(|c| c.id.clone())
                .ok_or_else(|| anyhow::anyhow!("no clip {i} on the current timeline"))
        };
        Some(match name {
            "create" => {
                if !area.get_active() {
                    self.toggle(app);
                }
                Ok(())
            }
            "create-new" => {
                self.new_timeline(app, (!arg.is_empty()).then(|| arg.to_string()));
                Ok(())
            }
            "create-open" => {
                self.open(app, arg);
                Ok(())
            }
            "create-close" => {
                self.close(app);
                Ok(())
            }
            "create-select" => index(arg).map(|id| self.select(app, &id)),
            "create-retake" => {
                self.retake(app);
                Ok(())
            }
            "create-remove" => {
                let id = self.selected.borrow().clone();
                match id {
                    Some(id) => {
                        self.remove(app, &id);
                        Ok(())
                    }
                    None => Err(anyhow::anyhow!("no clip is selected (create-select:INDEX)")),
                }
            }
            "create-move" => {
                let Some((from, to)) = arg.split_once(',') else {
                    return Some(Err(anyhow::anyhow!("create-move:INDEX,TO")));
                };
                index(from).and_then(|id| {
                    let to: usize = to.parse().map_err(|_| anyhow::anyhow!("TO is a number"))?;
                    self.move_clip(app, &id, to);
                    Ok(())
                })
            }
            "create-export" => {
                self.export(app, arg == "vertical");
                Ok(())
            }
            "create-zoom" => arg
                .parse::<f32>()
                .map(|z| area.set_zoom(z.clamp(0.0, 1.0)))
                .map_err(|_| anyhow::anyhow!("create-zoom takes 0 to 1")),
            "create-scroll" => arg
                .parse::<f32>()
                .map(|x| area.set_scroll_x(x.max(0.0)))
                .map_err(|_| anyhow::anyhow!("create-scroll takes points")),
            "create-playhead" => arg
                .parse::<f64>()
                .map(|t| self.set_playhead(app, t))
                .map_err(|_| anyhow::anyhow!("create-playhead takes seconds")),
            "editor" => self.open_editor(app),
            "create-mark" => {
                let Some((a, b)) = arg.split_once(',') else {
                    return Some(Err(anyhow::anyhow!("create-mark:IN,OUT")));
                };
                match (a.parse::<f32>(), b.parse::<f32>()) {
                    (Ok(a), Ok(b)) => {
                        if let Some(surface) = self.surface(app) {
                            surface.area().set_mark_in(a);
                            surface.area().set_mark_out(b);
                        }
                        Ok(())
                    }
                    _ => Err(anyhow::anyhow!("create-mark:IN,OUT takes seconds")),
                }
            }
            "create-split" => {
                self.split(app);
                Ok(())
            }
            "create-delete" => {
                self.delete_range(app);
                Ok(())
            }
            "create-angle" => {
                self.set_angle(app, arg);
                Ok(())
            }
            "create-trim" => {
                let parts: Vec<&str> = arg.split(',').collect();
                match parts.as_slice() {
                    [i, edge, delta] => index(i).and_then(|id| {
                        let delta: f64 = delta
                            .parse()
                            .map_err(|_| anyhow::anyhow!("SECONDS is a number"))?;
                        self.trim(app, &id, *edge == "out", delta);
                        Ok(())
                    }),
                    _ => Err(anyhow::anyhow!("create-trim:INDEX,in|out,SECONDS")),
                }
            }
            "create-undo" => {
                self.step(app, false);
                Ok(())
            }
            "create-redo" => {
                self.step(app, true);
                Ok(())
            }
            "create-layout" => arg
                .parse::<i32>()
                .map(|l| {
                    if let Some(surface) = self.surface(app) {
                        surface.area().set_layout(l.clamp(0, 1));
                    }
                })
                .map_err(|_| anyhow::anyhow!("create-layout takes 0 or 1")),
            "create-preview" => {
                self.show_preview(app);
                if let Some(frame) = self.preview.wait(Duration::from_secs(10)) {
                    self.preview_timer.stop();
                    self.shown(app, frame);
                }
                Ok(())
            }
            "create-crop" => {
                match arg {
                    "clear" => self.crop_with(app, None, None, true),
                    preset if CropPreset::parse(preset).is_some() => self.crop_preset(app, preset),
                    rect => {
                        let v: Vec<f64> = rect.split(',').filter_map(|v| v.parse().ok()).collect();
                        let [x, y, width, height] = v[..] else {
                            return Some(Err(anyhow::anyhow!(
                                "create-crop:X,Y,W,H, a preset (16:9, 9:16, 1:1, 4:5) or clear"
                            )));
                        };
                        self.crop_with(
                            app,
                            Some(Crop {
                                x,
                                y,
                                width,
                                height,
                            }),
                            None,
                            false,
                        );
                    }
                }
                Ok(())
            }
            "create-crop-mode" => {
                if let Some(surface) = self.surface(app) {
                    surface.area().set_crop_mode(arg != "off");
                }
                Ok(())
            }
            "create-roll" => {
                let Some((i, by)) = arg.split_once(',') else {
                    return Some(Err(anyhow::anyhow!("create-roll:INDEX,SECONDS")));
                };
                index(i).and_then(|id| {
                    let by: f64 = by
                        .parse()
                        .map_err(|_| anyhow::anyhow!("SECONDS is a number"))?;
                    self.roll(app, &id, by);
                    Ok(())
                })
            }
            "create-key" => {
                self.edit_key(app, arg);
                Ok(())
            }
            "create-waves" => {
                self.show_rows(app);
                let until = Instant::now() + Duration::from_secs(20);
                loop {
                    self.waves.collect();
                    if self.waves.idle() || Instant::now() > until {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                self.show_rows(app);
                Ok(())
            }
            "create-thumbs" => {
                let until = Instant::now() + Duration::from_secs(20);
                loop {
                    self.thumbs.collect();
                    if self.thumbs.idle() || Instant::now() > until {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                self.show_rows(app);
                Ok(())
            }
            _ => return None,
        })
    }
}
