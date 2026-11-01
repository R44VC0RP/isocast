//! The keyboard while an Isocast window is focused (docs/hotkeys.md, "API
//! and client integration"): every key press and release that could be a
//! hotkey chord goes to the daemon with `hotkeys_input` on one persistent
//! connection, from a thread of its own (the window never waits for it);
//! when the app stops being active, every key still held is released.
//! Chords are physical US key positions, as the daemon's are, so they are
//! read from AppKit's key codes (an NSEvent local monitor), not from the
//! characters Slint is given. Settings → Hotkeys borrows the next press
//! to record a shortcut (`capture`); that press and its release go
//! nowhere else.

#[cfg(target_os = "macos")]
use std::cell::RefCell;
#[cfg(target_os = "macos")]
use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::sync::mpsc::{Receiver, Sender, channel};

#[cfg(target_os = "macos")]
use isocast_client::{Client, DaemonError};
#[cfg(target_os = "macos")]
use isocast_protocol::hotkeys::{Chord, Phase};

/// Gets the next chord typed (`None`: Escape, cancelled).
pub type Capture = Box<dyn FnOnce(Option<String>)>;

#[cfg(target_os = "macos")]
struct State {
    tx: Sender<(String, Phase)>,
    /// Keys held (by key code), with the chord their press sent: a
    /// release names the same chord, whatever the modifiers do meanwhile.
    pressed: HashMap<i64, String>,
    capture: Option<Capture>,
    /// Keys whose press a capture took: their release is taken too.
    taken: Vec<i64>,
}

#[cfg(target_os = "macos")]
thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Start forwarding (once, from the window's thread).
#[cfg(target_os = "macos")]
pub fn start() {
    if STATE.with(|s| s.borrow().is_some()) {
        return;
    }
    let (tx, rx) = channel();
    std::thread::Builder::new()
        .name("isocastd hotkey input".into())
        .spawn(move || forward(rx))
        .expect("spawn a thread");
    STATE.with(|s| {
        *s.borrow_mut() = Some(State {
            tx,
            pressed: HashMap::new(),
            capture: None,
            taken: Vec::new(),
        })
    });
    install();
}

/// Give the next key press to `capture` instead of the daemon (or stop
/// waiting for one, with `None`).
#[cfg(target_os = "macos")]
pub fn capture(capture: Option<Capture>) {
    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            state.capture = capture;
        }
    });
}

/// The daemon's end: one connection for every edge, so a held push-to-talk
/// key is released if this app goes away (the daemon releases a closed
/// connection's holds). It connects to a running daemon only.
#[cfg(target_os = "macos")]
fn forward(rx: Receiver<(String, Phase)>) {
    let mut client: Option<Client> = None;
    while let Ok((keys, phase)) = rx.recv() {
        if client.is_none() {
            client = Client::connect().ok().flatten();
        }
        let Some(c) = client.as_mut() else {
            debug(format_args!("hotkeys_input {keys} {phase:?}: no daemon"));
            continue;
        };
        let result = c.hotkey_input(keys.clone(), phase);
        debug(format_args!("hotkeys_input {keys} {phase:?}: {result:?}"));
        if let Err(error) = result
            && error.downcast_ref::<DaemonError>().is_none()
        {
            // Lost it: the next edge connects again.
            client = None;
        }
    }
}

/// A key went down or up (`code` a key code; media keys are 1000 + their
/// NX key type), with these modifiers (ctrl 1, alt 2, shift 4, cmd 8).
/// Whether the event goes on to the window.
#[cfg(target_os = "macos")]
fn key(code: i64, name: &str, modifiers: u32, down: bool, repeat: bool) -> bool {
    debug(format_args!(
        "key {name} ({code}) modifiers {modifiers} {} {}",
        if down { "down" } else { "up" },
        if repeat { "repeat" } else { "" }
    ));
    let mut give: Option<(Capture, Option<String>)> = None;
    let keep = STATE.with(|s| {
        let mut s = s.borrow_mut();
        let Some(state) = s.as_mut() else { return true };
        if down {
            if let Some(capture) = state.capture.take() {
                state.taken.push(code);
                if !repeat {
                    let chord = (!(name == "escape" && modifiers == 0))
                        .then(|| chord_text(modifiers, name));
                    give = Some((capture, chord));
                } else {
                    state.capture = Some(capture);
                }
                return false;
            }
            if repeat {
                return true;
            }
            // AppKit sends no key-up for a key pressed with Command: a new
            // press of a key still marked held releases it first.
            if let Some(old) = state.pressed.remove(&code) {
                let _ = state.tx.send((old, Phase::Release));
            }
            let chord = chord_text(modifiers, name);
            // Only what could be a binding goes (typing doesn't).
            if Chord::parse(&chord).is_ok() {
                let _ = state.tx.send((chord.clone(), Phase::Press));
                state.pressed.insert(code, chord);
            }
            true
        } else {
            if let Some(at) = state.taken.iter().position(|c| *c == code) {
                state.taken.remove(at);
                return false;
            }
            if let Some(chord) = state.pressed.remove(&code) {
                let _ = state.tx.send((chord, Phase::Release));
            }
            true
        }
    });
    if let Some((capture, chord)) = give {
        capture(chord);
    }
    keep
}

/// The modifiers changed (`modifiers` now held): a held chord that needs
/// one that was let go is released (AppKit sends no key-up for a key
/// pressed with Command, so its release shows here).
#[cfg(target_os = "macos")]
fn modifiers_changed(modifiers: u32) {
    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            let gone: Vec<i64> = state
                .pressed
                .iter()
                .filter(|(_, chord)| {
                    let needs = chord_modifiers(chord);
                    needs & modifiers != needs
                })
                .map(|(code, _)| *code)
                .collect();
            for code in gone {
                if let Some(chord) = state.pressed.remove(&code) {
                    let _ = state.tx.send((chord, Phase::Release));
                }
            }
        }
    });
}

/// A chord's modifiers (ctrl 1, alt 2, shift 4, cmd 8).
#[cfg(target_os = "macos")]
fn chord_modifiers(chord: &str) -> u32 {
    let parts: Vec<&str> = chord.split('+').collect();
    let mut bits = 0;
    for part in &parts[..parts.len().saturating_sub(1)] {
        bits |= match *part {
            "ctrl" => 1,
            "alt" => 2,
            "shift" => 4,
            "cmd" => 8,
            _ => 0,
        };
    }
    bits
}

/// With ISOCAST_DEBUG_KEYS set, each key edge and what the daemon said
/// goes to stderr (for a bug report about hotkeys).
#[cfg(target_os = "macos")]
fn debug(what: std::fmt::Arguments) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("ISOCAST_DEBUG_KEYS").is_some()) {
        eprintln!("isocast-ui keys: {what}");
    }
}

/// The app (or its window) is no longer the active one: release every
/// key still held.
#[cfg(target_os = "macos")]
pub fn release_all() {
    STATE.with(|s| {
        if let Some(state) = s.borrow_mut().as_mut() {
            for (_, chord) in state.pressed.drain() {
                let _ = state.tx.send((chord, Phase::Release));
            }
            state.taken.clear();
        }
    });
}

/// "ctrl+alt+shift+cmd+key", the daemon's canonical order.
#[cfg(target_os = "macos")]
pub fn chord_text(modifiers: u32, name: &str) -> String {
    let mut parts = Vec::new();
    for (bit, part) in [(1, "ctrl"), (2, "alt"), (4, "shift"), (8, "cmd")] {
        if modifiers & bit != 0 {
            parts.push(part);
        }
    }
    parts.push(name);
    parts.join("+")
}

/// A chord as this OS writes shortcuts, one cap each: ⌃⌥⇧⌘ and the key
/// on macOS ("ctrl+alt+cmd+r" → ⌃ ⌥ ⌘ R), names elsewhere (Ctrl Alt Win R,
/// Super on Linux).
pub fn chord_caps(chord: &str) -> Vec<String> {
    let mut caps = Vec::new();
    let parts: Vec<&str> = chord.split('+').collect();
    let Some((key, modifiers)) = parts.split_last() else {
        return caps;
    };
    let modifiers_shown = if cfg!(target_os = "macos") {
        [("ctrl", "⌃"), ("alt", "⌥"), ("shift", "⇧"), ("cmd", "⌘")]
    } else {
        [
            ("ctrl", "Ctrl"),
            ("alt", "Alt"),
            ("shift", "Shift"),
            ("cmd", SUPER),
        ]
    };
    for (name, glyph) in modifiers_shown {
        if modifiers.iter().any(|m| m.eq_ignore_ascii_case(name)) {
            caps.push(glyph.to_string());
        }
    }
    caps.push(key_label(&key.to_ascii_lowercase()));
    caps
}

/// The logo key's name off macOS.
const SUPER: &str = if cfg!(windows) { "Win" } else { "Super" };

fn key_label(key: &str) -> String {
    if !cfg!(target_os = "macos") {
        match key {
            "return" => return "Enter".into(),
            "tab" => return "Tab".into(),
            "backspace" => return "Backspace".into(),
            "delete" => return "Delete".into(),
            _ => {}
        }
    }
    let named = match key {
        "space" => "Space",
        "return" => "↩",
        "tab" => "⇥",
        "escape" => "Esc",
        "backspace" => "⌫",
        "delete" => "⌦",
        "left" => "←",
        "right" => "→",
        "up" => "↑",
        "down" => "↓",
        "home" => "Home",
        "end" => "End",
        "pageup" => "Page Up",
        "pagedown" => "Page Down",
        "minus" => "-",
        "equal" => "=",
        "left_bracket" => "[",
        "right_bracket" => "]",
        "backslash" => "\\",
        "semicolon" => ";",
        "quote" => "'",
        "comma" => ",",
        "period" => ".",
        "slash" => "/",
        "grave" => "`",
        "numpad_decimal" => "Num .",
        "numpad_multiply" => "Num *",
        "numpad_plus" => "Num +",
        "numpad_clear" => "Num Clear",
        "numpad_divide" => "Num /",
        "numpad_enter" => "Num Enter",
        "numpad_minus" => "Num -",
        "numpad_equal" => "Num =",
        "media_play_pause" => "Play/Pause",
        "media_next" => "Next Track",
        "media_previous" => "Previous Track",
        "media_volume_up" => "Volume Up",
        "media_volume_down" => "Volume Down",
        "media_mute" => "Mute",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_string();
    }
    if let Some(n) = key.strip_prefix("numpad_") {
        return format!("Num {n}");
    }
    key.to_uppercase()
}

/// Static UI shortcuts use Slint's logical modifiers: Control is Command
/// on Mac and Ctrl elsewhere; Meta is Control on Mac, Win on Windows and
/// Super on Linux. This is distinct from a configured physical `cmd`
/// hotkey (shown as Win or Super).
pub fn ui_key_cap(mac: &str) -> &str {
    if cfg!(target_os = "macos") {
        return mac;
    }
    match mac {
        "⌘" => "Ctrl",
        "⌃" => SUPER,
        "⌥" => "Alt",
        "⇧" => "Shift",
        "⌫" => "Backspace",
        "⌦" => "Delete",
        "Return" | "↩" => "Enter",
        "⇥" => "Tab",
        _ => mac,
    }
}

/// A trusted, compact UI hint such as ⇧⌘Z, not user text or a saved binding.
pub fn ui_shortcut(mac: &str) -> String {
    if cfg!(target_os = "macos") {
        return mac.to_string();
    }
    let logo = format!("{SUPER}+");
    mac.replace("⌃⌘", &format!("{logo}Ctrl+"))
        .replace("⇧⌘", "Ctrl+Shift+")
        .replace("⌥⌘", "Ctrl+Alt+")
        .replace('⌘', "Ctrl+")
        .replace('⌃', &logo)
        .replace('⌥', "Alt+")
        .replace('⇧', "Shift+")
        .replace('⌫', "Backspace")
        .replace('⌦', "Delete")
}

/// AppKit's virtual key codes (HIToolbox Events.h), the daemon's names.
#[cfg(target_os = "macos")]
const KEY_CODES: &[(u16, &str)] = &[
    (0, "a"),
    (1, "s"),
    (2, "d"),
    (3, "f"),
    (4, "h"),
    (5, "g"),
    (6, "z"),
    (7, "x"),
    (8, "c"),
    (9, "v"),
    (11, "b"),
    (12, "q"),
    (13, "w"),
    (14, "e"),
    (15, "r"),
    (16, "y"),
    (17, "t"),
    (18, "1"),
    (19, "2"),
    (20, "3"),
    (21, "4"),
    (22, "6"),
    (23, "5"),
    (24, "equal"),
    (25, "9"),
    (26, "7"),
    (27, "minus"),
    (28, "8"),
    (29, "0"),
    (30, "right_bracket"),
    (31, "o"),
    (32, "u"),
    (33, "left_bracket"),
    (34, "i"),
    (35, "p"),
    (36, "return"),
    (37, "l"),
    (38, "j"),
    (39, "quote"),
    (40, "k"),
    (41, "semicolon"),
    (42, "backslash"),
    (43, "comma"),
    (44, "slash"),
    (45, "n"),
    (46, "m"),
    (47, "period"),
    (48, "tab"),
    (49, "space"),
    (50, "grave"),
    (51, "backspace"),
    (53, "escape"),
    (64, "f17"),
    (65, "numpad_decimal"),
    (67, "numpad_multiply"),
    (69, "numpad_plus"),
    (71, "numpad_clear"),
    (75, "numpad_divide"),
    (76, "numpad_enter"),
    (78, "numpad_minus"),
    (79, "f18"),
    (80, "f19"),
    (81, "numpad_equal"),
    (82, "numpad_0"),
    (83, "numpad_1"),
    (84, "numpad_2"),
    (85, "numpad_3"),
    (86, "numpad_4"),
    (87, "numpad_5"),
    (88, "numpad_6"),
    (89, "numpad_7"),
    (90, "f20"),
    (91, "numpad_8"),
    (92, "numpad_9"),
    (96, "f5"),
    (97, "f6"),
    (98, "f7"),
    (99, "f3"),
    (100, "f8"),
    (101, "f9"),
    (103, "f11"),
    (105, "f13"),
    (106, "f16"),
    (107, "f14"),
    (109, "f10"),
    (111, "f12"),
    (113, "f15"),
    (115, "home"),
    (116, "pageup"),
    (117, "delete"),
    (118, "f4"),
    (119, "end"),
    (120, "f2"),
    (121, "pagedown"),
    (122, "f1"),
    (123, "left"),
    (124, "right"),
    (125, "down"),
    (126, "up"),
];

#[cfg(target_os = "macos")]
fn key_name(code: u16) -> Option<&'static str> {
    KEY_CODES.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

/// NX key types of the media keys (as the daemon reads them).
#[cfg(target_os = "macos")]
fn media_name(kind: i64) -> Option<&'static str> {
    Some(match kind {
        0 => "media_volume_up",
        1 => "media_volume_down",
        7 => "media_mute",
        16 => "media_play_pause",
        17 => "media_next",
        18 => "media_previous",
        _ => return None,
    })
}

/// NSEventModifierFlags → ctrl 1, alt 2, shift 4, cmd 8.
#[cfg(target_os = "macos")]
fn modifiers(flags: usize) -> u32 {
    ((flags & (1 << 18) != 0) as u32)
        | (((flags & (1 << 19) != 0) as u32) << 1)
        | (((flags & (1 << 17) != 0) as u32) << 2)
        | (((flags & (1 << 20) != 0) as u32) << 3)
}

#[cfg(target_os = "macos")]
fn install() {
    use std::ptr::NonNull;

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    // NSEventMaskKeyDown | KeyUp | FlagsChanged | SystemDefined.
    let mask: u64 = (1 << 10) | (1 << 11) | (1 << 12) | (1 << 14);
    let handler = RcBlock::new(|event: NonNull<AnyObject>| -> *mut AnyObject {
        // SAFETY: AppKit hands the event for the duration of the call, on
        // the main thread; each property read is valid for its type.
        let keep = unsafe { on_event(event.as_ref()) };
        if keep {
            event.as_ptr()
        } else {
            std::ptr::null_mut()
        }
    });
    let blur = RcBlock::new(|_note: NonNull<AnyObject>| release_all());
    // SAFETY: AppKit's class methods on the main thread; the monitor and
    // observer live as long as the app (they are never removed).
    unsafe {
        let monitor: Option<Retained<AnyObject>> = msg_send![
            class!(NSEvent),
            addLocalMonitorForEventsMatchingMask: mask,
            handler: &*handler
        ];
        std::mem::forget(monitor);
        let center: Retained<AnyObject> = msg_send![class!(NSNotificationCenter), defaultCenter];
        let name = ns_string("NSApplicationDidResignActiveNotification");
        let none: *const AnyObject = std::ptr::null();
        let observer: Option<Retained<AnyObject>> = msg_send![
            &*center,
            addObserverForName: &*name,
            object: none,
            queue: none,
            usingBlock: &*blur
        ];
        std::mem::forget(observer);
    }
}

#[cfg(target_os = "macos")]
unsafe fn on_event(event: &objc2::runtime::AnyObject) -> bool {
    use objc2::msg_send;
    // SAFETY: the caller's; key events have keyCode and isARepeat, system
    // events subtype and data1.
    unsafe {
        let kind: usize = msg_send![event, type];
        debug(format_args!("event type {kind}"));
        let flags: usize = msg_send![event, modifierFlags];
        match kind {
            10 | 11 => {
                let code: u16 = msg_send![event, keyCode];
                let repeat: bool = if kind == 10 {
                    msg_send![event, isARepeat]
                } else {
                    false
                };
                match key_name(code) {
                    Some(name) => key(code.into(), name, modifiers(flags), kind == 10, repeat),
                    None => true,
                }
            }
            12 => {
                modifiers_changed(modifiers(flags));
                true
            }
            14 => {
                let subtype: i16 = msg_send![event, subtype];
                if subtype != 8 {
                    return true;
                }
                let data: isize = msg_send![event, data1];
                let nx = ((data >> 16) & 0xffff) as i64;
                let state = (data >> 8) & 0xff;
                let repeat = data & 1 != 0;
                match (media_name(nx), state) {
                    (Some(name), 0x0a) => key(1000 + nx, name, modifiers(flags), true, repeat),
                    (Some(name), 0x0b) => key(1000 + nx, name, modifiers(flags), false, false),
                    _ => true,
                }
            }
            _ => true,
        }
    }
}

#[cfg(target_os = "macos")]
unsafe fn ns_string(text: &str) -> objc2::rc::Retained<objc2::runtime::AnyObject> {
    use objc2::{class, msg_send};
    let text = std::ffi::CString::new(text).expect("no NULs");
    // SAFETY: a valid C string, copied by NSString.
    unsafe { msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()] }
}

#[cfg(not(target_os = "macos"))]
pub fn start() {}

#[cfg(not(target_os = "macos"))]
pub fn release_all() {}

#[cfg(not(target_os = "macos"))]
pub fn capture(capture: Option<Capture>) {
    // No physical-key hook yet. Cancel rather than leaving Settings waiting
    // for a key that this platform's stub cannot deliver.
    if let Some(capture) = capture {
        eprintln!("hotkeys: focused shortcut capture is not implemented on this platform");
        capture(None);
    }
}
