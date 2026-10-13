//! Carbon owns ordinary global chords, including key-up, without TCC.
//! Media keys use a passive session event tap only when listening is already
//! permitted. All native objects live on the main thread; callbacks only enqueue.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
    mpsc,
};
use std::time::Duration;

use isocast_engine::hotkeys::{Edge, GlobalHotkeys, Key, OnKey};
use isocast_protocol::hotkeys::{Phase, Registration};
use objc2::{class, msg_send, runtime::AnyObject};

type Ref = *mut c_void;
#[repr(C)]
#[derive(Default)]
struct HotKeyId {
    signature: u32,
    id: u32,
}
#[repr(C)]
struct EventType {
    class: u32,
    kind: u32,
}
const SIGNATURE: u32 = u32::from_be_bytes(*b"isoh");
const KEYBOARD: u32 = u32::from_be_bytes(*b"keyb");

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn GetEventDispatcherTarget() -> Ref;
    fn InstallEventHandler(
        target: Ref,
        handler: unsafe extern "C" fn(Ref, Ref, Ref) -> i32,
        count: u32,
        types: *const EventType,
        data: Ref,
        result: *mut Ref,
    ) -> i32;
    fn RegisterEventHotKey(
        code: u32,
        modifiers: u32,
        id: HotKeyId,
        target: Ref,
        options: u32,
        result: *mut Ref,
    ) -> i32;
    fn UnregisterEventHotKey(key: Ref) -> i32;
    fn GetEventKind(event: Ref) -> u32;
    fn GetEventParameter(
        event: Ref,
        name: u32,
        kind: u32,
        actual: *mut u32,
        size: u32,
        actual_size: *mut u32,
        data: Ref,
    ) -> i32;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightListenEventAccess() -> bool;
    fn CGEventTapCreate(
        location: u32,
        placement: u32,
        options: u32,
        mask: u64,
        callback: unsafe extern "C" fn(Ref, u32, Ref, Ref) -> Ref,
        data: Ref,
    ) -> Ref;
    fn CGEventTapEnable(tap: Ref, enable: bool);
    fn CGEventGetFlags(event: Ref) -> u64;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFMachPortCreateRunLoopSource(allocator: Ref, port: Ref, order: isize) -> Ref;
    fn CFMachPortInvalidate(port: Ref);
    fn CFRunLoopGetMain() -> Ref;
    fn CFRunLoopAddSource(run_loop: Ref, source: Ref, mode: Ref);
    fn CFRunLoopRemoveSource(run_loop: Ref, source: Ref, mode: Ref);
    fn CFRelease(value: Ref);
    static kCFRunLoopCommonModes: Ref;
}

#[derive(Default)]
struct Native {
    handler: Ref,
    refs: Vec<Ref>,
    keys: HashMap<u32, String>,
    media: Vec<Key>,
    generation: u64,
    next_id: u32,
    on_key: Option<OnKey>,
    tap: Ref,
    source: Ref,
}

thread_local! { static NATIVE: RefCell<Native> = RefCell::new(Native::default()); }

pub struct MacHotkeys;

impl GlobalHotkeys for MacHotkeys {
    fn replace(&self, keys: Vec<Key>, generation: u64, on_key: OnKey) -> Vec<Registration> {
        let fallback: Vec<_> = keys
            .iter()
            .map(|key| Registration {
                binding: key.binding.clone(),
                global: false,
                warning: Some(
                    "main event loop unavailable; focused hotkeys remain available".into(),
                ),
            })
            .collect();
        if objc2::MainThreadMarker::new().is_some() {
            return NATIVE.with(|native| native.borrow_mut().replace(keys, generation, on_key));
        }
        if crate::MAIN_LOOP.get().is_none() {
            return fallback;
        }
        // 0 queued, 1 running, 2 cancelled. A timed-out queued operation may
        // clean up old grabs, but must never install the requested new ones.
        let phase = Arc::new(AtomicU8::new(0));
        let queued = phase.clone();
        let (tx, rx) = mpsc::channel();
        dispatch2::DispatchQueue::main().exec_async(move || {
            if queued
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                NATIVE.with(|native| native.borrow_mut().replace(Vec::new(), generation, on_key));
                return;
            }
            let result =
                NATIVE.with(|native| native.borrow_mut().replace(keys, generation, on_key));
            let _ = tx.send(result);
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(result) => result,
            Err(_)
                if phase
                    .compare_exchange(0, 2, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok() =>
            {
                fallback
            }
            // Already executing: wait for the actual registration result,
            // never report focused-only while a native grab is installed.
            Err(_) => rx.recv().unwrap_or(fallback),
        }
    }
}

impl Native {
    fn replace(&mut self, keys: Vec<Key>, generation: u64, on_key: OnKey) -> Vec<Registration> {
        // SAFETY: native registration, run-loop and teardown calls are all on
        // the main thread. OS callbacks cannot interleave this synchronous block.
        unsafe {
            for key in self.refs.drain(..) {
                UnregisterEventHotKey(key);
            }
            self.keys.clear();
            self.media.clear();
            if !self.tap.is_null() {
                CFRunLoopRemoveSource(CFRunLoopGetMain(), self.source, kCFRunLoopCommonModes);
                CFMachPortInvalidate(self.tap);
                CFRelease(self.source);
                CFRelease(self.tap);
                self.source = null_mut();
                self.tap = null_mut();
            }
            self.generation = generation;
            self.on_key = Some(on_key);
            let target = GetEventDispatcherTarget();
            let mut installed = 0;
            if self.handler.is_null() && !keys.is_empty() {
                let types = [
                    EventType {
                        class: KEYBOARD,
                        kind: 5,
                    },
                    EventType {
                        class: KEYBOARD,
                        kind: 6,
                    },
                ];
                installed = InstallEventHandler(
                    target,
                    carbon_event,
                    2,
                    types.as_ptr(),
                    null_mut(),
                    &mut self.handler,
                );
            }
            let needs_media = keys.iter().any(|k| k.chord.key.starts_with("media_"));
            if needs_media && CGPreflightListenEventAccess() {
                // kCGSessionEventTap, tail append, listen-only; NX_SYSDEFINED=14.
                self.tap = CGEventTapCreate(1, 1, 1, 1 << 14, media_event, null_mut());
                if !self.tap.is_null() {
                    self.source = CFMachPortCreateRunLoopSource(null_mut(), self.tap, 0);
                    if self.source.is_null() {
                        CFMachPortInvalidate(self.tap);
                        CFRelease(self.tap);
                        self.tap = null_mut();
                    } else {
                        CFRunLoopAddSource(CFRunLoopGetMain(), self.source, kCFRunLoopCommonModes);
                    }
                }
            }
            keys.into_iter().map(|key| {
                let mut result = Registration { binding: key.binding.clone(), global: false, warning: None };
                if key.chord.key.starts_with("media_") {
                    result.global = !self.tap.is_null();
                    if result.global { self.media.push(key); }
                    else { result.warning = Some("media keys need Input Monitoring for Isocast Dev (System Settings > Privacy & Security > Input Monitoring), then restart the daemon; no permission is requested automatically. Focused input remains available; system media behavior is not suppressed".into()); }
                } else if installed != 0 {
                    result.warning = Some(format!("Carbon event handler failed ({installed}); focused input remains available"));
                } else if let Some(code) = key_code(&key.chord.key) {
                    self.next_id = self.next_id.wrapping_add(1);
                    let id = self.next_id;
                    let mut registered = null_mut();
                    let bits = key.chord.modifiers;
                    let modifiers = ((bits & 1 != 0) as u32 * 4096) | ((bits & 2 != 0) as u32 * 2048)
                        | ((bits & 4 != 0) as u32 * 512) | ((bits & 8 != 0) as u32 * 256);
                    let status = RegisterEventHotKey(code, modifiers, HotKeyId { signature: SIGNATURE, id }, target, 1, &mut registered);
                    if status == 0 {
                        self.refs.push(registered);
                        self.keys.insert(id, key.binding);
                        result.global = true;
                    } else {
                        result.warning = Some(format!("{}: global registration failed (Carbon {status}); another app/system shortcut may own this chord. Choose another key; focused input remains available", key.chord.canonical()));
                    }
                } else {
                    result.warning = Some("key is unsupported on macOS; focused input remains available".into());
                }
                result
            }).collect()
        }
    }

    fn send(&self, binding: &str, phase: Phase) {
        if let Some(on_key) = &self.on_key {
            on_key(Edge {
                binding: binding.into(),
                generation: self.generation,
                phase,
            });
        }
    }
}

unsafe extern "C" fn carbon_event(_: Ref, event: Ref, _: Ref) -> i32 {
    let mut id = HotKeyId::default();
    // SAFETY: Carbon supplied this event; id has the documented eight-byte ABI.
    let status = unsafe {
        GetEventParameter(
            event,
            u32::from_be_bytes(*b"----"),
            u32::from_be_bytes(*b"hkid"),
            null_mut(),
            8,
            null_mut(),
            (&mut id as *mut HotKeyId).cast(),
        )
    };
    if status != 0 || id.signature != SIGNATURE {
        return -9874;
    }
    let phase = if unsafe { GetEventKind(event) } == 5 {
        Phase::Press
    } else {
        Phase::Release
    };
    NATIVE.with(|native| {
        let native = native.borrow();
        if let Some(binding) = native.keys.get(&id.id) {
            native.send(binding, phase);
        }
    });
    0
}

unsafe extern "C" fn media_event(_: Ref, kind: u32, event: Ref, _: Ref) -> Ref {
    // SAFETY: the callback runs on the main loop, event is borrowed. A passive
    // tap always returns it unchanged, never consuming the system's media key.
    unsafe {
        NATIVE.with(|native| {
            let native = native.borrow();
            if kind == u32::MAX || kind == u32::MAX - 1 {
                // Release any held action before reenabling a timed-out tap.
                for key in &native.media {
                    native.send(&key.binding, Phase::Release);
                }
                CGEventTapEnable(native.tap, true);
                return;
            }
            if kind != 14 || event.is_null() {
                return;
            }
            let ns: *mut AnyObject = msg_send![class!(NSEvent), eventWithCGEvent: event];
            if ns.is_null() {
                return;
            }
            let subtype: i16 = msg_send![ns, subtype];
            if subtype != 8 {
                return;
            }
            let data: isize = msg_send![ns, data1];
            let code = (data >> 16) & 0xffff;
            let state = (data >> 8) & 0xff;
            if state != 0x0a && state != 0x0b {
                return;
            }
            let flags = CGEventGetFlags(event);
            let modifiers = ((flags & (1 << 18) != 0) as u32)
                | (((flags & (1 << 19) != 0) as u32) << 1)
                | (((flags & (1 << 17) != 0) as u32) << 2)
                | (((flags & (1 << 20) != 0) as u32) << 3);
            for key in &native.media {
                let expected = match key.chord.key.as_str() {
                    "media_volume_up" => 0,
                    "media_volume_down" => 1,
                    "media_mute" => 7,
                    "media_play_pause" => 16,
                    "media_next" => 17,
                    "media_previous" => 18,
                    _ => -1,
                };
                // Release even if modifiers were lifted before the media key.
                if code == expected && (state == 0x0b || modifiers == key.chord.modifiers) {
                    native.send(
                        &key.binding,
                        if state == 0x0a {
                            Phase::Press
                        } else {
                            Phase::Release
                        },
                    );
                }
            }
        });
    }
    event
}

fn key_code(key: &str) -> Option<u32> {
    // Apple's Events.h / HIToolbox virtual-key constants, not character codes.
    let code = match key {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "equal" => 24,
        "9" => 25,
        "7" => 26,
        "minus" => 27,
        "8" => 28,
        "0" => 29,
        "right_bracket" => 30,
        "o" => 31,
        "u" => 32,
        "left_bracket" => 33,
        "i" => 34,
        "p" => 35,
        "return" => 36,
        "l" => 37,
        "j" => 38,
        "quote" => 39,
        "k" => 40,
        "semicolon" => 41,
        "backslash" => 42,
        "comma" => 43,
        "slash" => 44,
        "n" => 45,
        "m" => 46,
        "period" => 47,
        "tab" => 48,
        "space" => 49,
        "grave" => 50,
        "backspace" => 51,
        "escape" => 53,
        "numpad_decimal" => 65,
        "numpad_multiply" => 67,
        "numpad_plus" => 69,
        "numpad_clear" => 71,
        "numpad_divide" => 75,
        "numpad_enter" => 76,
        "numpad_minus" => 78,
        "numpad_equal" => 81,
        "numpad_0" => 82,
        "numpad_1" => 83,
        "numpad_2" => 84,
        "numpad_3" => 85,
        "numpad_4" => 86,
        "numpad_5" => 87,
        "numpad_6" => 88,
        "numpad_7" => 89,
        "numpad_8" => 91,
        "numpad_9" => 92,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        "f13" => 105,
        "f14" => 107,
        "f15" => 113,
        "f16" => 106,
        "f17" => 64,
        "f18" => 79,
        "f19" => 80,
        "f20" => 90,
        "home" => 115,
        "pageup" => 116,
        "delete" => 117,
        "end" => 119,
        "pagedown" => 121,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        _ => return None,
    };
    Some(code)
}
