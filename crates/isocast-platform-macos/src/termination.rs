//! AppKit must never take its default exit() path while the daemon owns files.

use std::cell::Cell;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSApplication, NSApplicationDelegate, NSApplicationTerminateReply};
use objc2_foundation::{NSObject, NSObjectProtocol};

pub struct Ivars {
    stop: Arc<dyn Fn() + Send + Sync>,
    requested: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject has no additional subclassing invariants. AppKit calls
    // this delegate on the main thread; media shutdown runs on a worker.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    #[name = "IsocastDaemonTerminationDelegate"]
    pub(crate) struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}
    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationShouldTerminate:))]
        fn should_terminate(&self, _app: &NSApplication) -> NSApplicationTerminateReply {
            if !self.ivars().requested.replace(true) {
                let stop = self.ivars().stop.clone();
                std::thread::spawn(move || {
                    stop();
                    // Keep AppKit/main-queue capture work alive until every
                    // recording is finalized, then acknowledge the quit AE.
                    dispatch2::DispatchQueue::main().exec_async(|| {
                        let mtm = MainThreadMarker::new().unwrap();
                        NSApplication::sharedApplication(mtm)
                            .replyToApplicationShouldTerminate(true);
                    });
                });
            }
            NSApplicationTerminateReply::TerminateLater
        }
    }
);

pub(crate) fn install(
    app: &NSApplication,
    stop: impl Fn() + Send + Sync + 'static,
) -> Retained<Delegate> {
    let mtm = MainThreadMarker::new().expect("termination delegate is installed on main");
    let delegate = Delegate::alloc(mtm).set_ivars(Ivars {
        stop: Arc::new(stop),
        requested: Cell::new(false),
    });
    // SAFETY: initialize the NSObject superclass once after setting Rust ivars.
    let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    delegate
}
