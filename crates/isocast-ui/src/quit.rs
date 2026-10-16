//! Leaving the app while it records or streams (ui/quit.slint). Every way
//! out comes here: Quit (⌘Q, the Dock, logging out: NSApplication asks
//! its delegate first), Close Window, Controls → Exit and the window's
//! close button. With nothing running the app quits at once; otherwise it
//! asks, as OBS does, so a recording never carries on unseen by mistake.

use std::cell::Cell;
use std::rc::{Rc, Weak};

use isocast_protocol::api::{EngineState, Method};
use isocast_protocol::stream::StreamState;
use slint::{CloseRequestResponse, ComponentHandle};

use crate::app::App;
use crate::{AppWindow, QuitPrompt, Ui};

thread_local! {
    /// The app, for NSApplication's question (it has no other way in).
    static APP: std::cell::RefCell<Weak<App>> = const { std::cell::RefCell::new(Weak::new()) };
    /// Quitting was chosen: the close and terminate requests that follow
    /// go through.
    static LEAVING: Cell<bool> = const { Cell::new(false) };
}

/// What is running that quitting would leave going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Running {
    recording: bool,
    streaming: bool,
}

fn running(app: &App) -> Running {
    let Ok(s) = app.session.try_borrow() else {
        return Running {
            recording: false,
            streaming: false,
        };
    };
    Running {
        recording: s
            .status
            .as_ref()
            .is_some_and(|x| matches!(x.state, EngineState::Recording | EngineState::Starting)),
        streaming: s.stream.as_ref().is_some_and(|x| {
            matches!(
                x.state,
                StreamState::Live | StreamState::Connecting | StreamState::Reconnecting
            )
        }),
    }
}

pub fn connect(app: &Rc<App>, window: &AppWindow) {
    APP.with(|a| *a.borrow_mut() = Rc::downgrade(app));
    let ui = window.global::<Ui>();
    let weak = Rc::downgrade(app);
    ui.on_quit(move || {
        if let Some(app) = weak.upgrade() {
            request(&app);
        }
    });
    let weak = Rc::downgrade(app);
    window
        .window()
        .on_close_requested(move || match weak.upgrade() {
            Some(app) if ask(&app) => CloseRequestResponse::KeepWindowShown,
            _ => CloseRequestResponse::HideWindow,
        });
    let prompt = window.global::<QuitPrompt>();
    let weak = Rc::downgrade(app);
    prompt.on_stop_and_quit(move || {
        if let Some(app) = weak.upgrade() {
            stop_and_quit(&app);
        }
    });
    prompt.on_keep(leave);
}

/// Quit, or ask first if something is running.
pub fn request(app: &Rc<App>) {
    if !ask(app) {
        leave();
    }
}

/// If something is running (and quitting wasn't already chosen), show the
/// question and say so; false means go ahead.
fn ask(app: &Rc<App>) -> bool {
    if LEAVING.with(Cell::get) {
        return false;
    }
    let now = running(app);
    if !now.recording && !now.streaming {
        return false;
    }
    show(app, now);
    true
}

fn show(app: &Rc<App>, now: Running) {
    let Some(window) = app.window() else { return };
    let prompt = window.global::<QuitPrompt>();
    let (what, keep) = match (now.recording, now.streaming) {
        (true, true) => ("recording and streaming", "Keep Both Going"),
        (true, false) => ("recording", "Keep Recording"),
        _ => ("streaming", "Keep Streaming"),
    };
    prompt.set_heading(format!("You are currently {what}.").into());
    let detail: &str = match (now.recording, now.streaming) {
        (true, true) => {
            "Stop and Quit saves the recording, ends the stream and closes Isocast. \
                 Keep Both Going closes the window and isocastd carries on; open Isocast \
                 again to see them and stop."
        }
        (true, false) => {
            "Stop and Quit saves the recording and closes Isocast. Keep Recording \
                 closes the window and isocastd carries on recording; open Isocast again \
                 to see it and stop."
        }
        _ => {
            "Stop and Quit ends the stream and closes Isocast. Keep Streaming closes \
                 the window and isocastd stays live; open Isocast again to see it and stop."
        }
    };
    let mut detail = detail.to_string();
    if crate::update::staged() {
        detail.push_str(
            " A downloaded Isocast update installs as Isocast closes; isocastd keeps \
             going on the version it runs, and the next launch restarts it once it is idle.",
        );
    }
    prompt.set_detail(detail.into());
    prompt.set_keep_text(keep.into());
    prompt.set_stopping(false);
    if window.global::<Ui>().get_dialog() != "quit" {
        app.open_dialog("quit", "");
    }
    // Asked from the Dock or another app: bring the window forward to ask.
    let _ = window.show();
}

/// The question as it would be asked (`--state quit:recording`,
/// `quit:streaming`, `quit:both`), for snapshots of an idle engine.
pub fn sample(app: &Rc<App>, which: &str) -> anyhow::Result<()> {
    let now = match which {
        "recording" | "" => Running {
            recording: true,
            streaming: false,
        },
        "streaming" => Running {
            recording: false,
            streaming: true,
        },
        "both" => Running {
            recording: true,
            streaming: true,
        },
        other => anyhow::bail!("quit:{other}: one of recording, streaming, both"),
    };
    show(app, now);
    Ok(())
}

/// Stop what's running, then quit; if a stop is refused, say why and stay.
fn stop_and_quit(app: &Rc<App>) {
    let now = running(app);
    let Some(window) = app.window() else { return };
    window.global::<QuitPrompt>().set_stopping(true);
    let left = Rc::new(Cell::new(
        usize::from(now.recording) + usize::from(now.streaming),
    ));
    let failed = Rc::new(Cell::new(false));
    if left.get() == 0 {
        leave();
        return;
    }
    let done = move |app: &Rc<App>, failed: &Rc<Cell<bool>>, left: &Rc<Cell<usize>>| {
        left.set(left.get() - 1);
        if left.get() > 0 {
            return;
        }
        if failed.get() {
            if let Some(window) = app.window() {
                window.global::<QuitPrompt>().set_stopping(false);
            }
            app.close_dialog();
            app.refresh_status(true);
        } else {
            leave();
        }
    };
    if now.recording {
        let (failed, left) = (failed.clone(), left.clone());
        app.call(Method::Stop, move |app, result| {
            if let Err(error) = result {
                app.failed("Couldn’t stop recording, so Isocast stays open", &error);
                failed.set(true);
            }
            done(app, &failed, &left);
        });
    }
    if now.streaming {
        app.call(Method::StreamStop(None), move |app, result| {
            if let Err(error) = result {
                app.failed("Couldn’t stop the stream, so Isocast stays open", &error);
                failed.set(true);
            }
            done(app, &failed, &left);
        });
    }
}

fn leave() {
    LEAVING.with(|l| l.set(true));
    let _ = slint::quit_event_loop();
}

/// NSApplication asks its delegate before quitting (⌘Q, the Dock, logging
/// out); winit's delegate doesn't answer, so give it the question. Call
/// once the event loop runs (winit sets the delegate then).
#[cfg(target_os = "macos")]
pub fn install_terminate_question() {
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Imp, Sel};
    use objc2::{class, msg_send, sel};

    /// NSTerminateCancel (0) when the question is shown, else
    /// NSTerminateNow (1).
    unsafe extern "C-unwind" fn should_terminate(
        _this: *mut AnyObject,
        _cmd: Sel,
        _sender: *mut AnyObject,
    ) -> usize {
        let app = APP.with(|a| a.borrow().upgrade());
        match app {
            Some(app) if ask(&app) => 0,
            _ => 1,
        }
    }

    // SAFETY: AppKit on the main thread; the method is added to the
    // delegate's class with the selector's real signature
    // (`-(NSApplicationTerminateReply)applicationShouldTerminate:(NSApplication *)`).
    unsafe {
        let ns_app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        let delegate: Option<Retained<AnyObject>> = msg_send![&*ns_app, delegate];
        let Some(delegate) = delegate else {
            isocast_diagnostics::diagnostic!("warn"; "quit: no application delegate to ask before quitting");
            return;
        };
        let class = delegate.class() as *const objc2::runtime::AnyClass as *mut _;
        let imp: Imp = std::mem::transmute::<
            unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize,
            Imp,
        >(should_terminate);
        let types = c"Q@:@";
        let added = objc2::ffi::class_addMethod(
            class,
            sel!(applicationShouldTerminate:),
            imp,
            types.as_ptr(),
        );
        if !added.as_bool() {
            objc2::ffi::class_replaceMethod(
                class,
                sel!(applicationShouldTerminate:),
                imp,
                types.as_ptr(),
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub fn install_terminate_question() {}
