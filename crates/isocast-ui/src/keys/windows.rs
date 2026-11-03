//! Settings borrows a physical press from the focused winit window. No system
//! hook, polling of typed keys, or background input in the UI process.

use std::cell::{Cell, RefCell};

use slint::winit_030::winit::{
    event::{ElementState, WindowEvent},
    keyboard::{KeyCode, ModifiersState, PhysicalKey},
};

use super::{Capture, chord_text};

thread_local! {
    static NEXT: RefCell<Option<Capture>> = const { RefCell::new(None) };
    static MODS: Cell<ModifiersState> = Cell::default();
    static TAKEN: Cell<Option<PhysicalKey>> = const { Cell::new(None) };
}

pub fn capture(next: Option<Capture>) {
    NEXT.replace(next);
}

pub fn release_all() {
    MODS.set(ModifiersState::empty());
    TAKEN.set(None);
    if let Some(cancel) = NEXT.take() {
        cancel(None);
    }
}

pub fn capture_event(event: &WindowEvent) -> bool {
    if let WindowEvent::ModifiersChanged(mods) = event {
        MODS.set(mods.state());
    }
    let WindowEvent::KeyboardInput {
        event,
        is_synthetic: false,
        ..
    } = event
    else {
        return false;
    };
    key_event(event.physical_key, event.state, event.repeat)
}

fn key_event(physical_key: PhysicalKey, state: ElementState, repeat: bool) -> bool {
    if TAKEN.get() == Some(physical_key) {
        if state == ElementState::Released {
            TAKEN.set(None);
        }
        return true;
    }
    if state != ElementState::Pressed || repeat || NEXT.with_borrow(Option::is_none) {
        return false;
    }
    let PhysicalKey::Code(code) = physical_key else {
        return false;
    };
    let Some(name) = key_name(code) else {
        return true;
    }; // e.g. a modifier alone
    let mods = MODS.get();
    let bits = u32::from(mods.control_key())
        | (u32::from(mods.alt_key()) << 1)
        | (u32::from(mods.shift_key()) << 2)
        | (u32::from(mods.super_key()) << 3);
    TAKEN.set(Some(physical_key));
    if let Some(done) = NEXT.take() {
        done((name != "escape" || bits != 0).then(|| chord_text(bits, name)));
    }
    true
}

fn key_name(code: KeyCode) -> Option<&'static str> {
    use KeyCode::*;
    // Match physical US positions; KeyEvent::text / logical_key change with
    // layout and modifiers. Only names from the protocol's catalogue are used.
    macro_rules! names { ($($code:ident: $name:literal),* $(,)?) => {
        match code { $($code => Some($name),)* _ => None }
    }; }
    names! {
        KeyA:"a", KeyB:"b", KeyC:"c", KeyD:"d", KeyE:"e", KeyF:"f", KeyG:"g",
        KeyH:"h", KeyI:"i", KeyJ:"j", KeyK:"k", KeyL:"l", KeyM:"m", KeyN:"n",
        KeyO:"o", KeyP:"p", KeyQ:"q", KeyR:"r", KeyS:"s", KeyT:"t", KeyU:"u",
        KeyV:"v", KeyW:"w", KeyX:"x", KeyY:"y", KeyZ:"z",
        Digit0:"0", Digit1:"1", Digit2:"2", Digit3:"3", Digit4:"4", Digit5:"5",
        Digit6:"6", Digit7:"7", Digit8:"8", Digit9:"9",
        F1:"f1", F2:"f2", F3:"f3", F4:"f4", F5:"f5", F6:"f6", F7:"f7", F8:"f8", F9:"f9", F10:"f10",
        F11:"f11", F12:"f12", F13:"f13", F14:"f14", F15:"f15", F16:"f16", F17:"f17", F18:"f18", F19:"f19", F20:"f20",
        Space:"space", Enter:"return", Tab:"tab", Escape:"escape", Backspace:"backspace", Delete:"delete",
        ArrowLeft:"left", ArrowRight:"right", ArrowUp:"up", ArrowDown:"down", Home:"home", End:"end", PageUp:"pageup", PageDown:"pagedown",
        Minus:"minus", Equal:"equal", BracketLeft:"left_bracket", BracketRight:"right_bracket", Backslash:"backslash",
        Semicolon:"semicolon", Quote:"quote", Comma:"comma", Period:"period", Slash:"slash", Backquote:"grave",
        Numpad0:"numpad_0", Numpad1:"numpad_1", Numpad2:"numpad_2", Numpad3:"numpad_3", Numpad4:"numpad_4",
        Numpad5:"numpad_5", Numpad6:"numpad_6", Numpad7:"numpad_7", Numpad8:"numpad_8", Numpad9:"numpad_9",
        NumpadDecimal:"numpad_decimal", NumpadMultiply:"numpad_multiply", NumpadAdd:"numpad_plus", NumpadClear:"numpad_clear",
        NumpadDivide:"numpad_divide", NumpadEnter:"numpad_enter", NumpadSubtract:"numpad_minus", NumpadEqual:"numpad_equal",
        MediaPlayPause:"media_play_pause", MediaTrackNext:"media_next", MediaTrackPrevious:"media_previous",
        AudioVolumeUp:"media_volume_up", AudioVolumeDown:"media_volume_down", AudioVolumeMute:"media_mute",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn recorder_waits_for_a_physical_chord_and_consumes_its_release() {
        let (captured, result) = mpsc::channel();
        capture(Some(Box::new(move |value| captured.send(value).unwrap())));
        let down = |code: KeyCode| key_event(code.into(), ElementState::Pressed, false);
        assert!(down(KeyCode::ControlLeft));
        assert!(result.try_recv().is_err());
        capture_event(&WindowEvent::ModifiersChanged(
            (ModifiersState::CONTROL | ModifiersState::ALT | ModifiersState::SUPER).into(),
        ));
        assert!(down(KeyCode::KeyR));
        assert_eq!(
            result.try_recv().unwrap().as_deref(),
            Some("ctrl+alt+cmd+r")
        );
        assert!(key_event(KeyCode::KeyR.into(), ElementState::Pressed, true));
        assert!(key_event(
            KeyCode::KeyR.into(),
            ElementState::Released,
            false
        ));
        assert!(!down(KeyCode::KeyA));
        release_all();
    }
}
