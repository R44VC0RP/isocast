//! Toasts (docs/design.md 3.13), as the web app's `toast()`: info and
//! success fade after 4 s, warnings after 8 s, errors stay until dismissed.
//! A toast with the same key (or, without one, the same words) is updated
//! in place and its time starts again, rather than stacking another; at
//! most three show, newest first.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use slint::{ComponentHandle, SharedString};

use crate::app::App;
use crate::kit::model;
use crate::{AppWindow, ToastData, ToastKind, ToastRow, Toasts};

/// What a toast's action button does.
pub type ToastAction = Box<dyn Fn(&Rc<App>)>;

/// A toast to show.
pub struct Toast {
    pub kind: ToastKind,
    pub title: String,
    pub message: String,
    /// A button's label and what it does ("Undo", "Details").
    pub action: Option<(String, ToastAction)>,
    /// Updates the toast with the same key instead of adding one.
    pub key: Option<String>,
    /// A small table: label, value, and "", "bad" or "warn".
    pub rows: Vec<(String, String, &'static str)>,
}

impl Toast {
    pub fn new(kind: ToastKind, title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            message: message.into(),
            action: None,
            key: None,
            rows: Vec::new(),
        }
    }

    pub fn action(mut self, label: impl Into<String>, run: impl Fn(&Rc<App>) + 'static) -> Self {
        self.action = Some((label.into(), Box::new(run)));
        self
    }

    pub fn key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    pub fn rows(mut self, rows: Vec<(String, String, &'static str)>) -> Self {
        self.rows = rows;
        self
    }
}

struct Shown {
    id: i32,
    kind: ToastKind,
    title: String,
    message: String,
    key: Option<String>,
    data: ToastData,
    action: Option<ToastAction>,
    // Dismisses it when it fires (dropped with it).
    _timer: Option<slint::Timer>,
}

#[derive(Default)]
pub struct ToastList {
    shown: RefCell<Vec<Shown>>,
    next: Cell<i32>,
    /// Toasts with this key aren't drawn (`Toasts.hide-key`).
    hidden: RefCell<String>,
}

impl ToastList {
    /// Wire the Toasts global's buttons.
    pub fn connect(app: &Rc<App>, window: &AppWindow) {
        let weak = Rc::downgrade(app);
        window.global::<Toasts>().on_dismiss(move |id| {
            if let Some(app) = weak.upgrade() {
                app.toasts.dismiss(&app, id);
            }
        });
        let weak = Rc::downgrade(app);
        window.global::<Toasts>().on_hide_key(move |key| {
            if let Some(app) = weak.upgrade() {
                *app.toasts.hidden.borrow_mut() = key.to_string();
                app.toasts.publish(&app);
            }
        });
        let weak = Rc::downgrade(app);
        window.global::<Toasts>().on_action(move |id| {
            if let Some(app) = weak.upgrade() {
                app.toasts.run_action(&app, id);
            }
        });
    }

    /// Show `toast`; returns its id.
    pub fn show(&self, app: &Rc<App>, toast: Toast) -> i32 {
        let same = |s: &Shown| match &toast.key {
            Some(key) => s.key.as_deref() == Some(key),
            None => {
                s.key.is_none()
                    && s.kind == toast.kind
                    && s.title == toast.title
                    && s.message == toast.message
            }
        };
        let existing = self.shown.borrow().iter().position(same);
        let id = match existing {
            Some(at) => self.shown.borrow()[at].id,
            None => {
                let id = self.next.get() + 1;
                self.next.set(id);
                id
            }
        };
        let (action_label, action) = match toast.action {
            Some((label, run)) => (label, Some(run)),
            None => (String::new(), None),
        };
        let data = ToastData {
            id,
            kind: toast.kind,
            title: toast.title.as_str().into(),
            message: toast.message.as_str().into(),
            action: action_label.into(),
            rows: model(toast.rows.into_iter().map(|(label, value, tone)| ToastRow {
                label: label.into(),
                value: value.into(),
                tone: SharedString::from(tone),
            })),
        };
        // Errors stay; the rest go after a while.
        let timer = (toast.kind != ToastKind::Error).then(|| {
            let timer = slint::Timer::default();
            let weak: Weak<App> = Rc::downgrade(app);
            let after = if toast.kind == ToastKind::Warning {
                8
            } else {
                4
            };
            timer.start(
                slint::TimerMode::SingleShot,
                Duration::from_secs(after),
                move || {
                    if let Some(app) = weak.upgrade() {
                        app.toasts.dismiss(&app, id);
                    }
                },
            );
            timer
        });
        let entry = Shown {
            id,
            kind: toast.kind,
            title: toast.title,
            message: toast.message,
            key: toast.key,
            data,
            action,
            _timer: timer,
        };
        {
            let mut shown = self.shown.borrow_mut();
            match existing {
                Some(at) => shown[at] = entry,
                None => {
                    shown.push(entry);
                    // Three at most: the newest two and this one.
                    while shown.len() > 3 {
                        shown.remove(0);
                    }
                }
            }
        }
        self.publish(app);
        id
    }

    pub fn dismiss(&self, app: &Rc<App>, id: i32) {
        let removed = {
            let mut shown = self.shown.borrow_mut();
            let before = shown.len();
            shown.retain(|s| s.id != id);
            before != shown.len()
        };
        if removed {
            self.publish(app);
        }
    }

    /// Whether a toast with `key` is showing.
    pub fn has_key(&self, key: &str) -> bool {
        self.shown
            .borrow()
            .iter()
            .any(|s| s.key.as_deref() == Some(key))
    }

    fn run_action(&self, app: &Rc<App>, id: i32) {
        // Taken out while it runs: the action may show other toasts.
        let action = self
            .shown
            .borrow_mut()
            .iter_mut()
            .find(|s| s.id == id)
            .and_then(|s| s.action.take());
        if let Some(action) = action {
            self.dismiss(app, id);
            action(app);
        }
    }

    fn publish(&self, app: &Rc<App>) {
        let Some(window) = app.window() else { return };
        let hidden = self.hidden.borrow();
        let items: Vec<ToastData> = self
            .shown
            .borrow()
            .iter()
            .rev()
            .filter(|s| hidden.is_empty() || s.key.as_deref() != Some(hidden.as_str()))
            .map(|s| s.data.clone())
            .collect();
        window.global::<Toasts>().set_items(model(items));
    }
}
