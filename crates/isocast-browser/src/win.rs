//! The helper on Windows: see main.rs.
//!
//! The page lives in a WebView2 *composition controller*: WebView2 draws
//! into a Windows.UI.Composition visual this process owns, hosted in a tool
//! window placed outside every screen (it never shows, takes focus or
//! appears in the taskbar). Windows.Graphics.Capture captures that visual
//! (premultiplied BGRA, transparent where the page is); at the source's
//! frame rate the newest picture is read back, made straight-alpha and sent
//! as a `FRAM` record. A visual only produces pictures when it changes, so
//! a page that hasn't changed for a second sends a `REDY` beat instead.
//!
//! WebView2 runs its own browser, renderer and GPU processes (children of
//! this one, in isocastd's kill-on-close job). A renderer or browser that
//! dies or stops responding ends this helper with `FAIL`, and isocastd
//! starts a new one.

use std::cell::{Cell, RefCell};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use isocast_browser_protocol as protocol;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    AddScriptToExecuteOnDocumentCreatedCompletedHandler, CoTaskMemPWSTR,
    CoreWebView2EnvironmentOptions, CreateCoreWebView2CompositionControllerCompletedHandler,
    CreateCoreWebView2EnvironmentCompletedHandler, NavigationCompletedEventHandler,
    NavigationStartingEventHandler, ProcessFailedEventHandler, WebMessageReceivedEventHandler,
};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::UI::Composition::Desktop::DesktopWindowTarget;
use windows::UI::Composition::{Compositor, ContainerVisual, Visual};
use windows::Win32::Foundation::{
    E_POINTER, HANDLE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAK, DwmSetWindowAttribute};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentThreadId, OpenProcess,
    PROCESS_QUERY_LIMITED_INFORMATION, SetWaitableTimer, TIMER_ALL_ACCESS,
};
use windows::Win32::System::WinRT::Composition::ICompositorDesktopInterop;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::{
    CreateDispatcherQueueController, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT, DispatcherQueueOptions,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetSystemMetrics, MSG, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, PostThreadMessageW, QS_ALLINPUT,
    RegisterClassW, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_SHOWNOACTIVATE, ShowWindow,
    TranslateMessage, WM_APP, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_NOREDIRECTIONBITMAP,
    WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{HSTRING, Interface, PCWSTR, PWSTR, w};
use windows_numerics::Vector2;

/// Where to get the runtime when it's missing.
const RUNTIME_LINK: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";
/// The script that routes a page's sound to `isocastAudio` (shared with
/// the macOS and Linux browser sources).
const AUDIO_SCRIPT: &str = include_str!("../../isocast-platform-macos/src/browser/audio.js");
/// Pictures waiting for isocastd; more are dropped, so memory stays bounded.
const QUEUE: usize = 2;
/// How long WebView2 may take to start its browser and the page's view.
const START: Duration = Duration::from_secs(30);
/// While a page loads or shows a still picture (no new pictures), how often
/// isocastd hears it's alive, so it isn't taken for a hung one.
const BEAT: Duration = Duration::from_secs(1);
/// A page still loading after this fails, and isocastd restarts it.
const LOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// Chromium switches: pages play sound and run timers at full speed without
/// a click or a visible window (the window is outside every screen, which
/// Chromium would otherwise treat as hidden and throttle).
const BROWSER_ARGUMENTS: &str = "--autoplay-policy=no-user-gesture-required \
     --disable-features=CalculateNativeWinOcclusion \
     --disable-background-timer-throttling --disable-renderer-backgrounding \
     --disable-backgrounding-occluded-windows";
/// audio.js posts to WebKit's message handler: on WebView2, that is the
/// page's `chrome.webview.postMessage` (top frame only, as on macOS).
const AUDIO_SHIM: &str = "if (window === window.top && window.chrome && window.chrome.webview) { \
     window.webkit = window.webkit || {}; \
     window.webkit.messageHandlers = { isocastAudio: { postMessage: (m) => window.chrome.webview.postMessage(m) } }; }\n";

struct Args {
    url: String,
    width: u32,
    height: u32,
    fps: u32,
    css: String,
    audio: bool,
}

/// The page from the first line on stdin, the rest from the arguments.
fn parse_args() -> Result<Args, String> {
    let mut first = String::new();
    std::io::stdin()
        .read_line(&mut first)
        .map_err(|e| format!("reading the page from stdin: {e}"))?;
    let (location, css) = protocol::parse_page_line(&first)?;
    let url = match location {
        protocol::Location::Url(url) => url,
        protocol::Location::File(path) => file_url(Path::new(
            &String::from_utf8(path).map_err(|_| "the page's path isn't UTF-8")?,
        )),
    };
    let mut args = std::env::args().skip(1);
    let (mut width, mut height, mut fps, mut audio) = (0, 0, 0, false);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--width" => width = value("--width")?.parse().map_err(|_| "--width: a number")?,
            "--height" => {
                height = value("--height")?
                    .parse()
                    .map_err(|_| "--height: a number")?
            }
            "--fps" => fps = value("--fps")?.parse().map_err(|_| "--fps: a number")?,
            "--audio" => audio = true,
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) || !(1..=240).contains(&fps) {
        return Err(format!("bad size or rate: {width}x{height} at {fps} fps"));
    }
    Ok(Args {
        url,
        width,
        height,
        fps,
        css,
        audio,
    })
}

/// Writes records to stdout from its own thread; the message loop never
/// blocks on isocastd.
struct Out {
    send: mpsc::SyncSender<Vec<u8>>,
    /// Picture buffers back from the writer, reused (no 4-byte-per-pixel
    /// allocation per frame).
    spare: mpsc::Receiver<Vec<u8>>,
}

impl Out {
    fn start() -> Self {
        let (send, receive) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
        let (give_back, spare) = mpsc::sync_channel::<Vec<u8>>(QUEUE + 1);
        std::thread::Builder::new()
            .name("isocast-browser-out".into())
            .spawn(move || {
                let mut stdout = std::io::stdout().lock();
                for record in receive {
                    if stdout
                        .write_all(&record)
                        .and_then(|_| stdout.flush())
                        .is_err()
                    {
                        // isocastd is gone.
                        std::process::exit(0);
                    }
                    if record.starts_with(protocol::FRAME) {
                        let _ = give_back.try_send(record);
                    }
                }
            })
            .expect("the output thread starts");
        Self { send, spare }
    }

    /// A buffer for a picture record, reused when one is free.
    fn buffer(&self) -> Vec<u8> {
        self.spare.try_recv().unwrap_or_default()
    }

    /// Queue a record; dropped if the queue is full (isocastd is behind).
    /// Returns whether it was queued.
    fn send(&self, record: Vec<u8>) -> bool {
        self.send.try_send(record).is_ok()
    }

    /// Queue a record, waiting for room (rare, small records).
    fn send_wait(&self, record: Vec<u8>) {
        let _ = self.send.send(record);
    }

    /// A sender of records waiting for room, for the page's handlers.
    fn waiting_sender(&self) -> impl Fn(Vec<u8>) + use<> {
        let send = self.send.clone();
        move |record| {
            let _ = send.send(record);
        }
    }

    fn fail(&self, reason: &str) {
        self.send_wait(protocol::fail(reason));
    }
}

/// `rate:base64` from audio.js: the rate and the samples' raw bytes
/// (interleaved stereo f32, little-endian). The page can post anything
/// there, so a rate isocastd would refuse or more than a second of sound
/// in one message is dropped, before decoding.
fn decode_audio(message: &str) -> Option<(u32, Vec<u8>)> {
    let (rate, data) = message.split_once(':')?;
    let rate: u32 = rate.parse().ok()?;
    let second = rate as usize * 8;
    if !protocol::AUDIO_RATES.contains(&rate) || data.len() > second.div_ceil(3) * 4 {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    (bytes.len().is_multiple_of(8) && bytes.len() <= second).then_some((rate, bytes))
}

pub fn main() -> i32 {
    let out = Out::start();
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            out.fail(&err);
            std::thread::sleep(Duration::from_millis(100));
            return 2;
        }
    };
    match run(&args, &out) {
        Ok(code) => code,
        Err(err) => {
            out.fail(&err);
            std::thread::sleep(Duration::from_millis(200));
            3
        }
    }
}

/// `file:///C:/a%20b/page.html` for a local file.
fn file_url(path: &Path) -> String {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let text = path.to_string_lossy().replace('\\', "/");
    // A UNC path (//server/share) keeps its host; a drive path gets a slash.
    let (prefix, rest) = match text.strip_prefix("//") {
        Some(rest) => ("file://", rest.to_string()),
        None => ("file:///", text),
    };
    let mut url = String::from(prefix);
    for byte in rest.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b':' | b'-' | b'_' | b'.' | b'~' => {
                url.push(byte as char)
            }
            _ => url.push_str(&format!("%{byte:02X}")),
        }
    }
    url
}

/// A JavaScript string literal of `text`.
fn js_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            '<' => out.push_str("\\u003c"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A script that adds `css` as the page's last style sheet.
fn css_script(css: &str) -> String {
    format!(
        "(() => {{ const add = () => {{ const s = document.createElement('style'); \
         s.textContent = {}; (document.head || document.documentElement).appendChild(s); }}; \
         if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', add); \
         else add(); }})();",
        js_string(css)
    )
}

fn hr(context: &str) -> impl Fn(windows::core::Error) -> String + '_ {
    move |err| format!("{context}: {err}")
}

fn wv(context: &str) -> impl Fn(webview2_com::Error) -> String + '_ {
    move |err| format!("{context}: {err:?}")
}

/// The installed WebView2 Runtime's version, or why there is none.
fn runtime_version() -> Result<String, String> {
    let mut version = PWSTR::null();
    let result =
        unsafe { GetAvailableCoreWebView2BrowserVersionString(PCWSTR::null(), &mut version) };
    let version = CoTaskMemPWSTR::from(version).to_string();
    match result {
        Ok(()) if !version.is_empty() => Ok(version),
        _ => Err(format!(
            "browser sources need the Microsoft Edge WebView2 Runtime, which isn't installed: \
             get it from {RUNTIME_LINK}"
        )),
    }
}

/// This helper's WebView2 profile: a private folder per process, so each
/// page has its own browser processes (restarted with it, and counted in
/// its job). Folders of helpers that have ended are removed.
fn user_data_folder() -> Result<PathBuf, String> {
    let root = std::env::temp_dir().join("isocast-browser");
    std::fs::create_dir_all(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let alive = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
                .map(|handle| unsafe {
                    let _ = windows::Win32::Foundation::CloseHandle(handle);
                })
                .is_ok();
            if !alive {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
    let folder = root.join(std::process::id().to_string());
    std::fs::create_dir_all(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    Ok(folder)
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

/// The window hosting the page's visual: a tool window (no taskbar button,
/// never activated) outside every screen, unless `ISOCAST_BROWSER_WINDOW`
/// says `screen` (top left of the primary screen, to look at it) or
/// `cloaked` (on screen but cloaked by DWM).
fn host_window(width: u32, height: u32) -> Result<HWND, String> {
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }
        .map_err(hr("GetModuleHandleW"))?
        .into();
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: w!("IsocastBrowserPage"),
        ..Default::default()
    };
    unsafe { RegisterClassW(&class) };
    let mode = std::env::var("ISOCAST_BROWSER_WINDOW").unwrap_or_default();
    let (x, y) = match mode.as_str() {
        "screen" | "cloaked" => (0, 0),
        _ => unsafe {
            // Left of the leftmost screen, with a margin.
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN) - width as i32 - 1000,
                GetSystemMetrics(SM_YVIRTUALSCREEN),
            )
        },
    };
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP,
            w!("IsocastBrowserPage"),
            w!("Isocast browser source"),
            WS_POPUP,
            x,
            y,
            width as i32,
            height as i32,
            None,
            None,
            Some(instance),
            None,
        )
    }
    .map_err(hr("creating the page's window"))?;
    if mode == "cloaked" {
        let cloak: i32 = 1;
        unsafe {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_CLOAK,
                (&cloak as *const i32).cast(),
                std::mem::size_of::<i32>() as u32,
            );
        }
    }
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    Ok(hwnd)
}

/// A D3D11 device for reading pictures back: the GPU, else WARP.
fn d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let mut last = String::new();
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        match create_device(driver) {
            Ok(device) => return Ok(device),
            Err(err) => last = err.to_string(),
        }
    }
    Err(format!(
        "no Direct3D 11 device for the page's pictures: {last}"
    ))
}

fn create_device(
    driver: D3D_DRIVER_TYPE,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            None,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let device: ID3D11Device = device.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
    let context: ID3D11DeviceContext =
        context.ok_or_else(|| windows::core::Error::from(E_POINTER))?;
    // The capture's free-threaded pool uses the device from its own thread.
    let multithread: ID3D11Multithread = context.cast()?;
    unsafe {
        let _ = multithread.SetMultithreadProtected(true);
    }
    Ok((device, context))
}

/// What the page's events tell the main loop.
struct PageState {
    /// A navigation is under way (nothing is sent until it completes).
    loading: Cell<bool>,
    /// When the current navigation started.
    load_started: Cell<Instant>,
    /// The page's processes failed: the reason, and the helper ends.
    failed: RefCell<Option<String>>,
}

/// The capture of the page's visual and its readback.
struct Capture {
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
}

impl Capture {
    fn start(visual: &Visual, width: u32, height: u32) -> Result<Self, String> {
        if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
            return Err("Windows.Graphics.Capture is unavailable on this desktop \
                 (browser sources need Windows 10 version 1903 or newer and a signed-in desktop)"
                .into());
        }
        let (device, context) = d3d_device()?;
        let dxgi: IDXGIDevice = device.cast().map_err(hr("IDXGIDevice"))?;
        let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|d| d.cast())
            .map_err(hr("CreateDirect3D11DeviceFromDXGIDevice"))?;
        let item = GraphicsCaptureItem::CreateFromVisual(visual)
            .map_err(hr("capturing the page's visual"))?;
        let size = SizeInt32 {
            Width: width as i32,
            Height: height as i32,
        };
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            3,
            size,
        )
        .map_err(hr("Direct3D11CaptureFramePool"))?;
        let session = pool
            .CreateCaptureSession(&item)
            .map_err(hr("CreateCaptureSession"))?;
        // Windows 10 2004+ and 11 only; older ones never draw either.
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }
            .map_err(hr("the readback texture"))?;
        let staging = staging.ok_or("D3D11 returned no readback texture")?;
        session.StartCapture().map_err(hr("StartCapture"))?;
        Ok(Self {
            _item: item,
            pool,
            session,
            context,
            staging,
            width,
            height,
        })
    }

    /// The newest picture since the last call, as a `FRAM` record in
    /// `record`; false if the visual hasn't changed.
    fn read(&self, record: &mut Vec<u8>) -> Result<bool, String> {
        let mut newest = None;
        loop {
            match self.pool.TryGetNextFrame() {
                Ok(frame) => {
                    if let Some(older) = newest.replace(frame) {
                        let _ = older.Close();
                    }
                }
                // An empty pool is S_OK with no frame.
                Err(err) if err.code().is_ok() => break,
                Err(err) => return Err(format!("reading the page's picture: {err}")),
            }
        }
        let Some(frame) = newest else {
            return Ok(false);
        };
        let copied = (|| -> windows::core::Result<(u32, u32)> {
            let content = frame.ContentSize()?;
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            let source: ID3D11Texture2D = unsafe { access.GetInterface() }?;
            let (cw, ch) = (
                (content.Width.max(0) as u32).min(self.width),
                (content.Height.max(0) as u32).min(self.height),
            );
            unsafe {
                self.context.CopySubresourceRegion(
                    &self.staging,
                    0,
                    0,
                    0,
                    0,
                    &source,
                    0,
                    Some(&D3D11_BOX {
                        left: 0,
                        top: 0,
                        front: 0,
                        right: cw,
                        bottom: ch,
                        back: 1,
                    }),
                );
            }
            Ok((cw, ch))
        })();
        let _ = frame.Close();
        let (cw, ch) = copied.map_err(|e| format!("copying the page's picture: {e}"))?;
        let pixels = protocol::frame(record, self.width, self.height);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(hr("reading the page's picture back"))?;
            let row = self.width as usize * 4;
            let copy = cw as usize * 4;
            for y in 0..ch as usize {
                let source = std::slice::from_raw_parts(
                    (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                    copy,
                );
                pixels[y * row..y * row + copy].copy_from_slice(source);
            }
            self.context.Unmap(&self.staging, 0);
        }
        // DWM composes premultiplied alpha.
        protocol::unpremultiply(pixels);
        Ok(true)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// A drift-free schedule at the page's frame rate.
struct Pacer {
    start: Instant,
    fps: u32,
    tick: u64,
}

impl Pacer {
    fn due(&self) -> Instant {
        self.start + Duration::from_nanos((self.tick + 1) * 1_000_000_000 / u64::from(self.fps))
    }
}

fn run(args: &Args, out: &Out) -> Result<i32, String> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(hr("CoInitializeEx"))?;
    }
    let version = runtime_version()?;
    eprintln!("WebView2 Runtime {version}");
    // Windows.UI.Composition needs a dispatcher queue on this thread.
    let _queue = unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_STA,
        })
    }
    .map_err(hr("CreateDispatcherQueueController"))?;
    let (width, height) = (args.width, args.height);
    let hwnd = host_window(width, height)?;

    let compositor = Compositor::new().map_err(hr("Compositor"))?;
    let interop: ICompositorDesktopInterop =
        compositor.cast().map_err(hr("ICompositorDesktopInterop"))?;
    let target: DesktopWindowTarget = unsafe { interop.CreateDesktopWindowTarget(hwnd, false) }
        .map_err(hr("CreateDesktopWindowTarget"))?;
    let root: ContainerVisual = compositor
        .CreateContainerVisual()
        .map_err(hr("CreateContainerVisual"))?;
    root.SetSize(Vector2 {
        X: width as f32,
        Y: height as f32,
    })
    .map_err(hr("sizing the page's visual"))?;
    target.SetRoot(&root).map_err(hr("SetRoot"))?;

    // The page's own background is transparent until it paints one.
    unsafe { std::env::set_var("WEBVIEW2_DEFAULT_BACKGROUND_COLOR", "0") };
    let folder = user_data_folder()?;
    let options = CoreWebView2EnvironmentOptions::default();
    unsafe { options.set_additional_browser_arguments(BROWSER_ARGUMENTS.to_string()) };
    let options: ICoreWebView2EnvironmentOptions = options.into();
    let started = Instant::now();
    let environment = {
        let (send, receive) = mpsc::channel();
        let folder = HSTRING::from(folder.as_os_str());
        CreateCoreWebView2EnvironmentCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                CreateCoreWebView2EnvironmentWithOptions(
                    PCWSTR::null(),
                    &folder,
                    &options,
                    &handler,
                )
                .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |result, environment| {
                result?;
                let _ = send.send(environment.ok_or_else(|| windows::core::Error::from(E_POINTER)));
                Ok(())
            }),
        )
        .map_err(wv("starting WebView2"))?;
        receive
            .recv()
            .map_err(|_| "WebView2 didn't start".to_string())?
            .map_err(hr("starting WebView2"))?
    };
    let environment: ICoreWebView2Environment3 = environment.cast().map_err(hr(
        "this WebView2 Runtime is too old for browser sources (no composition hosting)",
    ))?;
    let composition = {
        let (send, receive) = mpsc::channel();
        CreateCoreWebView2CompositionControllerCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                environment
                    .CreateCoreWebView2CompositionController(hwnd, &handler)
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(move |result, controller| {
                result?;
                let _ = send.send(controller.ok_or_else(|| windows::core::Error::from(E_POINTER)));
                Ok(())
            }),
        )
        .map_err(wv("creating the page's view"))?;
        receive
            .recv()
            .map_err(|_| "WebView2 didn't create the page's view".to_string())?
            .map_err(hr("creating the page's view"))?
    };
    if started.elapsed() > START {
        eprintln!(
            "WebView2 took {:.1} s to start",
            started.elapsed().as_secs_f64()
        );
    }
    let controller: ICoreWebView2Controller =
        composition.cast().map_err(hr("ICoreWebView2Controller"))?;
    unsafe {
        let visual: windows::core::IUnknown = root.cast().map_err(hr("the page's visual"))?;
        composition
            .SetRootVisualTarget(&visual)
            .map_err(hr("SetRootVisualTarget"))?;
        if let Ok(controller3) = controller.cast::<ICoreWebView2Controller3>() {
            // The page is laid out at exactly the source's size in pixels,
            // whatever the screens' scaling.
            let _ = controller3.SetBoundsMode(COREWEBVIEW2_BOUNDS_MODE_USE_RAW_PIXELS);
            let _ = controller3.SetShouldDetectMonitorScaleChanges(false);
            let _ = controller3.SetRasterizationScale(1.0);
        }
        controller
            .SetBounds(RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            })
            .map_err(hr("SetBounds"))?;
        if let Ok(controller2) = controller.cast::<ICoreWebView2Controller2>() {
            controller2
                .SetDefaultBackgroundColor(COREWEBVIEW2_COLOR {
                    A: 0,
                    R: 0,
                    G: 0,
                    B: 0,
                })
                .map_err(hr("a transparent background"))?;
        }
        controller.SetIsVisible(true).map_err(hr("SetIsVisible"))?;
    }
    let webview = unsafe { controller.CoreWebView2() }.map_err(hr("CoreWebView2"))?;
    unsafe {
        if let Ok(settings) = webview.Settings() {
            let _ = settings.SetAreDefaultContextMenusEnabled(false);
            let _ = settings.SetIsStatusBarEnabled(false);
            let _ = settings.SetIsZoomControlEnabled(false);
            let _ = settings.SetAreDevToolsEnabled(false);
        }
    }

    let state = Rc::new(PageState {
        loading: Cell::new(false),
        load_started: Cell::new(Instant::now()),
        failed: RefCell::new(None),
    });
    let mut scripts = Vec::new();
    if !args.css.trim().is_empty() {
        scripts.push(css_script(&args.css));
    }
    if args.audio {
        scripts.push(format!("{AUDIO_SHIM}{AUDIO_SCRIPT}"));
    }
    for script in scripts {
        let script = HSTRING::from(script);
        let webview = webview.clone();
        AddScriptToExecuteOnDocumentCreatedCompletedHandler::wait_for_async_operation(
            Box::new(move |handler| unsafe {
                webview
                    .AddScriptToExecuteOnDocumentCreated(&script, &handler)
                    .map_err(webview2_com::Error::WindowsError)
            }),
            Box::new(|result, _id| result),
        )
        .map_err(wv("adding the page's scripts"))?;
    }
    let mut token = 0i64;
    unsafe {
        if args.audio {
            // Each message is one chunk of the page's sound.
            let send = out.waiting_sender();
            webview
                .add_WebMessageReceived(
                    &WebMessageReceivedEventHandler::create(Box::new(move |_, message| {
                        let Some(message) = message else {
                            return Ok(());
                        };
                        let mut text = PWSTR::null();
                        if message.TryGetWebMessageAsString(&mut text).is_ok() {
                            let text = CoTaskMemPWSTR::from(text).to_string();
                            if let Some((rate, samples)) = decode_audio(&text) {
                                send(protocol::audio(rate, &samples));
                            }
                        }
                        Ok(())
                    })),
                    &mut token,
                )
                .map_err(hr("listening to the page's sound"))?;
        }
        let starting = state.clone();
        webview
            .add_NavigationStarting(
                &NavigationStartingEventHandler::create(Box::new(move |_, _| {
                    if !starting.loading.replace(true) {
                        starting.load_started.set(Instant::now());
                    }
                    Ok(())
                })),
                &mut token,
            )
            .map_err(hr("NavigationStarting"))?;
        let completed = state.clone();
        webview
            .add_NavigationCompleted(
                &NavigationCompletedEventHandler::create(Box::new(move |_, _| {
                    completed.loading.set(false);
                    Ok(())
                })),
                &mut token,
            )
            .map_err(hr("NavigationCompleted"))?;
        let failed = state.clone();
        webview
            .add_ProcessFailed(
                &ProcessFailedEventHandler::create(Box::new(move |_, event| {
                    let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND_UNKNOWN_PROCESS_EXITED;
                    if let Some(event) = event {
                        let _ = event.ProcessFailedKind(&mut kind);
                    }
                    let why = match kind {
                        COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED => {
                            Some("the page's browser process exited")
                        }
                        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED => {
                            Some("the page's renderer exited")
                        }
                        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE => {
                            Some("the page stopped responding")
                        }
                        // Frames, GPU and utility processes are restarted by
                        // WebView2 itself.
                        _ => None,
                    };
                    if let Some(why) = why {
                        failed
                            .failed
                            .borrow_mut()
                            .get_or_insert_with(|| why.to_string());
                    }
                    Ok(())
                })),
                &mut token,
            )
            .map_err(hr("ProcessFailed"))?;
    }

    let visual: Visual = root.cast().map_err(hr("the page's visual"))?;
    let capture = Capture::start(&visual, width, height)?;

    let url = HSTRING::from(args.url.as_str());
    state.loading.set(true);
    unsafe { webview.Navigate(&url) }.map_err(hr("opening the page"))?;
    out.send_wait(protocol::record(protocol::READY, 0));

    // Commands from isocastd; the reader wakes this thread's message loop.
    let (send, commands) = mpsc::channel::<String>();
    let thread = unsafe { GetCurrentThreadId() };
    std::thread::Builder::new()
        .name("isocast-browser-in".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if send.send(line).is_err() {
                    return;
                }
                unsafe {
                    let _ = PostThreadMessageW(thread, WM_APP, WPARAM(0), LPARAM(0));
                }
            }
            let _ = send.send("quit".into());
            unsafe {
                let _ = PostThreadMessageW(thread, WM_APP, WPARAM(0), LPARAM(0));
            }
        })
        .map_err(|e| format!("starting the input thread: {e}"))?;

    // A high-resolution timer paces the ticks (plain waits round to the
    // 15.6 ms system tick).
    let timer = unsafe {
        CreateWaitableTimerExW(
            None,
            PCWSTR::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS.0,
        )
    }
    .ok();
    let mut pacer = Pacer {
        start: Instant::now(),
        fps: args.fps,
        tick: 0,
    };
    let mut loaded = true;
    let mut cleared = false;
    let mut warned = false;
    let beat = protocol::record(protocol::READY, 0);
    let mut last_sent = Instant::now();
    // Counted for the log, every 10 s: pictures sent, pictures isocastd was
    // too busy to take, and ticks where the page hadn't changed.
    let (mut pictures, mut behind, mut unchanged) = (0u64, 0u64, 0u64);
    let mut reported = Instant::now();
    loop {
        // Wait for the next tick, handling messages (WebView2's events and
        // the input thread's wake-ups) meanwhile.
        loop {
            let now = Instant::now();
            let due = pacer.due();
            if now >= due {
                break;
            }
            let wait = due - now;
            let handles: Vec<HANDLE> = match &timer {
                Some(timer) => {
                    let relative = -((wait.as_nanos() / 100) as i64).max(1);
                    unsafe {
                        let _ = SetWaitableTimer(*timer, &relative, 0, None, None, false);
                    }
                    vec![*timer]
                }
                None => Vec::new(),
            };
            let millis = (wait.as_millis() as u32).saturating_add(1);
            unsafe {
                MsgWaitForMultipleObjectsEx(
                    Some(&handles),
                    millis,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
            }
            pump();
            while let Ok(command) = commands.try_recv() {
                match command.trim() {
                    "reload" if loaded => unsafe {
                        let _ = webview.Reload();
                    },
                    "unload" if loaded => {
                        loaded = false;
                        cleared = false;
                        unsafe {
                            let _ = webview.Navigate(w!("about:blank"));
                        }
                    }
                    "load" if !loaded => {
                        loaded = true;
                        state.loading.set(true);
                        state.load_started.set(Instant::now());
                        unsafe {
                            let _ = webview.Navigate(&url);
                        }
                    }
                    "quit" => {
                        unsafe {
                            let _ = controller.Close();
                        }
                        return Ok(0);
                    }
                    _ => {}
                }
            }
            if let Some(why) = state.failed.borrow().clone() {
                out.fail(&why);
                std::thread::sleep(Duration::from_millis(200));
                return Ok(4);
            }
        }
        pacer.tick += 1;
        // Behind by more than a tick (a slow readback, a busy machine):
        // skip ahead rather than send a burst.
        let now = Instant::now();
        if now > pacer.due() {
            let ticks = (now - pacer.start).as_nanos() * u128::from(pacer.fps) / 1_000_000_000;
            pacer.tick = ticks as u64;
        }
        if !loaded {
            if !cleared {
                // Waiting for room: a dropped one would leave the last page
                // picture showing while the page is unloaded.
                cleared = true;
                let mut record = out.buffer();
                protocol::frame(&mut record, width, height);
                out.send_wait(record);
            }
            continue;
        }
        // Nothing until the page has loaded (as on macOS): the first
        // picture a source shows is the page, not an empty view. A page
        // that never finishes loading fails.
        if state.loading.get() && state.load_started.get().elapsed() > LOAD_TIMEOUT {
            out.fail(&format!(
                "the page didn't finish loading within {} s",
                LOAD_TIMEOUT.as_secs()
            ));
            std::thread::sleep(Duration::from_millis(200));
            return Ok(5);
        }
        if reported.elapsed() >= Duration::from_secs(10) {
            reported = Instant::now();
            eprintln!(
                "last 10 s: {pictures} pictures, {behind} dropped (isocastd behind), {unchanged} ticks unchanged"
            );
            (pictures, behind, unchanged) = (0, 0, 0);
        }
        let mut record = out.buffer();
        let sent = match capture.read(&mut record) {
            Ok(true) if !state.loading.get() => {
                if out.send(record) {
                    pictures += 1;
                } else {
                    behind += 1;
                }
                true
            }
            Ok(false) => {
                unchanged += 1;
                false
            }
            Ok(true) => false,
            Err(err) => {
                if !warned {
                    warned = true;
                    eprintln!("{err}");
                }
                false
            }
        };
        // A still page (the capture only makes pictures when it changes) or
        // a loading one: isocastd hears it lives.
        if sent {
            last_sent = Instant::now();
        } else if last_sent.elapsed() >= BEAT {
            last_sent = Instant::now();
            out.send(beat.clone());
        }
    }
}

/// Handle every waiting message.
fn pump() {
    let mut message = MSG::default();
    unsafe {
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_files_become_file_urls() {
        assert_eq!(
            file_url(Path::new(r"C:\Users\Ryan Vogel\page #1.html")),
            "file:///C:/Users/Ryan%20Vogel/page%20%231.html"
        );
        assert_eq!(
            file_url(Path::new(r"\\server\share\a.html")),
            "file://server/share/a.html"
        );
    }

    #[test]
    fn page_audio_messages_are_bounded() {
        let samples: Vec<u8> = [0.5f32, -0.5]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&samples);
        assert_eq!(
            decode_audio(&format!("44100:{encoded}")),
            Some((44100, samples))
        );
        // A rate isocastd refuses, garbage, half frames.
        assert_eq!(decode_audio(&format!("1:{encoded}")), None);
        assert_eq!(decode_audio("48000:@@"), None);
        let odd = base64::engine::general_purpose::STANDARD.encode([1, 2, 3]);
        assert_eq!(decode_audio(&format!("48000:{odd}")), None);
    }

    #[test]
    fn css_is_a_safe_script_string() {
        let script = css_script("body { content: \"</style>\\\n\" }");
        assert!(
            script.contains(r#""body { content: \"\u003c/style>\\\n\" }""#),
            "{script}"
        );
    }
}
