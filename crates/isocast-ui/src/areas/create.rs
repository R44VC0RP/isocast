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

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use isocast_protocol::api::{EngineState, Event, Method};
use isocast_protocol::timeline::{
    ClipInfo, ExportStarted, Retaken, TimelineInfo, TimelineList, TimelineMoveParams,
    TimelineNewParams, TimelineOpenParams, TimelineRemoveParams, TimelineTarget,
};
use slint::{ComponentHandle, ModelRc, VecModel};

use super::{Area, Changed};
use crate::app::App;
use crate::daemon::Link;
use crate::kit::{self, MenuBuilder};
use crate::thumbs::Thumbs;
use crate::toasts::Toast;
use crate::{AppWindow, ClipRow, CreateArea, ToastKind};

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
];

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
}

/// "c3" → "Take 3".
fn take_name(id: &str) -> String {
    match id.strip_prefix('c') {
        Some(n) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => format!("Take {n}"),
        _ => id.to_string(),
    }
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
        let rows: Vec<ClipRow> = clips
            .iter()
            .enumerate()
            .map(|(index, clip)| {
                let file = program_file(clip);
                let thumbs = match file {
                    Some(path) => match self.thumbs.get(path) {
                        Some(frames) => frames,
                        None => {
                            asked |= self.thumbs.request(path, clip.duration_seconds);
                            empty.clone()
                        }
                    },
                    None => empty.clone(),
                };
                ClipRow {
                    id: clip.id.as_str().into(),
                    number: place(index).into(),
                    name: take_name(&clip.id).into(),
                    length: mm_ss(clip.duration_seconds).into(),
                    start: clip.start_seconds as f32,
                    duration: clip.duration_seconds as f32,
                    selected: selected.as_deref() == Some(clip.id.as_str()),
                    problem: clip.problem.is_some() || clip.files.iter().any(|f| !f.exists),
                    dropped: clip.dropped_frames > 0,
                    thumbs,
                }
            })
            .collect();
        kit::update_rows(&self.rows, rows);
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

    /// The header: the take's line, what can be done, the export.
    fn show_header(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<CreateArea>();
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
        area.set_summary(
            format!(
                "{n} take{} · {}",
                if n == 1 { "" } else { "s" },
                mm_ss(total)
            )
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
        self.retake_since.set(Some(Instant::now()));
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
            _ => {}
        }
    }

    fn toggle(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let area = window.global::<CreateArea>();
        let on = !area.get_active();
        area.set_active(on);
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
        let area = window.global::<CreateArea>();
        area.set_clips(ModelRc::from(self.rows.clone()));
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
