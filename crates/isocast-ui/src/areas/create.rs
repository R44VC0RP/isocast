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

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use isocast_protocol::PROGRAM;
use isocast_protocol::api::{EngineState, Event, Method};
use isocast_protocol::timeline::{
    ClipFile, ClipInfo, ExportStarted, Retaken, TimelineAngleParams, TimelineInfo, TimelineList,
    TimelineMoveParams, TimelineNewParams, TimelineOpenParams, TimelineRangeParams,
    TimelineRemoveParams, TimelineSplitParams, TimelineTarget, TimelineTrimParams,
};
use slint::{ComponentHandle, ModelRc, VecModel};

use super::{Area, Changed};
use crate::app::App;
use crate::daemon::Link;
use crate::kit::{self, MenuBuilder};
use crate::thumbs::{Preview, Thumbs};
use crate::toasts::Toast;
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
    "create-export",
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
    "editor (open the editor window; the snapshot draws it)",
];

/// How often the preview plays forward (J/K/L).
const PLAY_EVERY: Duration = Duration::from_millis(33);

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
    /// When the preview's newest picture was asked for, for
    /// ISOCAST_PREVIEW_STATS=1 (scrub latency on a test Mac).
    asked_at: Cell<Option<Instant>>,
    /// The editor window (C.3), once opened (Window › Editor).
    editor: RefCell<Option<EditorWindow>>,
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
        self.show(app);
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
                        SegmentRow {
                            clip: clip.id.as_str().into(),
                            start: clip.start_seconds as f32,
                            duration: clip.duration_seconds as f32,
                            thumbs,
                            in_frac,
                            out_frac,
                            active: clip.angle.as_deref() == Some(source.as_str()),
                            missing: file.is_none(),
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

    /// Take in thumbnails as they are made, every 50 ms until none are
    /// left (no timer runs otherwise).
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
                if app.create.thumbs.collect() {
                    app.create.show_rows(&app);
                }
                if app.create.thumbs.idle() {
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

    fn export(&self, app: &Rc<App>) {
        if self.export.borrow().is_some() {
            return;
        }
        *self.export.borrow_mut() = Some((String::new(), 0.0));
        self.show_header(app);
        app.call_as::<ExportStarted>(Method::TimelineExport(None), |app, result| {
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
        Some(
            MenuBuilder::new()
                .item("editor", "Open in Editor")
                .separator()
                .item("reveal", "Show in Finder")
                .disabled(!file)
                .separator()
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
        area.on_export(move || {
            if let Some(app) = weak.upgrade() {
                app.create.export(&app);
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
            area.set_preview_label("".into());
            self.preview.reset();
            return;
        };
        let source = clip.angle.clone().unwrap_or_else(|| PROGRAM.to_string());
        let into = (at - clip.start_seconds).clamp(0.0, clip.duration_seconds);
        // The frame on screen then: just inside the clip at its end.
        let take = clip.in_seconds + into.min(clip.duration_seconds - 1e-3).max(0.0);
        let name = match source.as_str() {
            PROGRAM => "Program".to_string(),
            other => source_name(app, other),
        };
        area.set_preview_label(format!("{name} · {}", mm_ss_cc(at)).into());
        let Some(file) = file_of(&clip, &source) else {
            area.set_has_preview(false);
            return;
        };
        let seconds = (take - file.start_offset_seconds.unwrap_or(0.0)).max(0.0);
        if self.preview.request(&file.path, seconds) {
            self.asked_at.set(Some(Instant::now()));
            self.collect_preview(app);
        }
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
            Err(error) => app.failed(failed, &error),
        });
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
        if let Some(editor) = self.editor.borrow().as_ref() {
            editor.show()?;
            focus(editor);
            return Ok(());
        }
        let editor = EditorWindow::new()?;
        {
            let area = editor.global::<CreateArea>();
            self.wire(app, &area);
            area.set_active(true);
        }
        editor.show()?;
        focus(&editor);
        *self.editor.borrow_mut() = Some(editor);
        self.load(app);
        Ok(())
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
        self.played.set(Some(Instant::now()));
        if next == 0.0 {
            self.play_timer.stop();
        } else if !self.play_timer.running() {
            let weak = Rc::downgrade(app);
            self.play_timer
                .start(slint::TimerMode::Repeated, PLAY_EVERY, move || {
                    if let Some(app) = weak.upgrade() {
                        app.create.play_step(&app);
                    }
                });
        }
        self.show_header(app);
    }

    fn play_step(&self, app: &Rc<App>) {
        let now = Instant::now();
        let since = self
            .played
            .replace(Some(now))
            .map_or(0.0, |t| now.duration_since(t).as_secs_f64());
        let total = self
            .timeline
            .borrow()
            .as_ref()
            .map_or(0.0, |t| t.duration_seconds);
        let at = self.playhead(app) + since * self.shuttle.get();
        if at <= 0.0 || at >= total {
            self.shuttle.set(0.0);
            self.play_timer.stop();
            self.show_header(app);
        }
        self.set_playhead(app, at.clamp(0.0, total));
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
            "clear" => self.clear_marks(app),
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
            _ => self.clip_menu(owner.strip_prefix("create:clip:")?),
        }
    }

    fn menu_chosen(&self, app: &Rc<App>, owner: &str, id: &str) -> bool {
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
                self.export(app);
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
