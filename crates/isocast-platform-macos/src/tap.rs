//! What the computer, or one app, plays, through a Core Audio process tap
//! (macOS 14.2+): a tap on the processes' output, read through a private
//! aggregate device. Unlike ScreenCaptureKit, no display stream runs.
//!
//! A tap names processes by their Core Audio process objects, which exist
//! only once a process has opened audio, so the tap's process list is kept
//! up to date as processes come and go. An app's sound often comes from
//! helper processes (a browser's renderers, WebKit's GPU process), so a
//! process belongs to the app macOS holds responsible for it.
//!
//! The aggregate device is clocked by the system output. When the output
//! changes (AirPods connect) or the tap's format changes, the tap and
//! device are built again; the engine fills the short gap with silence.
//! Audio is delivered as 48 kHz stereo floats stamped on the host clock,
//! resampled when the output runs at another rate.

use std::ffi::{CStr, c_void};
use std::mem::size_of;
use std::ptr::{self, NonNull};
use std::slice;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use isocast_engine::{AudioSink, SAMPLE_RATE, Time};
use isocast_protocol::devices::{AudioApp, Permission};
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::NSRunningApplication;
use objc2_audio_toolbox::{
    AudioConverterDispose, AudioConverterFillComplexBuffer, AudioConverterNew, AudioConverterRef,
    AudioConverterSetProperty, kAudioConverterQuality_High,
    kAudioConverterSampleRateConverterQuality,
};
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice,
    AudioObjectAddPropertyListener, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectRemovePropertyListener, AudioObjectSetPropertyData, CATapDescription,
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
    kAudioDevicePropertyDeviceIsAlive, kAudioDevicePropertyDeviceUID,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyProcessObjectList,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyDescription,
    kAudioTapPropertyFormat, kAudioTapPropertyUID,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioStreamPacketDescription,
    AudioTimeStamp, AudioTimeStampFlags, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
    kAudioFormatFlagIsPacked, kAudioFormatLinearPCM,
};
use objc2_core_foundation::{CFDictionary, CFString};
use objc2_core_media::CMClock;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use objc2_core_audio::{
    kAudioDevicePropertyStreams, kAudioObjectPropertyScopeInput, kAudioStreamPropertyVirtualFormat,
};

use crate::from_cm_time;
use crate::output::{address, default_output, property, property_bytes, string_property};

/// Channels delivered: taps mix down to stereo.
pub const CHANNELS: u32 = 2;
/// Set to `screencapturekit` to capture system and app audio the way
/// macOS before 14.2 must (for comparing the two, and as an escape hatch).
const BACKEND_ENV: &str = "ISOCAST_AUDIO_CAPTURE";
/// How long a burst of Core Audio notifications is let settle before the
/// tap is rebuilt (switching outputs sends several).
const SETTLE: Duration = Duration::from_millis(100);
/// Rebuilding gives up after this many failures in a row.
const REBUILD_ATTEMPTS: u32 = 5;

/// `AudioHardwareCreateProcessTap` and `AudioHardwareDestroyProcessTap`,
/// looked up at run time: they're new in macOS 14.2 and the app runs on
/// 14.0, where linking them directly would keep it from launching.
struct TapFunctions {
    create: unsafe extern "C" fn(*const CATapDescription, *mut AudioObjectID) -> i32,
    destroy: unsafe extern "C" fn(AudioObjectID) -> i32,
}

fn functions() -> Option<&'static TapFunctions> {
    static FUNCTIONS: OnceLock<Option<TapFunctions>> = OnceLock::new();
    FUNCTIONS
        .get_or_init(|| {
            let find = |name: &CStr| {
                // SAFETY: a plain symbol lookup in the loaded images.
                let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
                (!symbol.is_null()).then_some(symbol)
            };
            let create = find(c"AudioHardwareCreateProcessTap")?;
            let destroy = find(c"AudioHardwareDestroyProcessTap")?;
            // SAFETY: Core Audio's own signatures (AudioHardware.h).
            Some(unsafe {
                TapFunctions {
                    create: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*const CATapDescription, *mut AudioObjectID) -> i32,
                    >(create),
                    destroy: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(AudioObjectID) -> i32,
                    >(destroy),
                }
            })
        })
        .as_ref()
}

/// TCC's own preflight and request (private; Apple's sample for process
/// taps and other recorders use them the same way): the only way to know
/// whether System Audio Recording is allowed without opening a tap, which
/// shows macOS's prompt and blocks until it's answered.
struct Tcc {
    preflight: unsafe extern "C" fn(*const CFString, *const c_void) -> i32,
    request: unsafe extern "C" fn(*const CFString, *const c_void, *mut c_void),
}

fn tcc() -> Option<&'static Tcc> {
    static TCC: OnceLock<Option<Tcc>> = OnceLock::new();
    TCC.get_or_init(|| {
        let path = c"/System/Library/PrivateFrameworks/TCC.framework/Versions/A/TCC";
        // SAFETY: loading a system framework and looking up two symbols.
        unsafe {
            let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW);
            let (preflight, request) = if handle.is_null() {
                (ptr::null_mut(), ptr::null_mut())
            } else {
                (
                    libc::dlsym(handle, c"TCCAccessPreflight".as_ptr()),
                    libc::dlsym(handle, c"TCCAccessRequest".as_ptr()),
                )
            };
            if preflight.is_null() || request.is_null() {
                // System audio recording then reads as "not asked", so
                // screen recording records it where that's allowed.
                isocast_diagnostics::diagnostic!("warn";
                    "audio tap: TCC's preflight isn't available; system audio recording \
                     can't be checked");
                return None;
            }
            Some(Tcc {
                preflight: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*const CFString, *const c_void) -> i32,
                >(preflight),
                request: std::mem::transmute::<
                    *mut c_void,
                    unsafe extern "C" fn(*const CFString, *const c_void, *mut c_void),
                >(request),
            })
        }
    })
    .as_ref()
}

const AUDIO_CAPTURE: &str = "kTCCServiceAudioCapture";

/// Whether System Audio Recording is allowed: granted where macOS has no
/// such permission (before 14.2), not determined if TCC can't say.
pub fn permission() -> Permission {
    if !supported() {
        return Permission::Granted;
    }
    let Some(tcc) = tcc() else {
        return Permission::NotDetermined;
    };
    let service = CFString::from_str(AUDIO_CAPTURE);
    // SAFETY: a CFString and no options, as TCC.h declares.
    match unsafe { (tcc.preflight)(&*service, ptr::null()) } {
        0 => Permission::Granted,
        1 => Permission::Denied,
        _ => Permission::NotDetermined,
    }
}

/// Ask for System Audio Recording, showing macOS's prompt if it hasn't
/// been decided, and wait up to `timeout` for the answer.
pub fn request_permission(timeout: Duration) -> Permission {
    let state = permission();
    if state != Permission::NotDetermined {
        return state;
    }
    let Some(tcc) = tcc() else {
        return Permission::NotDetermined;
    };
    let (tx, rx) = mpsc::channel();
    let tx = std::sync::Mutex::new(tx);
    // The callback's argument is a CoreFoundation Boolean (a byte).
    let answered = block2::RcBlock::new(move |granted: u8| {
        let _ = tx.lock().map(|tx| tx.send(granted != 0));
    });
    let service = CFString::from_str(AUDIO_CAPTURE);
    let block = block2::RcBlock::as_ptr(&answered);
    // SAFETY: TCC copies the block; the service is a CFString.
    unsafe { (tcc.request)(&*service, ptr::null(), block.cast()) };
    match rx.recv_timeout(timeout) {
        Ok(true) => Permission::Granted,
        Ok(false) => Permission::Denied,
        Err(_) => permission(),
    }
}

/// Which way system and app audio are captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Core Audio process taps (macOS 14.2+).
    ProcessTap,
    /// A ScreenCaptureKit stream (before 14.2, or when asked for).
    ScreenCaptureKit,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::ProcessTap => "Core Audio process tap",
            Backend::ScreenCaptureKit => "ScreenCaptureKit",
        }
    }
}

/// Process taps on macOS 14.2 and later where System Audio Recording is
/// allowed. Where it isn't but screen recording is, ScreenCaptureKit, so
/// a setup that recorded before doesn't stop at a new prompt (or fail
/// once turned down); with neither, a tap, whose prompt asks for less.
/// `ISOCAST_AUDIO_CAPTURE=screencapturekit` forces ScreenCaptureKit.
pub fn backend() -> Backend {
    let forced = std::env::var(BACKEND_ENV)
        .is_ok_and(|v| v.eq_ignore_ascii_case("screencapturekit") || v.eq_ignore_ascii_case("sck"));
    if forced || !supported() {
        return Backend::ScreenCaptureKit;
    }
    match permission() {
        Permission::Granted => Backend::ProcessTap,
        _ if objc2_core_graphics::CGPreflightScreenCaptureAccess() => Backend::ScreenCaptureKit,
        _ => Backend::ProcessTap,
    }
}

/// Whether this macOS has process taps (14.2 or later).
pub fn supported() -> bool {
    let version = crate::system::sysctl_string("kern.osproductversion").unwrap_or_default();
    let mut parts = version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) >= (14, 2) && functions().is_some()
}

/// What a tap captures.
#[derive(Debug, Clone)]
pub enum Target {
    /// Everything played, except these apps (and this process when
    /// `exclude_own`).
    System {
        exclude: Vec<String>,
        exclude_own: bool,
    },
    /// What one app plays.
    App(String),
}

/// A process Core Audio knows: one that has opened audio.
#[derive(Debug, Clone)]
struct Process {
    object: AudioObjectID,
    pid: i32,
    /// The app (or tool) it belongs to: the one macOS holds responsible.
    app: Owner,
    playing: bool,
}

/// An app, or a command-line tool without a bundle (like `afplay`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Owner {
    pid: i32,
    name: String,
    bundle_id: String,
    /// The executable's name ("afplay", "Google Chrome Helper").
    executable: String,
    /// The process's own bundle id, when it differs from the app's.
    process_bundle_id: String,
}

impl Owner {
    /// Matched as apps are elsewhere: the name or bundle id, ignoring case,
    /// or part of the name. Tools match by their executable's name too.
    fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return false;
        }
        let name = self.name.to_lowercase();
        let executable = self.executable.to_lowercase();
        [&self.bundle_id, &self.process_bundle_id]
            .iter()
            .any(|id| !id.is_empty() && id.to_lowercase() == query)
            || name == query
            || executable == query
            || (!name.is_empty() && name.contains(&query))
    }
}

/// Every process Core Audio knows, with the app each belongs to.
fn processes() -> Vec<Process> {
    let Some(bytes) = property_bytes(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyProcessObjectList,
        kAudioObjectPropertyScopeGlobal,
    ) else {
        return Vec::new();
    };
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|id| AudioObjectID::from_ne_bytes(*id))
        .filter_map(|object| {
            let pid = property::<i32>(
                object,
                kAudioProcessPropertyPID,
                kAudioObjectPropertyScopeGlobal,
            )?;
            let process_bundle_id =
                string_property(object, kAudioProcessPropertyBundleID).unwrap_or_default();
            let playing = property::<u32>(
                object,
                kAudioProcessPropertyIsRunningOutput,
                kAudioObjectPropertyScopeGlobal,
            )
            .is_some_and(|v| v != 0);
            // A helper belongs to the app responsible for it; a tool run
            // from a terminal is its own (the terminal didn't play it).
            let mut app = owner(responsible(pid));
            if app.bundle_id.is_empty() {
                app = owner(pid);
            }
            app.process_bundle_id = process_bundle_id;
            Some(Process {
                object,
                pid,
                app,
                playing,
            })
        })
        .collect()
}

/// The process macOS holds responsible for `pid` (the app that launched a
/// helper or XPC service), or `pid` itself.
fn responsible(pid: i32) -> i32 {
    unsafe extern "C" {
        // libSystem; not in public headers, but stable since macOS 10.14
        // (permissions.rs uses its sibling the same way).
        fn responsibility_get_pid_responsible_for_pid(pid: libc::pid_t) -> libc::pid_t;
    }
    // SAFETY: takes and returns a plain pid.
    let owner = unsafe { responsibility_get_pid_responsible_for_pid(pid) };
    if owner > 0 { owner } else { pid }
}

/// The app with process id `pid`, or the tool's executable name.
fn owner(pid: i32) -> Owner {
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid);
    let name = app
        .as_ref()
        .and_then(|a| a.localizedName())
        .map(|n| n.to_string())
        .unwrap_or_default();
    let bundle_id = app
        .as_ref()
        .and_then(|a| a.bundleIdentifier())
        .map(|n| n.to_string())
        .unwrap_or_default();
    let executable = executable(pid);
    Owner {
        pid,
        name: if name.is_empty() {
            executable.clone()
        } else {
            name
        },
        bundle_id,
        executable,
        process_bundle_id: String::new(),
    }
}

/// A process's executable name, as `ps` shows it.
fn executable(pid: i32) -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer and its size match.
    let n = unsafe { libc::proc_name(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf8_lossy(&buffer[..n as usize]).into_owned()
}

/// The apps that have opened audio, by name, for choosing one to capture
/// or leave out: apps, and tools (like `afplay`) while they play. System
/// services and this process aren't listed.
pub fn audio_apps() -> Vec<AudioApp> {
    let own = std::process::id() as i32;
    let mut apps: Vec<AudioApp> = Vec::new();
    for process in processes() {
        let tool = process.app.bundle_id.is_empty();
        if process.pid == own
            || process.app.pid == own
            || process.app.name.is_empty()
            || (tool && !process.playing)
        {
            continue;
        }
        match apps.iter_mut().find(|a| a.pid == process.app.pid as u32) {
            Some(app) => app.playing |= process.playing,
            None => apps.push(AudioApp {
                name: process.app.name.clone(),
                bundle_id: (!process.app.bundle_id.is_empty())
                    .then(|| process.app.bundle_id.clone()),
                pid: process.app.pid as u32,
                playing: process.playing,
            }),
        }
    }
    apps.sort_by(|a, b| {
        (!a.playing, a.name.to_lowercase()).cmp(&(!b.playing, b.name.to_lowercase()))
    });
    apps
}

/// The process objects `target` taps (an app) or leaves out (system).
fn chosen(target: &Target, processes: &[Process]) -> Vec<AudioObjectID> {
    let own = std::process::id() as i32;
    let mut objects: Vec<AudioObjectID> = processes
        .iter()
        .filter(|p| match target {
            Target::App(query) => p.pid != own && p.app.matches(query),
            Target::System {
                exclude,
                exclude_own,
            } => (*exclude_own && p.pid == own) || exclude.iter().any(|q| p.app.matches(q)),
        })
        .map(|p| p.object)
        .collect();
    objects.sort_unstable();
    objects
}

/// Check that `query` names a running app (or a tool playing audio); the
/// error lists the apps that have opened audio.
pub fn check_app(query: &str) -> Result<String> {
    let all = processes();
    if let Some(found) = all.iter().find(|p| p.app.matches(query)) {
        return Ok(found.app.name.clone());
    }
    if let Some(name) = running_app(query) {
        return Ok(name);
    }
    let mut names: Vec<String> = audio_apps().into_iter().map(|a| a.name).collect();
    names.dedup();
    bail!(
        "no running app matches {query:?}; apps with audio: {}",
        if names.is_empty() {
            "none".into()
        } else {
            names.join(", ")
        }
    )
}

/// A running app matching `query` that hasn't opened audio yet.
fn running_app(query: &str) -> Option<String> {
    let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
    let apps = workspace.runningApplications();
    apps.iter()
        .map(|app| owner(app.processIdentifier()))
        .find(|o| o.matches(query))
        .map(|o| o.name)
}

/// A running tap capture. Dropping it stops the capture.
pub struct TapCapture {
    control: Sender<Event>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for TapCapture {
    fn drop(&mut self) {
        let _ = self.control.send(Event::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// What the worker thread is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    /// Processes started or stopped using audio.
    Processes,
    /// The output device, the tap's format, or the aggregate device
    /// changed: build the tap again.
    Rebuild,
    Stop,
}

/// Where delivered audio goes, shared by every tap the capture builds.
struct Delivery {
    samples: isocast_engine::SamplesCallback,
    stopped: Box<dyn Fn(String) + Send + Sync>,
}

/// Start capturing `target`. Returns the capture and its channel count.
pub fn capture(target: Target, sink: AudioSink) -> Result<(TapCapture, u32)> {
    if functions().is_none() {
        bail!("Core Audio process taps need macOS 14.2 or later");
    }
    if let Target::App(query) = &target {
        check_app(query)?;
    }
    let delivery = Arc::new(Delivery {
        samples: sink.samples,
        stopped: sink.stopped,
    });
    let (control, events) = mpsc::channel();
    let listeners = Listeners::new(control.clone());
    let tap = match Tap::start(&target, delivery.clone()) {
        Ok(tap) => tap,
        Err(error) => {
            listeners.remove();
            return Err(error);
        }
    };
    isocast_diagnostics::diagnostic!("info";
        "audio tap: started on {} at {} Hz", tap.output_name, tap.format.mSampleRate);
    listeners.watch_tap(tap.tap);
    let worker = std::thread::Builder::new()
        .name("isocast.audio-tap".into())
        .spawn(move || run(target, tap, delivery, events, listeners))
        .context("could not start the audio tap's thread")?;
    Ok((
        TapCapture {
            control,
            worker: Some(worker),
        },
        CHANNELS,
    ))
}

/// The worker: keeps the tap's processes current and rebuilds it when
/// the output changes, until stopped.
fn run(
    target: Target,
    mut tap: Tap,
    delivery: Arc<Delivery>,
    events: Receiver<Event>,
    listeners: Listeners,
) {
    let mut failures = 0;
    // An event to act on before waiting for the next.
    let mut pending: Option<Event> = None;
    loop {
        let received = match pending.take() {
            Some(event) => Ok(event),
            None => events.recv_timeout(Duration::from_secs(1)),
        };
        let event = match received {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => {
                // A device that went away doesn't always say so.
                if tap.alive() {
                    continue;
                }
                Event::Rebuild
            }
            Err(RecvTimeoutError::Disconnected) => Event::Stop,
        };
        if event == Event::Stop {
            break;
        }
        // Let a burst of output changes settle, then act once. Process
        // changes act at once: an app to leave out is heard until then.
        if event == Event::Rebuild {
            std::thread::sleep(SETTLE);
        }
        let mut rebuild = event == Event::Rebuild;
        let mut stop = false;
        while let Ok(more) = events.try_recv() {
            rebuild |= more == Event::Rebuild;
            stop |= more == Event::Stop;
        }
        if stop {
            break;
        }
        if !rebuild {
            match tap.update(&target) {
                Ok(()) => continue,
                Err(error) => {
                    isocast_diagnostics::diagnostic!("warn";
                        "audio tap: could not update its processes ({error:#}); rebuilding");
                }
            }
        }
        listeners.unwatch_tap(tap.tap);
        drop(tap);
        tap = loop {
            match Tap::start(&target, delivery.clone()) {
                Ok(tap) => {
                    failures = 0;
                    break tap;
                }
                Err(error) => {
                    failures += 1;
                    isocast_diagnostics::diagnostic!("warn";
                        "audio tap: rebuild {failures} failed: {error:#}");
                    if failures >= REBUILD_ATTEMPTS {
                        (delivery.stopped)(format!(
                            "the audio tap could not be rebuilt: {error:#}"
                        ));
                        listeners.remove();
                        return;
                    }
                    if events.recv_timeout(Duration::from_millis(500)) == Ok(Event::Stop) {
                        listeners.remove();
                        return;
                    }
                }
            }
        };
        listeners.watch_tap(tap.tap);
        isocast_diagnostics::diagnostic!("info";
            "audio tap: rebuilt on {} at {} Hz", tap.output_name, tap.format.mSampleRate);
        // Most of what changed while rebuilding is in the new tap already.
        // Process changes are applied again (cheap when nothing changed);
        // an output change counts only if the output now differs.
        std::thread::sleep(SETTLE);
        let mut output_changed = false;
        while let Ok(more) = events.try_recv() {
            match more {
                Event::Stop => {
                    listeners.unwatch_tap(tap.tap);
                    listeners.remove();
                    return;
                }
                Event::Rebuild => output_changed = true,
                Event::Processes => {}
            }
        }
        if let Err(error) = tap.update(&target) {
            isocast_diagnostics::diagnostic!("warn";
                "audio tap: could not update its processes after a rebuild ({error:#})");
        }
        if output_changed && tap.output_moved() {
            pending = Some(Event::Rebuild);
        }
    }
    listeners.unwatch_tap(tap.tap);
    listeners.remove();
    drop(tap);
}

/// Core Audio property listeners that tell the worker what changed. The
/// context they're given is never freed (see `remove`).
struct Listeners {
    context: *mut Sender<Event>,
}

// SAFETY: the context is a Sender (Send + Sync) that is never freed.
unsafe impl Send for Listeners {}

const SYSTEM_EVENTS: [u32; 2] = [
    kAudioHardwarePropertyProcessObjectList,
    kAudioHardwarePropertyDefaultOutputDevice,
];

impl Listeners {
    fn new(control: Sender<Event>) -> Self {
        let this = Listeners {
            context: Box::into_raw(Box::new(control)),
        };
        for selector in SYSTEM_EVENTS {
            this.add(kAudioObjectSystemObject as AudioObjectID, selector);
        }
        this
    }

    fn add(&self, object: AudioObjectID, selector: u32) {
        let address = address(selector, kAudioObjectPropertyScopeGlobal);
        // SAFETY: the context outlives the listener (removed first).
        unsafe {
            AudioObjectAddPropertyListener(
                object,
                NonNull::from(&address),
                Some(changed),
                self.context.cast(),
            )
        };
    }

    fn drop_listener(&self, object: AudioObjectID, selector: u32) {
        let address = address(selector, kAudioObjectPropertyScopeGlobal);
        unsafe {
            AudioObjectRemovePropertyListener(
                object,
                NonNull::from(&address),
                Some(changed),
                self.context.cast(),
            )
        };
    }

    fn watch_tap(&self, tap: AudioObjectID) {
        self.add(tap, kAudioTapPropertyFormat);
    }

    fn unwatch_tap(&self, tap: AudioObjectID) {
        self.drop_listener(tap, kAudioTapPropertyFormat);
    }

    fn remove(self) {
        for selector in SYSTEM_EVENTS {
            self.drop_listener(kAudioObjectSystemObject as AudioObjectID, selector);
        }
        // The context (a Sender) is left allocated: Core Audio doesn't say
        // a listener can't still be running when removing it returns, and a
        // few bytes per capture are cheaper than that question.
        let _ = self.context;
    }
}

/// A Core Audio property listener: tell the worker.
unsafe extern "C-unwind" fn changed(
    _object: AudioObjectID,
    count: u32,
    addresses: NonNull<AudioObjectPropertyAddress>,
    context: *mut c_void,
) -> i32 {
    // SAFETY: the context is the Listeners' Sender, alive while listening.
    let control = unsafe { &*context.cast::<Sender<Event>>() };
    let addresses = unsafe { slice::from_raw_parts(addresses.as_ptr(), count as usize) };
    for address in addresses {
        let event = if address.mSelector == kAudioHardwarePropertyProcessObjectList {
            Event::Processes
        } else {
            Event::Rebuild
        };
        let _ = control.send(event);
    }
    0
}

/// One tap, its aggregate device and the IOProc reading it.
struct Tap {
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    description: Retained<CATapDescription>,
    /// The process objects in the description, sorted.
    processes: Vec<AudioObjectID>,
    format: AudioStreamBasicDescription,
    output_name: String,
    /// The system output's UID when the tap was built.
    output_uid: Option<String>,
    /// What the IOProc uses; boxed so its address stays put.
    reader: *mut Reader,
}

// SAFETY: Core Audio objects may be used from any thread; the reader is
// only touched by the IOProc until it's stopped, then freed by drop.
unsafe impl Send for Tap {}

impl Tap {
    fn start(target: &Target, delivery: Arc<Delivery>) -> Result<Self> {
        let functions = functions().context("Core Audio process taps need macOS 14.2")?;
        let processes = chosen(target, &processes());
        let description = describe(target, &processes);
        let mut tap: AudioObjectID = 0;
        let status = unsafe { (functions.create)(Retained::as_ptr(&description), &mut tap) };
        if status != 0 || tap == 0 {
            bail!(
                "could not create an audio tap (Core Audio error {status}{})",
                permission_hint(status)
            );
        }
        // Built step by step; drop undoes whatever was made.
        let mut this = Tap {
            tap,
            aggregate: 0,
            proc_id: None,
            description,
            processes,
            format: zero_format(),
            output_name: String::new(),
            output_uid: None,
            reader: ptr::null_mut(),
        };
        let tap_uid = string_property(tap, kAudioTapPropertyUID).context("the tap has no UID")?;
        this.format = property::<AudioStreamBasicDescription>(
            tap,
            kAudioTapPropertyFormat,
            kAudioObjectPropertyScopeGlobal,
        )
        .context("the tap has no format")?;
        // The output it follows, for the log and to notice a change made
        // while it was being built.
        let output = default_output();
        this.output_uid = output.and_then(|o| string_property(o, kAudioDevicePropertyDeviceUID));
        this.output_name = output.map_or_else(|| "no output".into(), crate::output::device_name);
        this.aggregate = aggregate(&tap_uid)?;
        let inputs = property_bytes(
            this.aggregate,
            kAudioDevicePropertyStreams,
            kAudioObjectPropertyScopeInput,
        )
        .map_or(0, |b| b.len() / 4);
        if inputs != 1 {
            bail!("the tap's device has {inputs} input streams, not one");
        }
        // The IOProc gets the tap as the aggregate device's only input
        // stream; read the format it says it delivers.
        if let Some(format) = input_format(this.aggregate) {
            this.format = format;
        }
        let reader = Reader::new(this.format, delivery)?;
        this.reader = Box::into_raw(Box::new(reader));
        let mut proc_id: AudioDeviceIOProcID = None;
        check(
            unsafe {
                AudioDeviceCreateIOProcID(
                    this.aggregate,
                    Some(io_proc),
                    this.reader.cast(),
                    NonNull::from(&mut proc_id),
                )
            },
            "attach to the tap's device",
        )?;
        this.proc_id = proc_id;
        check(
            unsafe { AudioDeviceStart(this.aggregate, this.proc_id) },
            "start the tap's device",
        )?;
        Ok(this)
    }

    /// Point the tap at the processes `target` names now, if they changed.
    fn update(&mut self, target: &Target) -> Result<()> {
        let processes = chosen(target, &processes());
        if processes == self.processes {
            return Ok(());
        }
        let description = describe(target, &processes);
        // The tap keeps its UUID, so the aggregate device stays attached.
        unsafe { description.setUUID(&self.description.UUID()) };
        let pointer = Retained::as_ptr(&description);
        let status = unsafe {
            AudioObjectSetPropertyData(
                self.tap,
                NonNull::from(&address(
                    kAudioTapPropertyDescription,
                    kAudioObjectPropertyScopeGlobal,
                )),
                0,
                ptr::null(),
                size_of::<*const CATapDescription>() as u32,
                NonNull::from(&pointer).cast(),
            )
        };
        check(status, "change the tap's processes")?;
        self.description = description;
        self.processes = processes;
        Ok(())
    }

    /// Whether the system output is no longer the one the tap was built on.
    fn output_moved(&self) -> bool {
        let now = default_output().and_then(|o| string_property(o, kAudioDevicePropertyDeviceUID));
        now != self.output_uid
    }

    /// Whether the aggregate device still runs.
    fn alive(&self) -> bool {
        property::<u32>(
            self.aggregate,
            kAudioDevicePropertyDeviceIsAlive,
            kAudioObjectPropertyScopeGlobal,
        )
        .is_some_and(|alive| alive != 0)
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        unsafe {
            if self.proc_id.is_some() {
                // From a thread other than the IOProc's, this waits until
                // the IOProc has returned for the last time.
                AudioDeviceStop(self.aggregate, self.proc_id);
                AudioDeviceDestroyIOProcID(self.aggregate, self.proc_id);
            }
            if self.aggregate != 0 {
                AudioHardwareDestroyAggregateDevice(self.aggregate);
            }
            if let Some(functions) = functions() {
                (functions.destroy)(self.tap);
            }
            if !self.reader.is_null() {
                drop(Box::from_raw(self.reader));
            }
        }
    }
}

/// The format of a device's first input stream.
fn input_format(device: AudioObjectID) -> Option<AudioStreamBasicDescription> {
    let streams = property_bytes(
        device,
        kAudioDevicePropertyStreams,
        kAudioObjectPropertyScopeInput,
    )?;
    let stream = AudioObjectID::from_ne_bytes(*streams.as_chunks::<4>().0.first()?);
    property::<AudioStreamBasicDescription>(
        stream,
        kAudioStreamPropertyVirtualFormat,
        kAudioObjectPropertyScopeGlobal,
    )
    .filter(|f| f.mSampleRate > 0.0)
}

fn zero_format() -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: 0.0,
        mFormatID: 0,
        mFormatFlags: 0,
        mBytesPerPacket: 0,
        mFramesPerPacket: 0,
        mBytesPerFrame: 0,
        mChannelsPerFrame: 0,
        mBitsPerChannel: 0,
        mReserved: 0,
    }
}

/// A tap description for `target` with these process objects.
fn describe(target: &Target, processes: &[AudioObjectID]) -> Retained<CATapDescription> {
    let numbers: Vec<Retained<NSNumber>> =
        processes.iter().map(|&p| NSNumber::new_u32(p)).collect();
    let list = NSArray::from_retained_slice(&numbers);
    let description = unsafe {
        match target {
            Target::System { .. } => CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &list,
            ),
            Target::App(_) => {
                CATapDescription::initStereoMixdownOfProcesses(CATapDescription::alloc(), &list)
            }
        }
    };
    unsafe {
        description.setName(&NSString::from_str("Isocast audio capture"));
        // Only this process sees it; it goes away with the process.
        description.setPrivate(true);
    }
    description
}

/// A private aggregate device holding only the tap. No sub-device: an
/// output device that has inputs too (AirPods, most USB interfaces) would
/// bring its microphone streams into the aggregate, opening the
/// microphone (its permission and indicator) and putting its sound before
/// the tap's.
fn aggregate(tap_uid: &str) -> Result<AudioObjectID> {
    let key = |k: &CStr| NSString::from_str(k.to_str().unwrap_or_default());
    let uid = format!("{DEVICE_PREFIX}{}", uuid());
    let yes = NSNumber::new_bool(true);
    let no = NSNumber::new_bool(false);
    let tap = NSDictionary::<NSString, AnyObject>::from_slices(
        &[
            &*key(kAudioSubTapUIDKey),
            &*key(kAudioSubTapDriftCompensationKey),
        ],
        &[NSString::from_str(tap_uid).as_ref(), yes.as_ref()],
    );
    let taps = NSArray::from_retained_slice(&[tap]);
    let name = NSString::from_str("Isocast audio capture");
    let uid = NSString::from_str(&uid);
    let keys = [
        key(kAudioAggregateDeviceNameKey),
        key(kAudioAggregateDeviceUIDKey),
        key(kAudioAggregateDeviceIsPrivateKey),
        key(kAudioAggregateDeviceIsStackedKey),
        key(kAudioAggregateDeviceTapAutoStartKey),
        key(kAudioAggregateDeviceTapListKey),
    ];
    let values: [&AnyObject; 6] = [
        name.as_ref(),
        uid.as_ref(),
        yes.as_ref(),
        no.as_ref(),
        yes.as_ref(),
        taps.as_ref(),
    ];
    let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let description = NSDictionary::<NSString, AnyObject>::from_slices(&keys, &values);
    // SAFETY: NSDictionary is toll-free bridged with CFDictionary.
    let dictionary = unsafe { &*(Retained::as_ptr(&description).cast::<CFDictionary>()) };
    let mut device: AudioObjectID = 0;
    check(
        unsafe { AudioHardwareCreateAggregateDevice(dictionary, NonNull::from(&mut device)) },
        "create the tap's aggregate device",
    )?;
    Ok(device)
}

/// The UID prefix of the aggregate devices taps are read through.
const DEVICE_PREFIX: &str = "dev.isocast.tap.";

/// Whether a device UID is one of this process's tap devices.
pub(crate) fn is_ours(uid: &str) -> bool {
    uid.starts_with(DEVICE_PREFIX)
}

fn uuid() -> String {
    objc2_foundation::NSUUID::new().UUIDString().to_string()
}

fn check(status: i32, what: &str) -> Result<()> {
    match status {
        0 => Ok(()),
        status => Err(anyhow!("could not {what} (Core Audio error {status})")),
    }
}

/// What a refused tap most likely means.
fn permission_hint(status: i32) -> &'static str {
    // 'nope' and kAudioHardwareIllegalOperationError ('nope') /
    // kAudioDevicePermissionsError ('!hog').
    if status == 0x6e6f7065 || status == 0x21686f67 {
        "; System Audio Recording may not be allowed: System Settings → Privacy & Security → \
         Screen & System Audio Recording"
    } else {
        ""
    }
}

/// Reads the tap's buffers on Core Audio's real-time thread: interleaves
/// them, resamples to 48 kHz if needed, stamps them on the host clock and
/// delivers them.
struct Reader {
    delivery: Arc<Delivery>,
    channels: usize,
    planar: bool,
    rate: f64,
    resampler: Option<Resampler>,
    interleaved: Vec<f32>,
    /// Whether a bad format was reported already.
    failed: bool,
}

impl Reader {
    fn new(format: AudioStreamBasicDescription, delivery: Arc<Delivery>) -> Result<Self> {
        let float = format.mFormatFlags & kAudioFormatFlagIsFloat != 0;
        if format.mFormatID != kAudioFormatLinearPCM || !float || format.mBitsPerChannel != 32 {
            bail!(
                "the tap delivers an unsupported format ({}-bit, float: {float})",
                format.mBitsPerChannel
            );
        }
        let resampler = if format.mSampleRate == f64::from(SAMPLE_RATE) {
            None
        } else {
            Some(Resampler::new(format.mSampleRate)?)
        };
        Ok(Reader {
            delivery,
            channels: format.mChannelsPerFrame.max(1) as usize,
            planar: format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0,
            rate: format.mSampleRate,
            resampler,
            interleaved: Vec::new(),
            failed: false,
        })
    }

    fn read(&mut self, list: &AudioBufferList, time: &AudioTimeStamp) {
        if !time.mFlags.contains(AudioTimeStampFlags::HostTimeValid) {
            return;
        }
        let buffers =
            unsafe { slice::from_raw_parts(list.mBuffers.as_ptr(), list.mNumberBuffers as usize) };
        let frames = stereo(buffers, self.channels, self.planar, &mut self.interleaved);
        if frames == 0 {
            return;
        }
        let at = from_cm_time(unsafe { CMClock::make_host_time_from_system_units(time.mHostTime) });
        let delivery = &self.delivery;
        match &mut self.resampler {
            None => (delivery.samples)(&self.interleaved, at),
            Some(resampler) => match resampler.convert(&self.interleaved, at) {
                Ok(Some((samples, at))) => (delivery.samples)(samples, at),
                Ok(None) => {}
                Err(error) => {
                    if !self.failed {
                        self.failed = true;
                        (delivery.stopped)(format!("{error:#} (at {} Hz)", self.rate));
                    }
                }
            },
        }
    }
}

/// The tap's buffers as interleaved stereo in `out`: a mono tap plays in
/// both channels, extra channels are dropped. Returns the frame count.
fn stereo(buffers: &[AudioBuffer], channels: usize, planar: bool, out: &mut Vec<f32>) -> usize {
    let read = |buffer: &AudioBuffer| -> &[f32] {
        if buffer.mData.is_null() {
            return &[];
        }
        unsafe {
            slice::from_raw_parts(
                buffer.mData.cast::<f32>(),
                buffer.mDataByteSize as usize / size_of::<f32>(),
            )
        }
    };
    out.clear();
    if planar {
        let left = buffers.first().map(read).unwrap_or_default();
        let right = buffers.get(1).map(read).unwrap_or(left);
        let frames = left.len().min(right.len());
        out.reserve(frames * 2);
        for i in 0..frames {
            out.push(left[i]);
            out.push(right[i]);
        }
        frames
    } else {
        let Some(buffer) = buffers.first() else {
            return 0;
        };
        // The buffer says how many channels it interleaves; the format's
        // count stands in if it doesn't.
        let channels = match buffer.mNumberChannels as usize {
            0 => channels.max(1),
            n => n,
        };
        let data = read(buffer);
        let frames = data.len() / channels;
        out.reserve(frames * 2);
        for frame in data.chunks_exact(channels) {
            out.push(frame[0]);
            out.push(*frame.get(1).unwrap_or(&frame[0]));
        }
        frames
    }
}

/// Converts interleaved stereo floats at the output's rate to 48 kHz.
struct Resampler {
    converter: AudioConverterRef,
    rate: f64,
    pending: Vec<f32>,
    lent: Vec<f32>,
    output: Vec<f32>,
    /// Input frames handed in, and output frames made, since the start.
    frames_in: u64,
    frames_out: u64,
}

const NO_MORE_INPUT: i32 = 0x6e6f6d6f; // 'nomo', our own

impl Resampler {
    fn new(rate: f64) -> Result<Self> {
        let format = |rate: f64| AudioStreamBasicDescription {
            mSampleRate: rate,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let (from, to) = (format(rate), format(f64::from(SAMPLE_RATE)));
        let mut converter: AudioConverterRef = ptr::null_mut();
        check(
            unsafe {
                AudioConverterNew(
                    NonNull::from(&from),
                    NonNull::from(&to),
                    NonNull::from(&mut converter),
                )
            },
            "create a resampler",
        )?;
        let quality = kAudioConverterQuality_High;
        unsafe {
            AudioConverterSetProperty(
                converter,
                kAudioConverterSampleRateConverterQuality,
                size_of::<u32>() as u32,
                NonNull::from(&quality).cast(),
            )
        };
        Ok(Resampler {
            converter,
            rate,
            pending: Vec::new(),
            lent: Vec::new(),
            output: Vec::new(),
            frames_in: 0,
            frames_out: 0,
        })
    }

    /// Resample `input` (captured at `at`); the 48 kHz samples ready now
    /// and when the first of them was captured.
    fn convert(&mut self, input: &[f32], at: Time) -> Result<Option<(&[f32], Time)>> {
        let start_in = self.frames_in;
        let start_out = self.frames_out;
        self.pending.extend_from_slice(input);
        self.frames_in += (input.len() / 2) as u64;
        let room = (input.len() / 2) as f64 * f64::from(SAMPLE_RATE) / self.rate;
        let mut capacity = room.ceil() as usize + 64;
        self.output.resize(capacity * 2, 0.0);
        let mut taken = 0usize;
        let mut feed = Feed {
            pending: &self.pending,
            taken: &mut taken,
            lent: &mut self.lent,
        };
        let mut list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: (capacity * 8) as u32,
                mData: self.output.as_mut_ptr().cast(),
            }],
        };
        let mut packets = capacity as u32;
        let status = unsafe {
            AudioConverterFillComplexBuffer(
                self.converter,
                Some(supply),
                (&mut feed as *mut Feed).cast(),
                NonNull::from(&mut packets),
                NonNull::from(&mut list),
                ptr::null_mut::<AudioStreamPacketDescription>(),
            )
        };
        if status != 0 && status != NO_MORE_INPUT {
            bail!("resampling failed (Core Audio error {status})");
        }
        self.pending.drain(..(taken * 2).min(self.pending.len()));
        capacity = packets as usize;
        if capacity == 0 {
            return Ok(None);
        }
        self.frames_out += capacity as u64;
        // Output frame n came from input frame n × rate / 48000; `at` is
        // when input frame `start_in` was captured.
        let source = start_out as f64 * self.rate / f64::from(SAMPLE_RATE);
        let offset = (source - start_in as f64) / self.rate;
        let at = Time::from_nanos(at.as_nanos() + (offset * 1e9).round() as i64);
        Ok(Some((&self.output[..capacity * 2], at)))
    }
}

impl Drop for Resampler {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.converter) };
    }
}

struct Feed<'a> {
    pending: &'a [f32],
    taken: &'a mut usize,
    lent: &'a mut Vec<f32>,
}

/// The resampler's input callback: the pending frames it asks for, once.
unsafe extern "C-unwind" fn supply(
    _converter: AudioConverterRef,
    count: NonNull<u32>,
    data: NonNull<AudioBufferList>,
    _descriptions: *mut *mut AudioStreamPacketDescription,
    user: *mut c_void,
) -> i32 {
    let feed = unsafe { &mut *(user as *mut Feed) };
    let available = feed.pending.len() / 2 - *feed.taken;
    let wanted = unsafe { *count.as_ptr() } as usize;
    let n = wanted.min(available);
    let list = unsafe { &mut *data.as_ptr() };
    if n == 0 {
        unsafe { *count.as_ptr() = 0 };
        list.mBuffers[0].mDataByteSize = 0;
        list.mBuffers[0].mData = ptr::null_mut();
        return NO_MORE_INPUT;
    }
    let from = *feed.taken * 2;
    feed.lent.clear();
    feed.lent
        .extend_from_slice(&feed.pending[from..from + n * 2]);
    *feed.taken += n;
    list.mNumberBuffers = 1;
    list.mBuffers[0].mNumberChannels = 2;
    list.mBuffers[0].mDataByteSize = (n * 8) as u32;
    list.mBuffers[0].mData = feed.lent.as_mut_ptr().cast();
    unsafe { *count.as_ptr() = n as u32 };
    0
}

/// The aggregate device's IOProc, on Core Audio's real-time thread.
unsafe extern "C-unwind" fn io_proc(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    context: *mut c_void,
) -> i32 {
    // SAFETY: the context is the tap's Reader, freed only after the IOProc
    // has stopped; only this callback uses it meanwhile.
    let reader = unsafe { &mut *context.cast::<Reader>() };
    let (list, time) = unsafe { (input.as_ref(), input_time.as_ref()) };
    // A panic must not unwind into Core Audio.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| reader.read(list, time)));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str) -> Owner {
        Owner {
            pid: 1,
            name: name.into(),
            bundle_id: String::new(),
            executable: name.into(),
            process_bundle_id: String::new(),
        }
    }

    #[test]
    fn apps_and_tools_match_by_name_bundle_or_part() {
        let music = Owner {
            pid: 2,
            name: "Music".into(),
            bundle_id: "com.apple.Music".into(),
            executable: "Music".into(),
            process_bundle_id: String::new(),
        };
        assert!(music.matches("music"));
        assert!(music.matches("com.apple.music"));
        assert!(music.matches("mus"));
        assert!(!music.matches("com.apple"));
        assert!(!music.matches(""));
        let helper = Owner {
            process_bundle_id: "com.google.Chrome.helper".into(),
            ..tool("Google Chrome")
        };
        assert!(helper.matches("com.google.chrome.helper"));
        assert!(tool("afplay").matches("afplay"));
        assert!(!tool("afplay").matches("say"));
    }

    #[test]
    fn interleaved_and_planar_buffers_become_stereo() {
        let mut out = Vec::new();
        let mut data = [1.0f32, 2.0, 3.0, 4.0];
        let buffer = AudioBuffer {
            mNumberChannels: 2,
            mDataByteSize: 16,
            mData: data.as_mut_ptr().cast(),
        };
        assert_eq!(stereo(&[buffer], 2, false, &mut out), 2);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        let mut left = [1.0f32, 3.0];
        let mut right = [2.0f32, 4.0];
        let planar = [
            AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: 8,
                mData: left.as_mut_ptr().cast(),
            },
            AudioBuffer {
                mNumberChannels: 1,
                mDataByteSize: 8,
                mData: right.as_mut_ptr().cast(),
            },
        ];
        assert_eq!(stereo(&planar, 2, true, &mut out), 2);
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0]);
        let mut mono = [5.0f32, 6.0];
        let buffer = AudioBuffer {
            mNumberChannels: 1,
            mDataByteSize: 8,
            mData: mono.as_mut_ptr().cast(),
        };
        assert_eq!(stereo(&[buffer], 1, false, &mut out), 2);
        assert_eq!(out, [5.0, 5.0, 6.0, 6.0]);
    }

    #[test]
    fn resampling_keeps_time_and_length() {
        let mut resampler = Resampler::new(44_100.0).unwrap();
        let mut made = 0;
        let mut first = None;
        for i in 0..100u64 {
            let chunk = vec![0.25f32; 441 * 2];
            let at = Time::from_nanos((i * 10_000_000) as i64);
            if let Some((samples, at)) = resampler.convert(&chunk, at).unwrap() {
                first.get_or_insert(at);
                made += samples.len() / 2;
            }
        }
        // One second in, about one second out (the converter holds back
        // a few frames).
        assert!((47_000..=48_000).contains(&made), "{made}");
        assert_eq!(first, Some(Time::from_nanos(0)));
    }
}
