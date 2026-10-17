//! Project files (#88; ui/areas/projects/): File › Open… (⌘O) and Open
//! Recent, `.isoprj` files opened from the Finder (src/documents.rs), and
//! "Open the editor after recording" (the setup's
//! `recording.open_editor`, in Settings › Output).
//!
//! Each is a method on the engine's table: `project_open` takes the
//! project's timelines in and makes one current, then the editor window
//! shows it (`project_opened`, from this window, the CLI or MCP);
//! `recent_projects` is the Open Recent list. After a recording stops, the
//! editor opens on its project without taking the keyboard: behind the
//! studio window while a stream runs.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use isocast_protocol::api::{Event, Method};
use isocast_protocol::isoprj::{ProjectOpenParams, ProjectOpened, RecentProjects};
use isocast_protocol::timeline::{TimelineInfo, TimelineRelinkParams};
use slint::{ComponentHandle, ModelRc, VecModel};

use super::{Area, Changed};
use crate::app::App;
use crate::daemon::Link;
use crate::toasts::Toast;
use crate::{AppWindow, EditorWindow, ProjectsArea, RecentRow, ToastKind};

/// Snapshot states (`--state`).
pub const STATES: &[&str] = &[
    "project-open:PATH|last (as File › Open… with that file chosen; last: the recording just stopped)",
    "project-open-only:PATH|last (the engine opens it, the editor doesn't show)",
    "project-recent (read File › Open Recent's list)",
];

/// How often files the Finder handed over are looked for.
const DOCUMENTS_EVERY: Duration = Duration::from_millis(250);

/// How the editor comes up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Raise {
    /// In front, with the keyboard (it was asked for).
    Focus,
    /// Shown, but the app isn't brought forward (a recording stopped).
    Show,
    /// Behind the studio window, which keeps the keyboard (a recording
    /// stopped while a stream runs).
    Behind,
}

#[derive(Default)]
pub struct Projects {
    link: Cell<Option<Link>>,
    /// Picks up files the Finder handed over.
    documents: slint::Timer,
}

impl Projects {
    /// Read the Open Recent list.
    fn load_recent(&self, app: &Rc<App>) {
        app.call_as::<RecentProjects>(Method::RecentProjects(None), |app, result| {
            // An engine from before #88 has none.
            let Ok(recent) = result else { return };
            app.projects.show_recent(app, &recent);
        });
    }

    fn show_recent(&self, app: &Rc<App>, recent: &RecentProjects) {
        let Some(window) = app.window() else { return };
        let rows: Vec<RecentRow> = recent
            .projects
            .iter()
            .map(|project| {
                let shared = recent
                    .projects
                    .iter()
                    .filter(|p| p.name == project.name)
                    .count()
                    > 1;
                let folder = project
                    .path
                    .parent()
                    .and_then(|f| f.file_name())
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                RecentRow {
                    title: match shared {
                        true => format!("{} — {folder}", project.name),
                        false => project.name.clone(),
                    }
                    .into(),
                    path: project.path.display().to_string().into(),
                    exists: project.exists,
                }
            })
            .collect();
        let model = ModelRc::new(VecModel::from(rows));
        window.global::<ProjectsArea>().set_recent(model.clone());
        if let Some(editor) = app.create.editor_window() {
            editor.global::<ProjectsArea>().set_recent(model);
        }
    }

    /// The editor window's File menu: Open…, Open Recent and Clear Menu,
    /// as the studio's (macOS shows the key window's menu bar).
    pub fn connect_editor(&self, app: &Rc<App>, editor: &EditorWindow) {
        let area = editor.global::<ProjectsArea>();
        let weak = Rc::downgrade(app);
        area.on_open_recent(move |path| {
            if let Some(app) = weak.upgrade() {
                app.projects.open(&app, PathBuf::from(path.as_str()));
            }
        });
        let weak = Rc::downgrade(app);
        area.on_open_file(move || {
            if let Some(app) = weak.upgrade() {
                app.projects.choose(&app);
            }
        });
        let weak = Rc::downgrade(app);
        area.on_clear_recent(move || {
            if let Some(app) = weak.upgrade() {
                let _ = app.projects.menu(&app, "clear_recent_projects");
            }
        });
        if let Some(window) = app.window() {
            area.set_recent(window.global::<ProjectsArea>().get_recent());
        }
    }

    /// Open `path` (a project file or folder) in the editor, as asked for
    /// (File › Open…, Open Recent, the Finder).
    pub fn open(&self, app: &Rc<App>, path: PathBuf) {
        self.open_with(app, path, Raise::Focus);
    }

    fn open_with(&self, app: &Rc<App>, path: PathBuf, raise: Raise) {
        let method = Method::ProjectOpen(ProjectOpenParams {
            path: path.clone(),
            timeline: None,
            // This window opens the editor itself, as `raise` says;
            // others are told with `project_opened`.
            editor: Some(false),
        });
        app.call_as::<ProjectOpened>(method, move |app, result| match result {
            Ok(opened) => {
                if let Err(error) = app.create.open_editor_as(app, raise) {
                    isocast_diagnostics::diagnostic!("editor: {error:#}");
                }
                if !opened.offline.is_empty() {
                    app.projects.offline(app, &opened);
                }
                app.projects.load_recent(app);
            }
            Err(error) if raise == Raise::Focus => {
                app.failed("Couldn’t open the project", &error);
            }
            // After a recording: never a dialog over the studio.
            Err(error) => {
                isocast_diagnostics::diagnostic!(
                    "warn"; "editor after recording: {}: {}", path.display(), error.message
                );
            }
        });
    }

    /// Say which takes are offline, with Relink… to choose the folder
    /// they are in now.
    fn offline(&self, app: &Rc<App>, opened: &ProjectOpened) {
        let n = opened.offline.len();
        // One clip per folder the takes were in: relinking it by folder
        // finds every take of that folder in the new one.
        let mut clips: Vec<String> = Vec::new();
        let mut folders: Vec<PathBuf> = Vec::new();
        for clip in &opened.timeline.clips {
            let folder = clip.take.parent().map(PathBuf::from).unwrap_or_default();
            if opened.offline.contains(&clip.take) && !folders.contains(&folder) {
                folders.push(folder);
                clips.push(clip.id.clone());
            }
        }
        let timeline = opened.timeline.id.clone();
        app.show_toast(
            Toast::new(
                ToastKind::Warning,
                format!("{n} take{} offline", if n == 1 { " is" } else { "s are" }),
                "Their files aren’t where the project says. Choose the folder they’re in now.",
            )
            .action("Relink…", move |app| {
                let chosen = crate::platform::choose(crate::platform::Choose {
                    folders: true,
                    multiple: false,
                    prompt: "Relink",
                    types: &[],
                });
                let Some(folder) = chosen.and_then(|paths| paths.into_iter().next()) else {
                    return;
                };
                for clip in &clips {
                    app.call_as::<TimelineInfo>(
                        Method::TimelineRelink(TimelineRelinkParams {
                            clip: clip.clone(),
                            path: PathBuf::from(&folder),
                            timeline: Some(timeline.clone()),
                        }),
                        |app, result| {
                            if let Err(error) = result {
                                app.failed("Couldn’t relink the take", &error);
                            }
                        },
                    );
                }
            }),
        );
    }

    /// A clip menu's Relink… (an offline clip of the current timeline):
    /// choose the folder its take is in now, or its manifest's.
    pub fn relink(&self, app: &Rc<App>, clip: &str) {
        let chosen = crate::platform::choose(crate::platform::Choose {
            folders: true,
            multiple: false,
            prompt: "Relink",
            types: &[],
        });
        let Some(folder) = chosen.and_then(|paths| paths.into_iter().next()) else {
            return;
        };
        app.call_as::<TimelineInfo>(
            Method::TimelineRelink(TimelineRelinkParams {
                clip: clip.to_string(),
                path: PathBuf::from(folder),
                timeline: None,
            }),
            |app, result| {
                if let Err(error) = result {
                    app.failed("Couldn’t relink the take", &error);
                }
            },
        );
    }

    /// File › Open…: choose a project file.
    fn choose(&self, app: &Rc<App>) {
        let chosen = crate::platform::choose(crate::platform::Choose {
            folders: false,
            multiple: false,
            prompt: "Open",
            types: &[isocast_protocol::isoprj::EXTENSION],
        });
        if let Some(path) = chosen.and_then(|paths| paths.into_iter().next()) {
            self.open(app, PathBuf::from(path));
        }
    }

    /// Files the Finder handed over, once the engine is there to open
    /// them.
    fn take_documents(&self, app: &Rc<App>) {
        if app.session.borrow().link != Some(Link::Connected) {
            return;
        }
        for path in crate::documents::take() {
            self.open(app, path);
        }
    }

    /// A recording stopped (not a split): open the editor on its project
    /// if the setup asks for it.
    fn recording_stopped(&self, app: &Rc<App>, folder: &std::path::Path) {
        let (wanted, streaming) = {
            let s = app.session.borrow();
            (
                s.setup.as_ref().is_some_and(|s| s.recording.open_editor),
                s.stream.as_ref().is_some_and(|s| s.is_active()),
            )
        };
        if !wanted {
            return;
        }
        let raise = match streaming {
            true => Raise::Behind,
            false => Raise::Show,
        };
        self.open_with(app, folder.to_path_buf(), raise);
    }
}

impl Area for Projects {
    fn connect(&self, app: &Rc<App>, window: &AppWindow) {
        let weak = Rc::downgrade(app);
        window.global::<ProjectsArea>().on_open_recent(move |path| {
            if let Some(app) = weak.upgrade() {
                app.projects.open(&app, PathBuf::from(path.as_str()));
            }
        });
        if !app.mode.offscreen {
            let weak = Rc::downgrade(app);
            self.documents
                .start(slint::TimerMode::Repeated, DOCUMENTS_EVERY, move || {
                    if let Some(app) = weak.upgrade() {
                        app.projects.take_documents(&app);
                    }
                });
        }
    }

    fn update(&self, app: &Rc<App>, _window: &AppWindow, changed: Changed) {
        if changed.any(Changed::LINK) {
            let link = app.session.borrow().link;
            if self.link.replace(link) != link && link == Some(Link::Connected) {
                self.load_recent(app);
            }
        }
    }

    fn menu(&self, app: &Rc<App>, id: &str) -> bool {
        match id {
            "open_project" => self.choose(app),
            "clear_recent_projects" => {
                app.call_as::<RecentProjects>(
                    Method::RecentProjects(Some(isocast_protocol::isoprj::RecentProjectsParams {
                        clear: true,
                    })),
                    |app, result| {
                        if let Ok(recent) = result {
                            app.projects.show_recent(app, &recent);
                        }
                    },
                );
            }
            _ => return false,
        }
        true
    }

    fn event(&self, app: &Rc<App>, event: &Event) {
        match event {
            // Opened by the CLI, MCP or another window: show it here.
            Event::ProjectOpened { editor, .. } => {
                if *editor && let Err(error) = app.create.open_editor_as(app, Raise::Focus) {
                    isocast_diagnostics::diagnostic!("editor: {error:#}");
                }
                self.load_recent(app);
            }
            Event::Stopped {
                report: Some(_),
                folder: Some(folder),
                ..
            } => self.recording_stopped(app, folder),
            _ => {}
        }
    }

    fn state(
        &self,
        app: &Rc<App>,
        _window: &AppWindow,
        name: &str,
        arg: &str,
    ) -> Option<anyhow::Result<()>> {
        Some(match name {
            "project-open" | "project-open-only" => {
                // `last`: the project of the recording that just stopped.
                let path = match arg {
                    "last" => app
                        .session
                        .borrow()
                        .status
                        .as_ref()
                        .and_then(|s| s.last.as_ref())
                        .map(|last| last.folder.clone()),
                    path => Some(PathBuf::from(path)),
                };
                match path {
                    Some(path) if name == "project-open" => {
                        self.open(app, path);
                        Ok(())
                    }
                    // As `isocast project open --no-editor`: only the
                    // engine's side (and Open Recent's list).
                    Some(path) => {
                        app.call_as::<ProjectOpened>(
                            Method::ProjectOpen(ProjectOpenParams {
                                path,
                                timeline: None,
                                editor: Some(false),
                            }),
                            |app, _| app.projects.load_recent(app),
                        );
                        Ok(())
                    }
                    None => Err(anyhow::anyhow!(
                        "no recording has stopped yet (record, stop-record)"
                    )),
                }
            }
            "project-recent" => {
                self.load_recent(app);
                Ok(())
            }
            _ => return None,
        })
    }
}
