//! Updating the app itself with Sparkle 2 (macOS): Isocast › Check for
//! Updates…, a check about once a day (Settings › General turns it off),
//! and Sparkle's own windows (release notes, Install / Remind Me Later /
//! Skip). Updating is how the app is packaged, not something the engine
//! does, so it has no method in the API (docs/notes.md, 2026-10-14).
//!
//! The released bundle carries Sparkle.framework in Contents/Frameworks
//! and its feed and public key in Info.plist; the framework is loaded at
//! run time, so a development build (neither) simply has no updater. This
//! module is Sparkle's updater delegate, and an update never interrupts a
//! recording or stream:
//!
//! - a scheduled check waits while isocastd records (or pauses), streams
//!   or exports, so no window opens over a take or on air, and runs once
//!   it is idle;
//! - an update chosen to install waits for the same, then isocastd is
//!   stopped as `isocast restart` stops it (refused while busy), so the
//!   relaunched app starts the new engine;
//! - an engine older than the bundled one (left running from before an
//!   update: Quit › Keep Recording lets Sparkle replace the app while
//!   isocastd records; or an older copy's engine on the same socket) is
//!   restarted with a notice once it is idle, and while it records or
//!   streams a notice says it restarts afterwards, so the app and its
//!   engine are the same build (with or without Sparkle);
//! - the feed is always the Info.plist's: one set in user defaults (which
//!   Sparkle would otherwise prefer) is cleared and ignored.

#[cfg(target_os = "macos")]
pub use mac::{
    automatic, available, check_now, feed_url, set_automatic, staged, start, stopping_engine,
};

#[cfg(not(target_os = "macos"))]
mod other {
    use std::rc::Rc;

    use crate::app::App;

    pub fn start(_app: &Rc<App>) {}
    pub fn available() -> bool {
        false
    }
    pub fn automatic() -> bool {
        false
    }
    pub fn set_automatic(_on: bool) {}
    pub fn check_now() {}
    pub fn staged() -> bool {
        false
    }
    pub fn stopping_engine() -> bool {
        false
    }
    pub fn feed_url() -> Option<String> {
        None
    }
}

#[cfg(not(target_os = "macos"))]
pub use other::{
    automatic, available, check_now, feed_url, set_automatic, staged, start, stopping_engine,
};

/// What keeps an update from installing now, from the engine's status:
/// a recording (paused or not), a stream, a timeline export, the virtual
/// camera (a call may be showing it) or the replay buffer. The engine's
/// `shutdown_if_idle` decides the same atomically before it stops.
fn busy_with(
    status: Option<&isocast_protocol::api::Status>,
    stream_active: bool,
    camera_active: bool,
    replay_active: bool,
) -> Option<&'static str> {
    use isocast_protocol::api::EngineState;
    use isocast_protocol::virtual_camera::VirtualCameraState;
    if status.is_some_and(|s| s.state != EngineState::Idle) {
        Some("the recording")
    } else if stream_active
        || status
            .and_then(|s| s.stream.as_ref())
            .is_some_and(|s| s.is_active())
    {
        Some("the stream")
    } else if status.is_some_and(|s| s.export.is_some()) {
        Some("the export")
    } else if camera_active
        || status
            .and_then(|s| s.virtual_camera.as_ref())
            .is_some_and(|c| {
                matches!(
                    c.state,
                    VirtualCameraState::Starting
                        | VirtualCameraState::Running
                        | VirtualCameraState::Stopping
                )
            })
    {
        Some("the virtual camera")
    } else if replay_active
        || status
            .and_then(|s| s.replay.as_ref())
            .is_some_and(|r| r.state == isocast_protocol::replay::ReplayState::Running)
    {
        Some("the replay buffer")
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::cell::{Cell, RefCell};
    use std::ptr::{self, NonNull};
    use std::rc::{Rc, Weak};
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use block2::{Block, RcBlock};
    use isocast_client::Client;
    use isocast_protocol::api::Method;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool, NSObject};
    use objc2::{ClassType, class, define_class, msg_send, sel};

    use crate::app::App;
    use crate::platform::{ns_string, rust_string};
    use crate::toasts::Toast;
    use crate::{ToastKind, daemon};

    /// The stale-engine notices' toast key: the restart replaces the notice.
    const STALE_KEY: &str = "stale-engine";

    /// The menu item's tag, to find it again after the menu bar is rebuilt.
    const MENU_TAG: isize = 0x150CA57;

    struct Sparkle {
        controller: Retained<AnyObject>,
        updater: Retained<AnyObject>,
        _delegate: Retained<AnyObject>,
        _observer: Option<Retained<AnyObject>>,
    }

    thread_local! {
        static APP: RefCell<Weak<App>> = const { RefCell::new(Weak::new()) };
        static SPARKLE: RefCell<Option<Sparkle>> = const { RefCell::new(None) };
        /// `tick`'s timer, with or without Sparkle: an older engine is
        /// restarted either way.
        static TICKER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
        /// The installation Sparkle let us postpone, until isocastd is idle.
        static INSTALL: RefCell<Option<RcBlock<dyn Fn()>>> = const { RefCell::new(None) };
        /// A scheduled check waited for isocastd.
        static DEFERRED: Cell<bool> = const { Cell::new(false) };
        /// Stopping isocastd to install.
        static FINISHING: Cell<bool> = const { Cell::new(false) };
        /// The waiting update was announced.
        static TOLD: Cell<bool> = const { Cell::new(false) };
        static RETRY_AT: Cell<Option<Instant>> = const { Cell::new(None) };
        /// The out-of-date isocastd (its pid) that was asked to stop.
        static RESTARTED: Cell<Option<Option<u32>>> = const { Cell::new(None) };
        /// The out-of-date isocastd (its pid) whose restart was announced
        /// as waiting for a recording, stream or other work to stop.
        static STALE_TOLD: Cell<Option<Option<u32>>> = const { Cell::new(None) };
        /// No stale-engine check until then: just after a restart, the
        /// status shown is still the old engine's.
        static RESTART_PAUSE: Cell<Option<Instant>> = const { Cell::new(None) };
        /// An update is downloaded and extracted: Sparkle installs it when
        /// the app quits, whatever isocastd is doing then.
        static STAGED: Cell<bool> = const { Cell::new(false) };
        /// When the postponed installation was let go (isocastd stopped).
        static INSTALLED_AT: Cell<Option<Instant>> = const { Cell::new(None) };
        /// Replies from the engine that was just stopped on purpose keep
        /// arriving (failed) for a moment; until then they aren't errors.
        static QUIET_UNTIL: Cell<Option<Instant>> = const { Cell::new(None) };
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements and the class
        // adds no Drop or ivars.
        #[unsafe(super(NSObject))]
        #[name = "IsocastUpdaterDelegate"]
        struct Delegate;

        impl Delegate {
            /// Scheduled checks wait while isocastd is busy (and run when
            /// it is idle); checks the person asked for go ahead.
            #[unsafe(method(updater:mayPerformUpdateCheck:error:))]
            fn may_perform(
                &self,
                _updater: &AnyObject,
                check: isize,
                error: *mut *mut AnyObject,
            ) -> Bool {
                const IN_BACKGROUND: isize = 1;
                let busy = app().and_then(|app| busy(&app));
                isocast_diagnostics::debug!(
                    "updates: check asked (background: {}, isocastd busy: {busy:?})",
                    check == IN_BACKGROUND
                );
                match busy {
                    Some(what) if check == IN_BACKGROUND => {
                        DEFERRED.set(true);
                        isocast_diagnostics::diagnostic!("info";
                            "updates: scheduled check waits until {what} stops");
                        if !error.is_null() {
                            // SAFETY: Sparkle passes a valid out pointer;
                            // the error is autoreleased, as it expects.
                            unsafe {
                                *error = Retained::autorelease_ptr(ns_error(&format!(
                                    "Isocast checks for updates after {what} stops."
                                )));
                            }
                        }
                        Bool::NO
                    }
                    _ => Bool::YES,
                }
            }

            /// Install and Relaunch: always postponed until isocastd is
            /// idle and stopped (see `tick`).
            #[unsafe(method(updater:shouldPostponeRelaunchForUpdate:untilInvokingBlock:))]
            fn postpone(
                &self,
                _updater: &AnyObject,
                item: &AnyObject,
                install: &Block<dyn Fn()>,
            ) -> Bool {
                isocast_diagnostics::diagnostic!("info";
                    "updates: {} is ready; it installs once isocastd is idle", version(item));
                INSTALL.with(|i| *i.borrow_mut() = Some(install.copy()));
                STAGED.set(true);
                TOLD.set(false);
                RETRY_AT.set(None);
                tick();
                Bool::YES
            }

            /// The feed is the Info.plist's, never one from user defaults.
            #[unsafe(method_id(feedURLStringForUpdater:))]
            fn feed(&self, _updater: &AnyObject) -> Option<Retained<AnyObject>> {
                // SAFETY: the main bundle's Info.plist, read on the main thread.
                unsafe { info(&main_bundle(), "SUFeedURL") }
            }

            #[unsafe(method(updater:didFindValidUpdate:))]
            fn found(&self, _updater: &AnyObject, item: &AnyObject) {
                isocast_diagnostics::diagnostic!("info"; "updates: found {}", version(item));
            }

            #[unsafe(method(updater:willInstallUpdate:))]
            fn installing(&self, _updater: &AnyObject, item: &AnyObject) {
                isocast_diagnostics::diagnostic!("info"; "updates: installing {}", version(item));
            }

            #[unsafe(method(updater:didExtractUpdate:))]
            fn extracted(&self, _updater: &AnyObject, item: &AnyObject) {
                isocast_diagnostics::diagnostic!("info";
                    "updates: {} is ready to install (installs on quit)", version(item));
                STAGED.set(true);
            }

            /// Every abort, including one after isocastd was stopped for
            /// the installation: start the engine again.
            #[unsafe(method(updater:didAbortWithError:))]
            fn aborted(&self, _updater: &AnyObject, error: &AnyObject) {
                if is_deferral(error) {
                    isocast_diagnostics::diagnostic!("info"; "updates: {}", describe(error));
                } else {
                    isocast_diagnostics::diagnostic!("warn";
                        "updates: stopped: {}", describe(error));
                    STAGED.set(false);
                }
                recover_engine("the update was aborted");
            }

            #[unsafe(method(updater:didFinishUpdateCycleForUpdateCheck:error:))]
            fn cycle_finished(&self, _updater: &AnyObject, _check: isize, error: *mut AnyObject) {
                if let Some(error) = NonNull::new(error) {
                    // SAFETY: a valid NSError for the call's duration.
                    let error = unsafe { error.as_ref() };
                    if !is_deferral(error) {
                        recover_engine("the update cycle ended with an error");
                    }
                }
            }

            #[unsafe(method(updaterWillRelaunchApplication:))]
            fn relaunching(&self, _updater: &AnyObject) {
                isocast_diagnostics::diagnostic!("info"; "updates: relaunching");
            }
        }
    );

    fn app() -> Option<Rc<App>> {
        APP.with(|a| a.borrow().upgrade())
    }

    /// Whether an update waits to install when the app quits.
    pub fn staged() -> bool {
        STAGED.get() || INSTALL.with(|i| i.borrow().is_some())
    }

    /// Stopping isocastd for an update (or it was stopped and Sparkle is
    /// installing): its link errors are expected then.
    pub fn stopping_engine() -> bool {
        FINISHING.get()
            || daemon::NO_LAUNCH.load(Ordering::SeqCst)
            || QUIET_UNTIL
                .get()
                .is_some_and(|until| Instant::now() < until)
    }

    /// Stopping isocastd on purpose: its link errors are expected for a while.
    fn quiet_for(seconds: u64) {
        QUIET_UNTIL.set(Some(Instant::now() + Duration::from_secs(seconds)));
    }

    /// If isocastd was stopped for an installation that didn't happen,
    /// let the link start it again.
    fn recover_engine(why: &str) {
        INSTALLED_AT.set(None);
        if daemon::NO_LAUNCH.swap(false, Ordering::SeqCst) {
            quiet_for(5);
            isocast_diagnostics::diagnostic!("warn";
                "updates: {why} after isocastd was stopped; starting it again");
            if let Some(app) = app() {
                app.refresh_status(true);
            }
        }
    }

    /// Our own refusal of a scheduled check while busy (not a failure).
    fn is_deferral(error: &AnyObject) -> bool {
        // SAFETY: NSError's domain, an NSString.
        unsafe {
            let domain: Option<Retained<AnyObject>> = msg_send![error, domain];
            domain.is_some_and(|d| rust_string(&d) == "dev.isocast.update")
        }
    }

    fn busy(app: &App) -> Option<&'static str> {
        let Ok(s) = app.session.try_borrow() else {
            return Some("the engine’s work");
        };
        let stream = s.stream.as_ref().is_some_and(|x| x.is_active());
        let camera = s.vcam.as_ref().is_some_and(|c| {
            use isocast_protocol::virtual_camera::VirtualCameraState as V;
            matches!(c.state, V::Starting | V::Running | V::Stopping)
        });
        let replay = s.busy_replay
            || s.replay
                .as_ref()
                .is_some_and(|r| r.state == isocast_protocol::replay::ReplayState::Running);
        super::busy_with(s.status.as_ref(), stream, camera, replay)
    }

    /// Start Sparkle if this bundle carries it (released builds do).
    pub fn start(app: &Rc<App>) {
        if app.mode.offscreen {
            return;
        }
        APP.with(|a| *a.borrow_mut() = Rc::downgrade(app));
        // SAFETY: AppKit and Sparkle on the main thread.
        match unsafe { load() } {
            Ok(sparkle) => {
                SPARKLE.with(|s| *s.borrow_mut() = Some(sparkle));
                // SAFETY: as above.
                unsafe { ensure_menu_item() };
            }
            Err(why) => isocast_diagnostics::diagnostic!("info"; "updates: off ({why})"),
        }
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), tick);
        TICKER.with(|t| *t.borrow_mut() = Some(timer));
    }

    unsafe fn load() -> Result<Sparkle, String> {
        let bundle = main_bundle();
        // SAFETY (whole function): AppKit/Foundation/Sparkle messages with
        // their documented signatures, on the main thread.
        unsafe {
            if info(&bundle, "SUFeedURL").is_none() || info(&bundle, "SUPublicEDKey").is_none() {
                return Err("no update feed in Info.plist: a development build".into());
            }
            let frameworks: Option<Retained<AnyObject>> =
                msg_send![&*bundle, privateFrameworksPath];
            let frameworks = frameworks.map(|f| rust_string(&f)).unwrap_or_default();
            let path = ns_string(&format!("{frameworks}/Sparkle.framework"));
            let framework: Option<Retained<AnyObject>> =
                msg_send![class!(NSBundle), bundleWithPath: &*path];
            let framework = framework.ok_or("no Sparkle.framework in Contents/Frameworks")?;
            let mut error: *mut AnyObject = ptr::null_mut();
            let loaded: Bool = msg_send![&*framework, loadAndReturnError: &mut error];
            if !loaded.as_bool() {
                let why = NonNull::new(error)
                    .map(|e| describe(e.as_ref()))
                    .unwrap_or_default();
                return Err(format!("Sparkle.framework didn't load: {why}"));
            }
            let class = AnyClass::get(c"SPUStandardUpdaterController")
                .ok_or("Sparkle.framework has no SPUStandardUpdaterController")?;
            let delegate: Retained<AnyObject> = msg_send![Delegate::class(), new];
            let allocated: objc2::rc::Allocated<AnyObject> = msg_send![class, alloc];
            let controller: Retained<AnyObject> = msg_send![
                allocated,
                initWithStartingUpdater: false,
                updaterDelegate: &*delegate,
                userDriverDelegate: ptr::null::<AnyObject>()
            ];
            let updater: Retained<AnyObject> = msg_send![&*controller, updater];
            // A feed in user defaults would win over Info.plist's (Sparkle's
            // deprecated setFeedURL:); the delegate ignores it too.
            let _: Option<Retained<AnyObject>> = msg_send![&*updater, clearFeedURLFromUserDefaults];
            let _: () = msg_send![&*controller, startUpdater];
            let observer = observe_menu_bar();
            isocast_diagnostics::diagnostic!("info"; "updates: on, feed {}",
                info(&bundle, "SUFeedURL").map(|f| rust_string(&f)).unwrap_or_default());
            Ok(Sparkle {
                controller,
                updater,
                _delegate: delegate,
                _observer: observer,
            })
        }
    }

    /// The update feed in this build's Info.plist (none in a development
    /// build), for Developer › Show Update Feed.
    pub fn feed_url() -> Option<String> {
        // SAFETY: the main bundle's Info.plist, read on the main thread.
        unsafe { info(&main_bundle(), "SUFeedURL").map(|f| rust_string(&f)) }
    }

    pub fn available() -> bool {
        SPARKLE.with(|s| s.borrow().is_some())
    }

    /// Whether Sparkle checks about once a day.
    pub fn automatic() -> bool {
        SPARKLE.with(|s| {
            s.borrow().as_ref().is_some_and(|s| {
                // SAFETY: a BOOL property, on the main thread.
                let on: Bool = unsafe { msg_send![&*s.updater, automaticallyChecksForUpdates] };
                on.as_bool()
            })
        })
    }

    pub fn set_automatic(on: bool) {
        SPARKLE.with(|s| {
            if let Some(s) = s.borrow().as_ref() {
                // SAFETY: as above; Sparkle keeps it in user defaults.
                let _: () = unsafe { msg_send![&*s.updater, setAutomaticallyChecksForUpdates: on] };
            }
        });
    }

    /// Check now, showing Sparkle's window (as the menu item does).
    pub fn check_now() {
        SPARKLE.with(|s| {
            if let Some(s) = s.borrow().as_ref() {
                // SAFETY: the controller's action, on the main thread.
                let _: () =
                    unsafe { msg_send![&*s.controller, checkForUpdates: ptr::null::<AnyObject>()] };
            }
        });
    }

    /// Every second: run a check that waited, and finish an update once
    /// isocastd is idle.
    fn tick() {
        let Some(app) = app() else { return };
        // Sparkle had the go-ahead a minute ago and the app still runs: the
        // installation didn't happen without an abort being reported.
        if INSTALLED_AT
            .get()
            .is_some_and(|at| at.elapsed() > Duration::from_secs(60))
        {
            recover_engine("the installation didn't start within a minute");
        }
        let busy = busy(&app);
        let waiting = INSTALL.with(|i| i.borrow().is_some());
        if waiting {
            // The waiting installation supersedes a waiting check.
            DEFERRED.set(false);
        }
        if busy.is_none() && DEFERRED.replace(false) {
            SPARKLE.with(|s| {
                if let Some(s) = s.borrow().as_ref() {
                    isocast_diagnostics::diagnostic!("info"; "updates: running the check that waited");
                    // SAFETY: on the main thread.
                    let _: () = unsafe { msg_send![&*s.updater, checkForUpdatesInBackground] };
                }
            });
        }
        if FINISHING.get() {
            return;
        }
        if !waiting {
            restart_stale_engine(&app, busy);
            return;
        }
        if let Some(what) = busy {
            if !TOLD.replace(true) {
                app.toast(
                    ToastKind::Info,
                    "Update ready",
                    format!("Isocast installs it and restarts after {what} stops."),
                );
            }
            return;
        }
        if RETRY_AT.get().is_some_and(|at| Instant::now() < at) {
            return;
        }
        finish();
    }

    /// An engine older than the one this app bundles (left running from
    /// before an update, or another copy's older engine on the same
    /// socket; `isocast_client::stale_engine`) is stopped once it is idle,
    /// and the link then starts the bundled one. While it records, streams
    /// or does other work (`busy`), a notice says it restarts afterwards.
    /// Once per engine process.
    fn restart_stale_engine(app: &Rc<App>, busy: Option<&'static str>) {
        if RESTART_PAUSE
            .get()
            .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        let stale = app.session.try_borrow().ok().and_then(|s| {
            let status = s
                .status
                .as_ref()
                .filter(|_| s.link == Some(daemon::Link::Connected))?;
            isocast_client::stale_engine(status.daemon.as_ref())
        });
        let Some(stale) = stale else { return };
        if let Some(what) = busy {
            if STALE_TOLD.replace(Some(stale.pid)) != Some(stale.pid) {
                isocast_diagnostics::diagnostic!("info";
                    "updates: isocastd {} is older than this app (build {}); restarting it after {what} stops",
                    stale.running, stale.expected);
                // Stays until dismissed, or replaced (same key) once the
                // engine restarts.
                app.show_toast(
                    Toast::new(
                        ToastKind::Info,
                        "Older engine running",
                        format!(
                            "isocastd {} is older than this Isocast. It keeps running {what} \
                             and restarts as this version’s engine once {what} stops.",
                            stale.running
                        ),
                    )
                    .key(STALE_KEY)
                    .action2("OK", |_| {}),
                );
            }
            return;
        }
        if RESTARTED.get() == Some(stale.pid) {
            return;
        }
        RESTARTED.set(Some(stale.pid));
        FINISHING.set(true);
        quiet_for(40);
        isocast_diagnostics::diagnostic!("info";
            "updates: isocastd {} is older than this app (build {}); restarting it",
            stale.running, stale.expected);
        std::thread::spawn(move || {
            let result = stop_engine();
            let _ = slint::invoke_from_event_loop(move || {
                FINISHING.set(false);
                match result {
                    Stopped::Done => {
                        quiet_for(5);
                        // The status shown is the old engine's until the
                        // new one answers.
                        RESTART_PAUSE.set(Some(Instant::now() + Duration::from_secs(15)));
                        if let Some(app) = self::app() {
                            app.show_toast(
                                Toast::new(
                                    ToastKind::Info,
                                    "Engine restarted",
                                    format!(
                                        "Isocast Engine {} was still running from an older Isocast; \
                                         Isocast restarted it with this version’s engine.",
                                        stale.running
                                    ),
                                )
                                .key(STALE_KEY),
                            );
                            app.refresh_status(true);
                        }
                    }
                    // Busy again (or with what the status doesn't show,
                    // as a `tune` benchmark): try again in a while.
                    Stopped::Busy(_) => {
                        QUIET_UNTIL.set(None);
                        RESTARTED.set(None);
                        RESTART_PAUSE.set(Some(Instant::now() + Duration::from_secs(15)));
                    }
                    Stopped::Failed(why) => {
                        QUIET_UNTIL.set(None);
                        isocast_diagnostics::diagnostic!("warn";
                            "updates: couldn't restart the older isocastd: {why}");
                        if let Some(app) = self::app() {
                            let pid = stale.pid.map_or_else(
                                || "`pgrep -f 'Isocast Engine'`".to_string(),
                                |pid| pid.to_string(),
                            );
                            app.show_toast(
                                Toast::new(
                                    ToastKind::Warning,
                                    "Older engine running",
                                    format!(
                                        "Isocast Engine {} is older than this Isocast and couldn’t be \
                                         restarted ({why}). Quit Isocast, end it with `kill {pid}` \
                                         (it finishes its files first), then open Isocast again.",
                                        stale.running
                                    ),
                                )
                                .key(STALE_KEY)
                                // Stays until dismissed: it says what to do.
                                .action2("OK", |_| {}),
                            );
                        }
                    }
                }
            });
        });
    }

    /// Stop isocastd (only if it is still idle), then let Sparkle install
    /// and relaunch.
    fn finish() {
        FINISHING.set(true);
        quiet_for(40);
        daemon::NO_LAUNCH.store(true, Ordering::SeqCst);
        isocast_diagnostics::diagnostic!("info"; "updates: stopping isocastd to install");
        std::thread::spawn(|| {
            let result = stop_engine();
            let _ = slint::invoke_from_event_loop(move || finished(result));
        });
    }

    enum Stopped {
        Done,
        /// The engine's refusal (what is running).
        Busy(String),
        Failed(String),
    }

    /// `isocast restart`'s stop: the engine's `shutdown_if_idle`, refused
    /// atomically while it records, streams, exports or feeds the virtual
    /// camera; then wait for it to finish and let go of its socket.
    fn stop_engine() -> Stopped {
        let mut client = match Client::connect() {
            Ok(Some(client)) => client,
            Ok(None) => return Stopped::Done,
            Err(error) => return Stopped::Failed(format!("{error:#}")),
        };
        if let Err(error) = client.call::<serde_json::Value>(Method::ShutdownIfIdle) {
            return match error.downcast_ref::<isocast_client::DaemonError>() {
                Some(refusal) if refusal.code() == isocast_protocol::api::RpcError::WRONG_STATE => {
                    Stopped::Busy(refusal.0.message.clone())
                }
                _ => Stopped::Failed(format!("{error:#}")),
            };
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            match Client::connect() {
                Ok(None) => return Stopped::Done,
                Ok(Some(_)) | Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        Stopped::Failed("isocastd was still running 30 s after it was asked to stop".into())
    }

    fn finished(result: Stopped) {
        FINISHING.set(false);
        match result {
            Stopped::Done => {
                isocast_diagnostics::diagnostic!("info"; "updates: isocastd stopped; installing");
                let install = INSTALL.with(|i| i.borrow_mut().take());
                if let Some(install) = install {
                    INSTALLED_AT.set(Some(Instant::now()));
                    install.call(());
                }
            }
            Stopped::Busy(what) => {
                QUIET_UNTIL.set(None);
                daemon::NO_LAUNCH.store(false, Ordering::SeqCst);
                // The status may not show what refused (a `tune`
                // benchmark): ask again in a while, not every second.
                RETRY_AT.set(Some(Instant::now() + Duration::from_secs(15)));
                isocast_diagnostics::diagnostic!("info"; "updates: not installing yet: {what}");
            }
            Stopped::Failed(why) => {
                QUIET_UNTIL.set(None);
                daemon::NO_LAUNCH.store(false, Ordering::SeqCst);
                RETRY_AT.set(Some(Instant::now() + Duration::from_secs(15)));
                isocast_diagnostics::diagnostic!("warn"; "updates: couldn't stop isocastd: {why}");
                if let Some(app) = app() {
                    app.toast(
                        ToastKind::Warning,
                        "Update waiting",
                        format!("Isocast couldn’t stop isocastd to install the update ({why}). It tries again shortly."),
                    );
                }
            }
        }
    }

    /// Put Isocast › Check for Updates… after About, in the application
    /// menu Slint makes (and makes again whenever the menu bar changes).
    unsafe fn ensure_menu_item() {
        // SAFETY (whole function): AppKit's menus, on the main thread.
        unsafe {
            let Some(controller) =
                SPARKLE.with(|s| s.borrow().as_ref().map(|s| s.controller.clone()))
            else {
                return;
            };
            let ns_app: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
            let main: Option<Retained<AnyObject>> = msg_send![&*ns_app, mainMenu];
            let Some(main) = main else { return };
            let count: isize = msg_send![&*main, numberOfItems];
            if count == 0 {
                return;
            }
            let first: Retained<AnyObject> = msg_send![&*main, itemAtIndex: 0isize];
            let menu: Option<Retained<AnyObject>> = msg_send![&*first, submenu];
            let Some(menu) = menu else { return };
            let existing: Option<Retained<AnyObject>> = msg_send![&*menu, itemWithTag: MENU_TAG];
            let items: isize = msg_send![&*menu, numberOfItems];
            if existing.is_some() || items == 0 {
                return;
            }
            let title = ns_string("Check for Updates…");
            let key = ns_string("");
            let allocated: objc2::rc::Allocated<AnyObject> = msg_send![class!(NSMenuItem), alloc];
            let item: Retained<AnyObject> = msg_send![
                allocated,
                initWithTitle: &*title,
                action: sel!(checkForUpdates:),
                keyEquivalent: &*key
            ];
            let _: () = msg_send![&*item, setTarget: &*controller];
            let _: () = msg_send![&*item, setTag: MENU_TAG];
            let _: () = msg_send![&*menu, insertItem: &*item, atIndex: items.min(1)];
        }
    }

    /// Slint rebuilds the menu bar when what it shows changes; put the item
    /// back each time.
    unsafe fn observe_menu_bar() -> Option<Retained<AnyObject>> {
        let block = RcBlock::new(|_notification: NonNull<AnyObject>| {
            // SAFETY: AppKit posts menu notifications on the main thread.
            unsafe { ensure_menu_item() }
        });
        // SAFETY: the notification center keeps the block; the returned
        // observer is kept for the app's life.
        unsafe {
            let center: Retained<AnyObject> =
                msg_send![class!(NSNotificationCenter), defaultCenter];
            let name = ns_string("NSMenuDidAddItemNotification");
            msg_send![
                &*center,
                addObserverForName: &*name,
                object: ptr::null::<AnyObject>(),
                queue: ptr::null::<AnyObject>(),
                usingBlock: &*block
            ]
        }
    }

    fn main_bundle() -> Retained<AnyObject> {
        // SAFETY: NSBundle's shared main bundle.
        unsafe { msg_send![class!(NSBundle), mainBundle] }
    }

    unsafe fn info(bundle: &AnyObject, key: &str) -> Option<Retained<AnyObject>> {
        // SAFETY: Info.plist lookup with an NSString key.
        unsafe {
            let key = ns_string(key);
            msg_send![bundle, objectForInfoDictionaryKey: &*key]
        }
    }

    /// An appcast item's "0.0.91 (91)".
    fn version(item: &AnyObject) -> String {
        // SAFETY: SUAppcastItem's string properties.
        unsafe {
            let shown: Option<Retained<AnyObject>> = msg_send![item, displayVersionString];
            let build: Option<Retained<AnyObject>> = msg_send![item, versionString];
            format!(
                "{} ({})",
                shown.map(|s| rust_string(&s)).unwrap_or_default(),
                build.map(|s| rust_string(&s)).unwrap_or_default()
            )
        }
    }

    fn describe(error: &AnyObject) -> String {
        // SAFETY: NSError's localizedDescription.
        unsafe {
            let text: Option<Retained<AnyObject>> = msg_send![error, localizedDescription];
            text.map(|t| rust_string(&t)).unwrap_or_default()
        }
    }

    fn ns_error(message: &str) -> Retained<AnyObject> {
        // SAFETY: NSError and NSDictionary class constructors.
        unsafe {
            let domain = ns_string("dev.isocast.update");
            let key = ns_string("NSLocalizedDescription");
            let text = ns_string(message);
            let info: Retained<AnyObject> =
                msg_send![class!(NSDictionary), dictionaryWithObject: &*text, forKey: &*key];
            msg_send![class!(NSError), errorWithDomain: &*domain, code: 1isize, userInfo: &*info]
        }
    }
}
