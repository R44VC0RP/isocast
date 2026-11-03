//! Thread-owned RegisterHotKey registrations. No keyboard hook: a WM_HOTKEY
//! wakes the thread, and only a held push binding needs a release-state check.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};

use isocast_engine::hotkeys::{Edge, GlobalHotkeys, Key, OnKey};
use isocast_protocol::hotkeys::{Phase, Registration};
use parking_lot::Mutex;
use windows::Win32::Foundation::{
    ERROR_HOTKEY_ALREADY_REGISTERED, ERROR_REQUIRES_INTERACTIVE_WINDOWSTATION, HANDLE, WAIT_FAILED,
    WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Default)]
pub struct WindowsHotkeys(Mutex<Option<Worker>>);

struct Worker {
    stop: Arc<OwnedHandle>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // SAFETY: both threads retain the same kernel event until after join.
        unsafe { SetEvent(HANDLE(self.stop.as_raw_handle())) }.expect("live stop event");
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl GlobalHotkeys for WindowsHotkeys {
    fn replace(&self, keys: Vec<Key>, generation: u64, on_key: OnKey) -> Vec<Registration> {
        let mut worker = self.0.lock();
        // Join first: no old grabs or queued WM_HOTKEYs survive a rebind.
        *worker = None;
        if keys.is_empty() {
            return Vec::new();
        }
        let fallback = |why: String| {
            keys.iter()
                .map(|k| Registration {
                    binding: k.binding.clone(),
                    global: false,
                    warning: Some(format!(
                        "{}: global hotkey thread unavailable: {why}",
                        k.chord.canonical()
                    )),
                })
                .collect()
        };
        // SAFETY: unnamed, manual-reset event, initially unsignalled; transfer
        // its sole close obligation directly to OwnedHandle.
        let stop = match unsafe { CreateEventW(None, true, false, None) } {
            Ok(handle) => Arc::new(unsafe { OwnedHandle::from_raw_handle(handle.0) }),
            Err(e) => return fallback(e.to_string()),
        };
        let cancel = stop.clone();
        let (ready, result) = mpsc::channel();
        let requested = keys.clone();
        match thread::Builder::new()
            .name("isocast global hotkeys".into())
            .spawn(move || {
                run(requested, generation, on_key, ready, &cancel);
            }) {
            Ok(thread) => {
                *worker = Some(Worker {
                    stop,
                    thread: Some(thread),
                })
            }
            Err(e) => return fallback(e.to_string()),
        }
        result.recv().unwrap_or_else(|e| fallback(e.to_string()))
    }
}

struct Grab {
    key: Key,
    vk: u32,
    down: bool,
}

fn run(
    keys: Vec<Key>,
    generation: u64,
    on_key: OnKey,
    ready: mpsc::Sender<Vec<Registration>>,
    stop: &OwnedHandle,
) {
    let mut grabs = Vec::<Grab>::new();
    let mut results = Vec::with_capacity(keys.len());
    // SAFETY: query the keyboard layout of the foreground window at registration
    // time (thread 0 means this thread when there is no foreground window).
    let layout =
        unsafe { GetKeyboardLayout(GetWindowThreadProcessId(GetForegroundWindow(), None)) };
    for key in keys {
        let mut result = Registration {
            binding: key.binding.clone(),
            global: false,
            warning: None,
        };
        let id = grabs.len() as i32 + 1;
        if let Some(vk) = virtual_key(&key.chord.key, layout) {
            // SAFETY: HWND=None binds to this thread's queue. IDs are unique
            // among successful grabs, bounded by the 256-binding profile limit.
            match unsafe {
                RegisterHotKey(None, id, modifiers(key.chord.modifiers) | MOD_NOREPEAT, vk)
            } {
                Ok(()) => {
                    result.global = true;
                    grabs.push(Grab {
                        key,
                        vk,
                        down: false,
                    });
                }
                Err(e) => {
                    let why = if e.code() == ERROR_HOTKEY_ALREADY_REGISTERED.to_hresult() {
                        "the system or another app owns this chord. Choose another key or use focused input"
                    } else if e.code() == ERROR_REQUIRES_INTERACTIVE_WINDOWSTATION.to_hresult() {
                        "run the daemon in the signed-in desktop session or use focused input"
                    } else {
                        "choose another key or use focused input"
                    };
                    result.warning = Some(format!(
                        "{}: RegisterHotKey failed: {e}; {why}",
                        key.chord.canonical()
                    ));
                }
            }
        } else {
            result.warning = Some(format!(
                "{}: no distinct Windows virtual key for this chord; use focused input",
                key.chord.canonical()
            ));
        }
        results.push(result);
    }
    let send = |g: &Grab, phase| {
        on_key(Edge {
            binding: g.key.binding.clone(),
            generation,
            phase,
        })
    };
    if ready.send(results).is_ok() && !grabs.is_empty() {
        loop {
            let timeout = if grabs.iter().any(|g| g.down) {
                10
            } else {
                INFINITE
            };
            // SAFETY: the handle is kept alive across the wait. The thread
            // owns no windows; its only input messages are shortcut pulses.
            let wake = unsafe {
                MsgWaitForMultipleObjectsEx(
                    Some(&[HANDLE(stop.as_raw_handle())]),
                    timeout,
                    QS_HOTKEY,
                    MWMO_INPUTAVAILABLE,
                )
            };
            if wake == WAIT_OBJECT_0 || wake == WAIT_FAILED {
                break;
            }
            let mut msg = MSG::default();
            // SAFETY: remove only this thread's WM_HOTKEY messages.
            while unsafe { PeekMessageW(&mut msg, None, WM_HOTKEY, WM_HOTKEY, PM_REMOVE) }.as_bool()
            {
                let Some(g) = msg.wParam.0.checked_sub(1).and_then(|i| grabs.get_mut(i)) else {
                    continue;
                };
                // MOD_NOREPEAT means a second pulse proves a release happened,
                // even if a rapid second press landed between two samples.
                if g.down {
                    send(g, Phase::Release);
                }
                send(g, Phase::Press);
                g.down = g.key.hold;
                if !g.down {
                    send(g, Phase::Release);
                }
            }
            for g in grabs.iter_mut().filter(|g| g.down) {
                if !held(g.vk, g.key.chord.modifiers) {
                    g.down = false;
                    send(g, Phase::Release);
                }
            }
        }
    }
    for (i, g) in grabs.iter().enumerate() {
        if g.down {
            send(g, Phase::Release);
        }
        // SAFETY: unregister on the thread that owns these IDs, before exit.
        if let Err(e) = unsafe { UnregisterHotKey(None, i as i32 + 1) } {
            eprintln!("isocastd: unregistering {}: {e}", g.key.binding);
        }
    }
}

fn held(vk: u32, bits: u32) -> bool {
    // SAFETY: only the primary key and required modifiers of the active grab
    // are queried. The high bit is current state; the low bit isn't reliable.
    let down = |vk: i32| unsafe { GetAsyncKeyState(vk) < 0 };
    down(vk as i32)
        && [(1, VK_CONTROL), (2, VK_MENU), (4, VK_SHIFT)]
            .into_iter()
            .all(|(bit, key)| bits & bit == 0 || down(key.0.into()))
        && (bits & 8 == 0 || down(VK_LWIN.0.into()) || down(VK_RWIN.0.into()))
}

fn modifiers(bits: u32) -> HOT_KEY_MODIFIERS {
    [(1, MOD_CONTROL), (2, MOD_ALT), (4, MOD_SHIFT), (8, MOD_WIN)]
        .into_iter()
        .filter(|(bit, _)| bits & bit != 0)
        .fold(HOT_KEY_MODIFIERS(0), |mods, (_, flag)| mods | flag)
}

// Set 1 scan codes for the physical US positions whose VK changes with layout.
fn scan(key: &str) -> Option<u32> {
    if key.len() == 1 {
        for (first, row) in [
            (0x02, "1234567890"),
            (0x10, "qwertyuiop"),
            (0x1e, "asdfghjkl"),
            (0x2c, "zxcvbnm"),
        ] {
            if let Some(at) = row.find(key) {
                return Some(first + at as u32);
            }
        }
    }
    Some(match key {
        "minus" => 0x0c,
        "equal" => 0x0d,
        "left_bracket" => 0x1a,
        "right_bracket" => 0x1b,
        "semicolon" => 0x27,
        "quote" => 0x28,
        "grave" => 0x29,
        "backslash" => 0x2b,
        "comma" => 0x33,
        "period" => 0x34,
        "slash" => 0x35,
        _ => return None,
    })
}

fn virtual_key(key: &str, layout: HKL) -> Option<u32> {
    if let Some(scan) = scan(key) {
        // SAFETY: keyboard layout from GetKeyboardLayout; no pointers retained.
        let vk = unsafe { MapVirtualKeyExW(scan, MAPVK_VSC_TO_VK_EX, Some(layout)) };
        return (vk != 0).then_some(vk);
    }
    if let Some(n @ 1..=20) = key.strip_prefix('f').and_then(|s| s.parse::<u32>().ok()) {
        return Some(u32::from(VK_F1.0) + n - 1);
    }
    if let Some(n @ 0..=9) = key
        .strip_prefix("numpad_")
        .and_then(|s| s.parse::<u32>().ok())
    {
        return Some(u32::from(VK_NUMPAD0.0) + n);
    }
    Some(
        match key {
            "space" => VK_SPACE,
            "return" => VK_RETURN,
            "tab" => VK_TAB,
            "escape" => VK_ESCAPE,
            "backspace" => VK_BACK,
            "delete" => VK_DELETE,
            "left" => VK_LEFT,
            "right" => VK_RIGHT,
            "up" => VK_UP,
            "down" => VK_DOWN,
            "home" => VK_HOME,
            "end" => VK_END,
            "pageup" => VK_PRIOR,
            "pagedown" => VK_NEXT,
            "numpad_decimal" => VK_DECIMAL,
            "numpad_multiply" => VK_MULTIPLY,
            "numpad_plus" => VK_ADD,
            "numpad_clear" => VK_CLEAR,
            "numpad_divide" => VK_DIVIDE,
            "numpad_minus" => VK_SUBTRACT,
            "numpad_equal" => VK_OEM_NEC_EQUAL,
            "media_play_pause" => VK_MEDIA_PLAY_PAUSE,
            "media_next" => VK_MEDIA_NEXT_TRACK,
            "media_previous" => VK_MEDIA_PREV_TRACK,
            "media_volume_up" => VK_VOLUME_UP,
            "media_volume_down" => VK_VOLUME_DOWN,
            "media_mute" => VK_VOLUME_MUTE,
            // RegisterHotKey cannot distinguish extended Return (keypad Enter).
            _ => return None,
        }
        .0
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_physical_rows_and_all_catalogue_keys_except_keypad_enter() {
        assert_eq!(
            ["a", "q", "z", "0", "slash"].map(scan),
            [0x1e, 0x10, 0x2c, 0x0b, 0x35].map(Some)
        );
        let layout = unsafe { GetKeyboardLayout(0) };
        for key in isocast_protocol::hotkeys::key_names() {
            assert_eq!(
                virtual_key(&key, layout).is_some(),
                key != "numpad_enter",
                "{key}"
            );
        }
        assert_eq!(
            ["f13", "f20", "numpad_3", "media_mute"].map(|k| virtual_key(k, layout)),
            [VK_F13, VK_F20, VK_NUMPAD3, VK_VOLUME_MUTE].map(|vk| Some(vk.0.into()))
        );
    }

    #[test]
    fn modifiers_translate_independently_including_win() {
        assert_eq!(
            [1, 2, 4, 8].map(modifiers),
            [MOD_CONTROL, MOD_ALT, MOD_SHIFT, MOD_WIN]
        );
        assert_eq!(modifiers(11), MOD_CONTROL | MOD_ALT | MOD_WIN);
    }

    #[test]
    fn registration_reports_the_station_or_owns_the_chord_until_change_and_drop() {
        let native = WindowsHotkeys::default();
        let rival = WindowsHotkeys::default();
        let register = |native: &WindowsHotkeys, keys| native.replace(keys, 1, Arc::new(|_| {}));
        let key = Key {
            binding: "marker".into(),
            chord: isocast_protocol::hotkeys::Chord::parse("ctrl+alt+shift+f18").unwrap(),
            hold: false,
        };
        // An SSH/service worker cannot own ANY global shortcut. Determine
        // that independently of the registrar, and verify its actual report.
        let probe =
            unsafe { RegisterHotKey(None, 1, MOD_CONTROL | MOD_ALT | MOD_SHIFT, VK_F18.0.into()) };
        if let Err(e) = probe {
            assert_eq!(
                e.code(),
                ERROR_REQUIRES_INTERACTIVE_WINDOWSTATION.to_hresult()
            );
            let status = register(&native, vec![key]).remove(0);
            assert!(!status.global);
            let warning = status.warning.unwrap();
            assert!(
                warning.contains("ctrl+alt+shift+f18") && warning.contains("signed-in desktop")
            );
            return;
        }
        unsafe { UnregisterHotKey(None, 1) }.unwrap();
        assert!(register(&native, vec![key.clone()])[0].global);
        let conflict = register(&rival, vec![key.clone()]).remove(0);
        assert!(!conflict.global);
        assert_eq!(conflict.binding, "marker");
        assert!(conflict.warning.unwrap().contains("ctrl+alt+shift+f18"));
        register(&native, vec![]);
        assert!(register(&rival, vec![key.clone()])[0].global);
        drop(rival);
        assert!(register(&native, vec![key])[0].global);
    }
}
