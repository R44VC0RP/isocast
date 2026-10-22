//! The web view and its two links to the native side:
//!
//! - `isocast://app/…` is served by [`SchemeHandler`]: the interface's
//!   files, the setup schema (`/_schema.json`), monitors' video
//!   (`/_frames/<target>?w=<max width>&fps=<rate>&id=<n>`: the daemon's
//!   `frames` stream, its binary records passed through as they arrive, for
//!   the page to read with `response.body.getReader()`; 501 from daemons
//!   without it; `{"native": "frames_rate", "id", "fps"}` changes its
//!   rate), and single frames (`/_frame/<target>?after=<seq>&w=<max
//!   width>`), which come back as JPEG or PNG bytes with `X-Isocast-Seq`,
//!   `-Width`, `-Height` and (program, preview) `-Revision` headers.
//! - `window.webkit.messageHandlers.isocast.postMessage(json)` reaches
//!   [`Bridge`], which relays `{"method", "params"}` to the daemon and
//!   answers `{"result"}` or `{"error": {"code", "message"}}`, or runs a
//!   window command (`{"native": "drag"}` and so on): the menus, the open
//!   and save panels, and the files people pick (setups, scene collections
//!   and profiles to import or export, read and written with the CLI's
//!   code; the daemon only takes their documents).
//!
//! Daemon events reach the page as `window.__isocastEvent(event)`.

use isocast_ipc::Stream as UnixStream;
use std::collections::HashMap;
use std::io::Read;
use std::ptr;
use std::thread;

use block2::{DynBlock, RcBlock};
use isocast_client::library;
use isocast_protocol::library::{LibraryDocument, LibraryKind};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSModalResponseOK, NSOpenPanel, NSSavePanel, NSWindow, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSData, NSDictionary, NSHTTPURLResponse, NSNumber, NSObject, NSObjectNSKeyValueCoding,
    NSObjectProtocol, NSString, NSURL, NSURLRequest, ns_string,
};
use objc2_web_kit::{
    WKContentWorld, WKScriptMessage, WKScriptMessageHandlerWithReply, WKURLSchemeHandler,
    WKURLSchemeTask, WKUserContentController, WKWebView, WKWebViewConfiguration,
    WKWebsiteDataStore,
};
use serde_json::{Value, json};

use super::{DAEMON, on_main, reply_text, with_main};

type Reply = RcBlock<dyn Fn(*mut AnyObject, *mut NSString)>;

/// Requests waiting for the daemon, on the main thread.
#[derive(Default)]
pub(crate) struct State {
    replies: HashMap<u64, Reply>,
    /// Scheme tasks waiting for a frame, by address; a task WebKit stops
    /// is removed, and never answered.
    tasks: HashMap<usize, Retained<ProtocolObject<dyn WKURLSchemeTask>>>,
    /// The daemon connections of monitors' streams, by task, with the id
    /// the page gave each (for `frames_rate`); closed when WebKit stops the
    /// task.
    streams: HashMap<usize, (u64, UnixStream)>,
    next: u64,
}

/// A web view filling `window`, loading the interface.
pub(crate) fn web_view(
    mtm: MainThreadMarker,
    window: &NSWindow,
    query: &str,
) -> Retained<WKWebView> {
    let config = unsafe { WKWebViewConfiguration::new(mtm) };
    // Snapshots neither read nor change the interface's saved preferences
    // (panel sizes, studio mode).
    if !query.is_empty() {
        unsafe { config.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm)) };
    }
    let scheme = SchemeHandler::new(mtm);
    unsafe {
        config.setURLSchemeHandler_forURLScheme(
            Some(ProtocolObject::from_ref(&*scheme)),
            ns_string!("isocast"),
        )
    };
    let bridge = Bridge::new(mtm);
    unsafe {
        config
            .userContentController()
            .addScriptMessageHandlerWithReply_contentWorld_name(
                ProtocolObject::from_ref(&*bridge),
                &WKContentWorld::pageWorld(mtm),
                ns_string!("isocast"),
            )
    };
    let frame = window
        .contentView()
        .map(|view| view.bounds())
        .unwrap_or_default();
    let web =
        unsafe { WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &config) };
    // The window's colour shows until the page paints: no white flash.
    unsafe {
        web.setValue_forKey(
            Some(&NSNumber::new_bool(false)),
            ns_string!("drawsBackground"),
        );
        web.setUnderPageBackgroundColor(Some(&super::background_color()));
    }
    web.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    if cfg!(debug_assertions) || std::env::var_os("ISOCAST_INSPECT").is_some() {
        unsafe { web.setInspectable(true) };
    }
    window.setContentView(Some(&web));
    let address = format!("isocast://app/index.html{query}");
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(&address)) {
        unsafe { web.loadRequest(&NSURLRequest::requestWithURL(&url)) };
    }
    web
}

/// Send `event` to the interface (from any thread).
pub(crate) fn emit(event: Value) {
    let script = format!("window.__isocastEvent&&window.__isocastEvent({})", event);
    on_main(move || {
        if let Some(web) = with_main(|main| main.web.clone()) {
            unsafe { web.evaluateJavaScript_completionHandler(&NSString::from_str(&script), None) };
        }
    });
}

/// Run a script in the interface (on the main thread).
pub(crate) fn eval(script: &str) {
    if let Some(web) = with_main(|main| main.web.clone()) {
        unsafe { web.evaluateJavaScript_completionHandler(&NSString::from_str(script), None) };
    }
}

fn answer(reply: &Reply, value: &Value) {
    let text = reply_text(value);
    reply.call((super::object_ptr(&*text), ptr::null_mut()));
}

/// A message from the interface.
fn receive(body: Option<String>, reply: Reply) {
    let Some(message) = body.and_then(|text| serde_json::from_str::<Value>(&text).ok()) else {
        let error = json!({"error": {"code": -32600, "message": "expected a JSON string"}});
        answer(&reply, &error);
        return;
    };
    if let Some(command) = message.get("native").and_then(Value::as_str) {
        let result = native(command, &message);
        answer(&reply, &result);
        return;
    }
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        let error =
            json!({"error": {"code": -32600, "message": "give a method or a native command"}});
        answer(&reply, &error);
        return;
    };
    let method = method.to_string();
    let params = message.get("params").filter(|p| !p.is_null()).cloned();
    let Some(id) = with_main(|main| {
        main.state.next += 1;
        main.state.replies.insert(main.state.next, reply);
        main.state.next
    }) else {
        return;
    };
    thread::spawn(move || {
        let result = DAEMON.call(&method, params);
        on_main(move || {
            if let Some(reply) = with_main(|main| main.state.replies.remove(&id)).flatten() {
                answer(&reply, &result);
            }
        });
    });
}

/// Window and system commands from the interface, on the main thread.
fn native(command: &str, message: &Value) -> Value {
    let Some((window, mtm)) = with_main(|main| main.window.clone())
        .and_then(|window| Some((window, MainThreadMarker::new()?)))
    else {
        return json!({"result": null});
    };
    let text = |name: &str| {
        message
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    let result = match command {
        // The daemon connection as it stands, for a page that just loaded.
        "connection" => DAEMON.connection(),
        "reconnect" => {
            DAEMON.reconnect();
            Value::Null
        }
        // A monitor's stream changes its rate (`/_frames/…&id=<id>`), like
        // {"native": "frames_rate", "id": 3, "fps": 60}.
        "frames_rate" => {
            let number = |name: &str| message.get(name).and_then(Value::as_u64);
            if let (Some(id), Some(fps)) = (number("id"), number("fps")) {
                let line = format!("{}\n", json!({ "fps": fps.clamp(1, 60) }));
                with_main(|main| {
                    let found = main
                        .state
                        .streams
                        .values_mut()
                        .find(|(each, _)| *each == id);
                    if let Some((_, connection)) = found {
                        use std::io::Write;
                        let _ = connection.write_all(line.as_bytes());
                    }
                });
            }
            Value::Null
        }
        // The window's title, like "Isocast — My setup".
        "title" => {
            window.setTitle(&NSString::from_str(text("title")));
            Value::Null
        }
        // Menu items' check marks and titles, by action name.
        "menu_state" => {
            let none = Value::Null;
            super::set_menu_state(
                mtm,
                message.get("checked").unwrap_or(&none),
                message.get("titles").unwrap_or(&none),
            );
            Value::Null
        }
        // The Profile and Scene Collection menus' items, which change with
        // the library: {"native": "menu_items", "menu": "Profile", "items":
        // ["-" or {title, action, id, checked, enabled}, …]}.
        "menu_items" => {
            let none = Value::Null;
            return super::set_menu_items(mtm, text("menu"), message.get("items").unwrap_or(&none));
        }
        // The Controls dock's Exit.
        "quit" => {
            objc2_app_kit::NSApplication::sharedApplication(mtm).terminate(None);
            Value::Null
        }
        "window" => json!({
            "full_screen": window.styleMask().contains(objc2_app_kit::NSWindowStyleMask::FullScreen),
        }),
        "open_file" => open_panel(mtm, message),
        "save_file" => save_panel(mtm, message),
        // A scene collection or profile file to import (Isocast's, a whole
        // setup, or OBS's), as the method that adds it: {method, params}.
        "library_import_file" => {
            let kind = serde_json::from_value::<LibraryKind>(
                message.get("kind").cloned().unwrap_or_default(),
            );
            let method = kind
                .map_err(|err| format!("kind: {err}; kinds: collection, profile"))
                .and_then(|kind| {
                    library::import_method(kind, std::path::Path::new(text("path")))
                        .map_err(|err| format!("{err:#}"))
                })
                .and_then(|method| serde_json::to_value(method).map_err(|err| err.to_string()));
            match method {
                Ok(method) => method,
                Err(message) => return json!({"error": {"code": -32000, "message": message}}),
            }
        }
        // Write a scene collection or profile (`library_get`'s document) to
        // `path`: JSON if it ends in .json, else TOML.
        "library_export_file" => {
            let written = serde_json::from_value::<LibraryDocument>(
                message.get("document").cloned().unwrap_or_default(),
            )
            .map_err(|err| format!("document: {err}"))
            .and_then(|document| {
                library::write_export(&document, std::path::Path::new(text("path")))
                    .map_err(|err| format!("{err:#}"))
            });
            if let Err(message) = written {
                return json!({"error": {"code": -32000, "message": message}});
            }
            Value::Null
        }
        "reveal" => {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&expand_home(text("path"))));
            NSWorkspace::sharedWorkspace()
                .activateFileViewerSelectingURLs(&NSArray::from_slice(&[&*url]));
            Value::Null
        }
        "open_url" => {
            if let Some(url) = NSURL::URLWithString(&NSString::from_str(text("url"))) {
                NSWorkspace::sharedWorkspace().openURL(&url);
            }
            Value::Null
        }
        "read_setup_file" => match read_setup(text("path")) {
            Ok(setup) => setup,
            Err(message) => return json!({"error": {"code": -32000, "message": message}}),
        },
        "ready" => {
            super::snapshot::ready();
            Value::Null
        }
        // Errors in the interface, for whoever runs the app from a terminal.
        "log" => {
            isocast_diagnostics::diagnostic!("Isocast interface: {}", text("message"));
            Value::Null
        }
        _ => {
            return json!({"error": {"code": -32600, "message": format!(
                "unknown native command {command:?}; commands: connection, reconnect, \
                 frames_rate, title, menu_state, menu_items, quit, window, open_file, \
                 save_file, library_import_file, library_export_file, reveal, open_url, \
                 read_setup_file, ready, log"
            )}});
        }
    };
    json!({ "result": result })
}

/// `path` with a leading `~` made this user's home folder.
fn expand_home(path: &str) -> String {
    match (path.strip_prefix('~'), std::env::var("HOME")) {
        (Some(rest), Ok(home)) if rest.is_empty() || rest.starts_with('/') => {
            format!("{home}{rest}")
        }
        _ => path.to_string(),
    }
}

/// Ask for files (or a folder) with the open panel; their paths, or an
/// empty list if cancelled.
fn open_panel(mtm: MainThreadMarker, message: &Value) -> Value {
    let panel = NSOpenPanel::openPanel(mtm);
    let folders = message
        .get("folders")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    panel.setCanChooseDirectories(folders);
    panel.setCanChooseFiles(!folders);
    panel.setAllowsMultipleSelection(
        message
            .get("multiple")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    );
    if let Some(prompt) = message.get("prompt").and_then(Value::as_str) {
        panel.setPrompt(Some(&NSString::from_str(prompt)));
    }
    if let Some(title) = message.get("message").and_then(Value::as_str) {
        panel.setMessage(Some(&NSString::from_str(title)));
    }
    if let Some(types) = message.get("types").and_then(Value::as_array) {
        let types: Vec<Retained<NSString>> = types
            .iter()
            .filter_map(Value::as_str)
            .map(NSString::from_str)
            .collect();
        if !types.is_empty() {
            let refs: Vec<&NSString> = types.iter().map(|t| &**t).collect();
            #[allow(deprecated)]
            panel.setAllowedFileTypes(Some(&NSArray::from_slice(&refs)));
        }
    }
    if panel.runModal() != NSModalResponseOK {
        return json!([]);
    }
    let paths: Vec<String> = panel
        .URLs()
        .iter()
        .filter_map(|url| url.path().map(|p| p.to_string()))
        .collect();
    json!(paths)
}

/// Ask where to save a file with the save panel (`name` suggested,
/// `types` its extensions, the first added if none is typed): its path, or
/// null if cancelled. The panel asks before replacing a file.
fn save_panel(mtm: MainThreadMarker, message: &Value) -> Value {
    let panel = NSSavePanel::savePanel(mtm);
    if let Some(name) = message.get("name").and_then(Value::as_str) {
        panel.setNameFieldStringValue(&NSString::from_str(name));
    }
    if let Some(prompt) = message.get("prompt").and_then(Value::as_str) {
        panel.setPrompt(Some(&NSString::from_str(prompt)));
    }
    if let Some(title) = message.get("message").and_then(Value::as_str) {
        panel.setMessage(Some(&NSString::from_str(title)));
    }
    if let Some(types) = message.get("types").and_then(Value::as_array) {
        let types: Vec<Retained<NSString>> = types
            .iter()
            .filter_map(Value::as_str)
            .map(NSString::from_str)
            .collect();
        if !types.is_empty() {
            let refs: Vec<&NSString> = types.iter().map(|t| &**t).collect();
            #[allow(deprecated)]
            panel.setAllowedFileTypes(Some(&NSArray::from_slice(&refs)));
        }
    }
    if panel.runModal() != NSModalResponseOK {
        return Value::Null;
    }
    panel
        .URL()
        .and_then(|url| url.path())
        .map_or(Value::Null, |path| json!(path.to_string()))
}

/// A setup file (TOML or JSON) as JSON, with relative paths made absolute
/// against its folder, as the CLI reads them.
fn read_setup(path: &str) -> Result<Value, String> {
    let path = std::path::Path::new(path);
    let text = std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let json = path.extension().is_some_and(|ext| ext == "json");
    let mut setup = if json {
        isocast_protocol::Setup::from_json(&text)
    } else {
        isocast_protocol::Setup::from_toml(&text)
    }
    .map_err(|err| format!("{}: {err}", path.display()))?;
    if let Some(folder) = path.parent() {
        setup.resolve_paths(folder);
    }
    serde_json::to_value(setup).map_err(|err| err.to_string())
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Bridge doesn't
    // implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "IsocastBridge"]
    struct Bridge;

    unsafe impl NSObjectProtocol for Bridge {}

    unsafe impl WKScriptMessageHandlerWithReply for Bridge {
        #[unsafe(method(userContentController:didReceiveScriptMessage:replyHandler:))]
        fn did_receive(
            &self,
            _controller: &WKUserContentController,
            message: &WKScriptMessage,
            reply: &DynBlock<dyn Fn(*mut AnyObject, *mut NSString)>,
        ) {
            let body = unsafe { message.body() };
            let text = body.downcast_ref::<NSString>().map(NSString::to_string);
            receive(text, reply.copy());
        }
    }
);

impl Bridge {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and SchemeHandler
    // doesn't implement Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "IsocastSchemeHandler"]
    struct SchemeHandler;

    unsafe impl NSObjectProtocol for SchemeHandler {}

    unsafe impl WKURLSchemeHandler for SchemeHandler {
        #[unsafe(method(webView:startURLSchemeTask:))]
        fn start(&self, _web: &WKWebView, task: &ProtocolObject<dyn WKURLSchemeTask>) {
            start_task(task);
        }

        #[unsafe(method(webView:stopURLSchemeTask:))]
        fn stop(&self, _web: &WKWebView, task: &ProtocolObject<dyn WKURLSchemeTask>) {
            let key = task_key(task);
            let stream = with_main(|main| {
                main.state.tasks.remove(&key);
                main.state.streams.remove(&key)
            });
            if let Some(Some((_, stream))) = stream {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }
);

impl SchemeHandler {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

fn task_key(task: &ProtocolObject<dyn WKURLSchemeTask>) -> usize {
    ptr::from_ref(task) as usize
}

fn start_task(task: &ProtocolObject<dyn WKURLSchemeTask>) {
    let request = unsafe { task.request() };
    let Some(url) = request.URL() else {
        respond(task, 400, "text/plain", b"no URL", &[]);
        return;
    };
    let path = url.path().map(|p| p.to_string()).unwrap_or_default();
    let query = url.query().map(|q| q.to_string()).unwrap_or_default();
    let param = |name: &str| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
            .and_then(|value| value.parse::<u64>().ok())
    };
    if let Some(target) = path.strip_prefix("/_frames/") {
        let width = param("w").map_or(1280, |w| w.clamp(64, 3840) as u32);
        let fps = param("fps").map_or(30, |fps| fps.clamp(1, 60) as u32);
        let id = param("id").unwrap_or_default();
        let target = target.to_string();
        let key = task_key(task);
        if with_main(|main| main.state.tasks.insert(key, task.retain())).is_none() {
            return;
        }
        thread::spawn(move || relay_frames(key, id, &target, width, fps));
        return;
    }
    if let Some(target) = path.strip_prefix("/_frame/") {
        let after = param("after");
        let width = param("w").map_or(1280, |w| w.clamp(64, 3840) as u32);
        let target = target.to_string();
        let key = task_key(task);
        if with_main(|main| main.state.tasks.insert(key, task.retain())).is_none() {
            return;
        }
        thread::spawn(move || {
            let frame = DAEMON.frame(&target, after, width);
            on_main(move || {
                let Some(task) = with_main(|main| main.state.tasks.remove(&key)).flatten() else {
                    return;
                };
                match frame {
                    Ok(None) => respond(&task, 204, "text/plain", b"", &[]),
                    Ok(Some(frame)) => {
                        let mut headers = vec![
                            ("X-Isocast-Seq", frame.seq.to_string()),
                            ("X-Isocast-Width", frame.width.to_string()),
                            ("X-Isocast-Height", frame.height.to_string()),
                        ];
                        if let Some(revision) = frame.revision {
                            headers.push(("X-Isocast-Revision", revision.to_string()));
                        }
                        respond(&task, 200, frame.mime, &frame.bytes, &headers)
                    }
                    Err(message) => respond(
                        &task,
                        503,
                        "text/plain; charset=utf-8",
                        message.as_bytes(),
                        &[],
                    ),
                }
            });
        });
        return;
    }
    if path == "/_schema.json" {
        let schema =
            serde_json::to_vec(&isocast_protocol::Setup::json_schema()).unwrap_or_default();
        respond(task, 200, "application/json", &schema, &[]);
        return;
    }
    match crate::assets::get(&path) {
        Some((bytes, mime)) => respond(task, 200, mime, &bytes, &[]),
        None => respond(
            task,
            404,
            "text/plain",
            format!("{path} is not part of the interface").as_bytes(),
            &[],
        ),
    }
}

/// Relay a monitor's stream (on its own thread): the daemon's `frames`
/// records go to the page as they arrive, until WebKit stops the task (the
/// page let go of the response) or the stream ends.
fn relay_frames(key: usize, id: u64, target: &str, width: u32, fps: u32) {
    let task = move || with_main(|main| main.state.tasks.get(&key).cloned()).flatten();
    let (reply, mut reader) = match DAEMON.frames(target, width, fps) {
        Ok(stream) => stream,
        Err(error) => {
            let (status, message) = match error {
                None => (501, "this isocastd has no frames stream".to_string()),
                Some(message) => (503, message),
            };
            on_main(move || {
                if let Some(task) = with_main(|main| main.state.tasks.remove(&key)).flatten() {
                    respond(
                        &task,
                        status,
                        "text/plain; charset=utf-8",
                        message.as_bytes(),
                        &[],
                    );
                }
            });
            return;
        }
    };
    let Ok(connection) = reader.get_ref().try_clone() else {
        return;
    };
    on_main(move || {
        let Some(task) = task() else {
            let _ = connection.shutdown(std::net::Shutdown::Both);
            return;
        };
        with_main(|main| main.state.streams.insert(key, (id, connection)));
        let headers = [
            ("X-Isocast-Host-Time", reply.host_time.to_string()),
            ("X-Isocast-Fps", reply.fps.to_string()),
        ];
        send_head(&task, 200, "application/octet-stream", None, &headers);
    });
    let mut buffer = vec![0u8; 256 << 10];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let chunk = buffer[..read].to_vec();
        on_main(move || {
            if let Some(task) = task() {
                unsafe { task.didReceiveData(&NSData::with_bytes(&chunk)) };
            }
        });
    }
    on_main(move || {
        let task = with_main(|main| {
            main.state.streams.remove(&key);
            main.state.tasks.remove(&key)
        });
        if let Some(Some(task)) = task {
            unsafe { task.didFinish() };
        }
    });
}

fn respond(
    task: &ProtocolObject<dyn WKURLSchemeTask>,
    status: isize,
    mime: &str,
    body: &[u8],
    headers: &[(&str, String)],
) {
    if send_head(task, status, mime, Some(body.len()), headers) {
        unsafe {
            task.didReceiveData(&NSData::with_bytes(body));
            task.didFinish();
        }
    }
}

/// Start a response: its status and headers (a length if known). Returns
/// whether it was sent.
fn send_head(
    task: &ProtocolObject<dyn WKURLSchemeTask>,
    status: isize,
    mime: &str,
    length: Option<usize>,
    headers: &[(&str, String)],
) -> bool {
    let request = unsafe { task.request() };
    let Some(url) = request.URL() else {
        return false;
    };
    let mut fields = vec![
        ("Content-Type", mime.to_string()),
        ("Cache-Control", "no-store".to_string()),
    ];
    if let Some(length) = length {
        fields.push(("Content-Length", length.to_string()));
    }
    fields.extend(headers.iter().cloned());
    let keys: Vec<Retained<NSString>> = fields.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let values: Vec<Retained<NSString>> =
        fields.iter().map(|(_, v)| NSString::from_str(v)).collect();
    let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let value_refs: Vec<&NSString> = values.iter().map(|v| &**v).collect();
    let fields = NSDictionary::from_slices(&key_refs, &value_refs);
    let Some(response) = NSHTTPURLResponse::initWithURL_statusCode_HTTPVersion_headerFields(
        NSHTTPURLResponse::alloc(),
        &url,
        status,
        Some(ns_string!("HTTP/1.1")),
        Some(&fields),
    ) else {
        return false;
    };
    unsafe { task.didReceiveResponse(&response) };
    true
}
