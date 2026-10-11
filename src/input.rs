//! GPUI input to CEF input: keys, mouse buttons and modifiers, and CEF's
//! cursors back to GPUI's.

use cef::sys::{cef_cursor_type_t as Ct, cef_event_flags_t as Ef, cef_key_event_type_t as Kt};
use cef::{CursorType, KeyEvent, KeyEventType};
use gpui::{CursorStyle, Keystroke, Modifiers, MouseButton};

/// CEF's `EVENTFLAG_*` bits for `modifiers`, plus held mouse `buttons`.
pub(crate) fn event_flags(modifiers: &Modifiers, buttons: &[MouseButton]) -> u32 {
    let mut flags = 0;
    let mut set = |on: bool, flag: Ef| {
        if on {
            flags |= flag.0;
        }
    };
    set(modifiers.shift, Ef::EVENTFLAG_SHIFT_DOWN);
    set(modifiers.control, Ef::EVENTFLAG_CONTROL_DOWN);
    set(modifiers.alt, Ef::EVENTFLAG_ALT_DOWN);
    set(modifiers.platform, Ef::EVENTFLAG_COMMAND_DOWN);
    for button in buttons {
        match button {
            MouseButton::Left => set(true, Ef::EVENTFLAG_LEFT_MOUSE_BUTTON),
            MouseButton::Middle => set(true, Ef::EVENTFLAG_MIDDLE_MOUSE_BUTTON),
            MouseButton::Right => set(true, Ef::EVENTFLAG_RIGHT_MOUSE_BUTTON),
            MouseButton::Navigate(_) => {}
        }
    }
    flags
}

/// A key's DOM `code` and its Windows virtual key code.
fn named(key: &str) -> Option<(&'static str, i32)> {
    Some(match key {
        "enter" => ("Enter", 0x0d),
        "tab" => ("Tab", 0x09),
        "backspace" => ("Backspace", 0x08),
        "escape" => ("Escape", 0x1b),
        "delete" => ("Delete", 0x2e),
        "insert" => ("Insert", 0x2d),
        "home" => ("Home", 0x24),
        "end" => ("End", 0x23),
        "pageup" => ("PageUp", 0x21),
        "pagedown" => ("PageDown", 0x22),
        "left" => ("ArrowLeft", 0x25),
        "up" => ("ArrowUp", 0x26),
        "right" => ("ArrowRight", 0x27),
        "down" => ("ArrowDown", 0x28),
        "space" => ("Space", 0x20),
        "-" => ("Minus", 0xbd),
        "=" => ("Equal", 0xbb),
        "[" => ("BracketLeft", 0xdb),
        "]" => ("BracketRight", 0xdd),
        "\\" => ("Backslash", 0xdc),
        ";" => ("Semicolon", 0xba),
        "'" => ("Quote", 0xde),
        "," => ("Comma", 0xbc),
        "." => ("Period", 0xbe),
        "/" => ("Slash", 0xbf),
        "`" => ("Backquote", 0xc0),
        "shift" => ("ShiftLeft", 0x10),
        "control" => ("ControlLeft", 0x11),
        "alt" => ("AltLeft", 0x12),
        "platform" | "cmd" | "super" | "win" => ("MetaLeft", 0x5b),
        "menu" => ("ContextMenu", 0x5d),
        _ => return None,
    })
}

/// The DOM `code` and Windows virtual key code of `keystroke`'s key, when
/// it has a key position: named keys, F-keys, ASCII letters and digits.
pub(crate) fn key_code(keystroke: &Keystroke) -> Option<(String, i32)> {
    let k = keystroke.key.as_str();
    if let Some((code, vk)) = named(k) {
        return Some((code.to_string(), vk));
    }
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<i32>().ok())
        && (1..=24).contains(&n)
    {
        return Some((format!("F{n}"), 0x6f + n));
    }
    if k.len() == 1 && k.as_bytes()[0].is_ascii_alphanumeric() {
        let c = k.as_bytes()[0].to_ascii_uppercase();
        let code = if c.is_ascii_digit() {
            format!("Digit{}", c as char)
        } else {
            format!("Key{}", c as char)
        };
        return Some((code, c as i32));
    }
    None
}

/// The platform's own key code for a DOM `code`, which Chromium turns back
/// into `KeyboardEvent.code`: XKB keycodes on Linux, virtual key codes on
/// macOS, scan codes on Windows.
fn native_code(code: &str) -> i32 {
    #[cfg(target_os = "macos")]
    let table = MAC;
    #[cfg(not(target_os = "macos"))]
    let table = XKB;
    #[cfg(target_os = "windows")]
    let table = WIN_SCAN;
    table
        .iter()
        .find(|(c, _)| *c == code)
        .map_or(0, |(_, native)| *native)
}

/// Linux evdev codes + 8.
#[cfg(not(target_os = "macos"))]
const XKB: &[(&str, i32)] = &[
    ("Escape", 9), ("Digit1", 10), ("Digit2", 11), ("Digit3", 12), ("Digit4", 13),
    ("Digit5", 14), ("Digit6", 15), ("Digit7", 16), ("Digit8", 17), ("Digit9", 18),
    ("Digit0", 19), ("Minus", 20), ("Equal", 21), ("Backspace", 22), ("Tab", 23),
    ("KeyQ", 24), ("KeyW", 25), ("KeyE", 26), ("KeyR", 27), ("KeyT", 28), ("KeyY", 29),
    ("KeyU", 30), ("KeyI", 31), ("KeyO", 32), ("KeyP", 33), ("BracketLeft", 34),
    ("BracketRight", 35), ("Enter", 36), ("ControlLeft", 37), ("KeyA", 38), ("KeyS", 39),
    ("KeyD", 40), ("KeyF", 41), ("KeyG", 42), ("KeyH", 43), ("KeyJ", 44), ("KeyK", 45),
    ("KeyL", 46), ("Semicolon", 47), ("Quote", 48), ("Backquote", 49), ("ShiftLeft", 50),
    ("Backslash", 51), ("KeyZ", 52), ("KeyX", 53), ("KeyC", 54), ("KeyV", 55), ("KeyB", 56),
    ("KeyN", 57), ("KeyM", 58), ("Comma", 59), ("Period", 60), ("Slash", 61),
    ("AltLeft", 64), ("Space", 65), ("F1", 67), ("F2", 68), ("F3", 69), ("F4", 70),
    ("F5", 71), ("F6", 72), ("F7", 73), ("F8", 74), ("F9", 75), ("F10", 76), ("F11", 95),
    ("F12", 96), ("Home", 110), ("ArrowUp", 111), ("PageUp", 112), ("ArrowLeft", 113),
    ("ArrowRight", 114), ("End", 115), ("ArrowDown", 116), ("PageDown", 117),
    ("Insert", 118), ("Delete", 119), ("MetaLeft", 133), ("ContextMenu", 135),
];

#[cfg(target_os = "macos")]
const MAC: &[(&str, i32)] = &[
    ("KeyA", 0x00), ("KeyS", 0x01), ("KeyD", 0x02), ("KeyF", 0x03), ("KeyH", 0x04),
    ("KeyG", 0x05), ("KeyZ", 0x06), ("KeyX", 0x07), ("KeyC", 0x08), ("KeyV", 0x09),
    ("KeyB", 0x0b), ("KeyQ", 0x0c), ("KeyW", 0x0d), ("KeyE", 0x0e), ("KeyR", 0x0f),
    ("KeyY", 0x10), ("KeyT", 0x11), ("Digit1", 0x12), ("Digit2", 0x13), ("Digit3", 0x14),
    ("Digit4", 0x15), ("Digit6", 0x16), ("Digit5", 0x17), ("Equal", 0x18), ("Digit9", 0x19),
    ("Digit7", 0x1a), ("Minus", 0x1b), ("Digit8", 0x1c), ("Digit0", 0x1d),
    ("BracketRight", 0x1e), ("KeyO", 0x1f), ("KeyU", 0x20), ("BracketLeft", 0x21),
    ("KeyI", 0x22), ("KeyP", 0x23), ("Enter", 0x24), ("KeyL", 0x25), ("KeyJ", 0x26),
    ("Quote", 0x27), ("KeyK", 0x28), ("Semicolon", 0x29), ("Backslash", 0x2a),
    ("Comma", 0x2b), ("Slash", 0x2c), ("KeyN", 0x2d), ("KeyM", 0x2e), ("Period", 0x2f),
    ("Tab", 0x30), ("Space", 0x31), ("Backquote", 0x32), ("Backspace", 0x33),
    ("Escape", 0x35), ("MetaLeft", 0x37), ("ShiftLeft", 0x38), ("AltLeft", 0x3a),
    ("ControlLeft", 0x3b), ("F5", 0x60), ("F6", 0x61), ("F7", 0x62), ("F3", 0x63),
    ("F8", 0x64), ("F9", 0x65), ("F11", 0x67), ("F10", 0x6d), ("F12", 0x6f),
    ("Home", 0x73), ("PageUp", 0x74), ("Delete", 0x75), ("F4", 0x76), ("End", 0x77),
    ("F2", 0x78), ("PageDown", 0x79), ("F1", 0x7a), ("ArrowLeft", 0x7b),
    ("ArrowRight", 0x7c), ("ArrowDown", 0x7d), ("ArrowUp", 0x7e),
];

/// Windows scan codes (set 1); extended keys have 0xe0 in the high byte.
#[cfg(target_os = "windows")]
const WIN_SCAN: &[(&str, i32)] = &[
    ("Escape", 0x01), ("Digit1", 0x02), ("Digit2", 0x03), ("Digit3", 0x04), ("Digit4", 0x05),
    ("Digit5", 0x06), ("Digit6", 0x07), ("Digit7", 0x08), ("Digit8", 0x09), ("Digit9", 0x0a),
    ("Digit0", 0x0b), ("Minus", 0x0c), ("Equal", 0x0d), ("Backspace", 0x0e), ("Tab", 0x0f),
    ("KeyQ", 0x10), ("KeyW", 0x11), ("KeyE", 0x12), ("KeyR", 0x13), ("KeyT", 0x14),
    ("KeyY", 0x15), ("KeyU", 0x16), ("KeyI", 0x17), ("KeyO", 0x18), ("KeyP", 0x19),
    ("BracketLeft", 0x1a), ("BracketRight", 0x1b), ("Enter", 0x1c), ("ControlLeft", 0x1d),
    ("KeyA", 0x1e), ("KeyS", 0x1f), ("KeyD", 0x20), ("KeyF", 0x21), ("KeyG", 0x22),
    ("KeyH", 0x23), ("KeyJ", 0x24), ("KeyK", 0x25), ("KeyL", 0x26), ("Semicolon", 0x27),
    ("Quote", 0x28), ("Backquote", 0x29), ("ShiftLeft", 0x2a), ("Backslash", 0x2b),
    ("KeyZ", 0x2c), ("KeyX", 0x2d), ("KeyC", 0x2e), ("KeyV", 0x2f), ("KeyB", 0x30),
    ("KeyN", 0x31), ("KeyM", 0x32), ("Comma", 0x33), ("Period", 0x34), ("Slash", 0x35),
    ("AltLeft", 0x38), ("Space", 0x39), ("F1", 0x3b), ("F2", 0x3c), ("F3", 0x3d),
    ("F4", 0x3e), ("F5", 0x3f), ("F6", 0x40), ("F7", 0x41), ("F8", 0x42), ("F9", 0x43),
    ("F10", 0x44), ("F11", 0x57), ("F12", 0x58), ("Home", 0xe047), ("ArrowUp", 0xe048),
    ("PageUp", 0xe049), ("ArrowLeft", 0xe04b), ("ArrowRight", 0xe04d), ("End", 0xe04f),
    ("ArrowDown", 0xe050), ("PageDown", 0xe051), ("Insert", 0xe052), ("Delete", 0xe053),
    ("MetaLeft", 0xe05b), ("ContextMenu", 0xe05d),
];

fn key_event(kind: Kt, vk: i32, native: i32, modifiers: u32, character: u16) -> KeyEvent {
    KeyEvent {
        type_: KeyEventType::from(kind),
        modifiers,
        windows_key_code: vk,
        native_key_code: native,
        character,
        unmodified_character: character,
        ..Default::default()
    }
}

/// The CEF event for pressing or releasing `keystroke`'s key. Text comes
/// separately, through [`char_events`].
pub(crate) fn key_down_up(keystroke: &Keystroke, down: bool, repeat: bool) -> Option<KeyEvent> {
    let (code, vk) = key_code(keystroke)?;
    let mut modifiers = event_flags(&keystroke.modifiers, &[]);
    if repeat {
        modifiers |= Ef::EVENTFLAG_IS_REPEAT.0;
    }
    let character = keystroke
        .key_char
        .as_deref()
        .and_then(|c| c.encode_utf16().next())
        .unwrap_or(0);
    let kind = if down { Kt::KEYEVENT_RAWKEYDOWN } else { Kt::KEYEVENT_KEYUP };
    Some(key_event(kind, vk, native_code(&code), modifiers, character))
}

/// The CEF events typing `text`.
pub(crate) fn char_events(text: &str, modifiers: &Modifiers) -> Vec<KeyEvent> {
    let flags = event_flags(modifiers, &[]);
    text.encode_utf16()
        .map(|unit| {
            // Chromium expects Enter as a carriage return.
            let unit = if unit == u16::from(b'\n') { u16::from(b'\r') } else { unit };
            key_event(Kt::KEYEVENT_CHAR, i32::from(unit), 0, flags, unit)
        })
        .collect()
}

/// The GPUI keystroke of a key Chromium received itself (windowed pages).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn keystroke_of(event: &KeyEvent) -> Option<Keystroke> {
    let kind: Kt = event.type_.into();
    if kind != Kt::KEYEVENT_RAWKEYDOWN && kind != Kt::KEYEVENT_KEYDOWN {
        return None;
    }
    let vk = event.windows_key_code;
    let key = match vk {
        0x30..=0x39 | 0x41..=0x5a => ((vk as u8) as char).to_ascii_lowercase().to_string(),
        0x70..=0x87 => format!("f{}", vk - 0x6f),
        _ => {
            let (name, _) = [
                ("enter", 0x0d), ("tab", 0x09), ("backspace", 0x08), ("escape", 0x1b),
                ("delete", 0x2e), ("insert", 0x2d), ("home", 0x24), ("end", 0x23),
                ("pageup", 0x21), ("pagedown", 0x22), ("left", 0x25), ("up", 0x26),
                ("right", 0x27), ("down", 0x28), ("space", 0x20), ("-", 0xbd), ("=", 0xbb),
                ("[", 0xdb), ("]", 0xdd), ("\\", 0xdc), (";", 0xba), ("'", 0xde),
                (",", 0xbc), (".", 0xbe), ("/", 0xbf), ("`", 0xc0),
            ]
            .into_iter()
            .find(|(_, code)| *code == vk)?;
            name.to_string()
        }
    };
    let has = |flag: Ef| event.modifiers & flag.0 != 0;
    Some(Keystroke {
        modifiers: Modifiers {
            control: has(Ef::EVENTFLAG_CONTROL_DOWN),
            alt: has(Ef::EVENTFLAG_ALT_DOWN),
            shift: has(Ef::EVENTFLAG_SHIFT_DOWN),
            platform: has(Ef::EVENTFLAG_COMMAND_DOWN),
            function: false,
        },
        key,
        key_char: None,
    })
}

/// Whether `a` and `b` are the same key with the same modifiers.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn same_keystroke(a: &Keystroke, b: &Keystroke) -> bool {
    a.key == b.key && a.modifiers == b.modifiers
}

pub(crate) fn cursor_style(cursor: CursorType) -> CursorStyle {
    match Ct::from(cursor) {
        Ct::CT_HAND => CursorStyle::PointingHand,
        Ct::CT_IBEAM => CursorStyle::IBeam,
        Ct::CT_VERTICALTEXT => CursorStyle::IBeamCursorForVerticalLayout,
        Ct::CT_CROSS | Ct::CT_CELL => CursorStyle::Crosshair,
        Ct::CT_EASTRESIZE => CursorStyle::ResizeRight,
        Ct::CT_WESTRESIZE => CursorStyle::ResizeLeft,
        Ct::CT_EASTWESTRESIZE => CursorStyle::ResizeLeftRight,
        Ct::CT_NORTHRESIZE => CursorStyle::ResizeUp,
        Ct::CT_SOUTHRESIZE => CursorStyle::ResizeDown,
        Ct::CT_NORTHSOUTHRESIZE => CursorStyle::ResizeUpDown,
        Ct::CT_NORTHWESTRESIZE | Ct::CT_SOUTHEASTRESIZE | Ct::CT_NORTHWESTSOUTHEASTRESIZE => {
            CursorStyle::ResizeUpLeftDownRight
        }
        Ct::CT_NORTHEASTRESIZE | Ct::CT_SOUTHWESTRESIZE | Ct::CT_NORTHEASTSOUTHWESTRESIZE => {
            CursorStyle::ResizeUpRightDownLeft
        }
        Ct::CT_COLUMNRESIZE => CursorStyle::ResizeColumn,
        Ct::CT_ROWRESIZE => CursorStyle::ResizeRow,
        Ct::CT_GRAB => CursorStyle::OpenHand,
        Ct::CT_GRABBING => CursorStyle::ClosedHand,
        Ct::CT_NOTALLOWED | Ct::CT_NODROP => CursorStyle::OperationNotAllowed,
        Ct::CT_ALIAS => CursorStyle::DragLink,
        Ct::CT_COPY => CursorStyle::DragCopy,
        Ct::CT_CONTEXTMENU => CursorStyle::ContextualMenu,
        _ => CursorStyle::Arrow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(s: &str) -> Keystroke {
        Keystroke::parse(s).unwrap()
    }

    #[test]
    fn letters_and_named_keys_have_codes() {
        assert_eq!(key_code(&stroke("a")), Some(("KeyA".into(), 0x41)));
        assert_eq!(key_code(&stroke("7")), Some(("Digit7".into(), 0x37)));
        assert_eq!(key_code(&stroke("left")), Some(("ArrowLeft".into(), 0x25)));
        assert_eq!(key_code(&stroke("f5")), Some(("F5".into(), 0x74)));
        assert_eq!(key_code(&stroke("ů")), None);
    }

    #[test]
    fn key_events_carry_modifiers_and_native_codes() {
        let down = key_down_up(&stroke("ctrl-shift-a"), true, true).unwrap();
        assert_eq!(Kt::from(down.type_), Kt::KEYEVENT_RAWKEYDOWN);
        assert_eq!(down.windows_key_code, 0x41);
        let flags = Ef::EVENTFLAG_CONTROL_DOWN.0 | Ef::EVENTFLAG_SHIFT_DOWN.0 | Ef::EVENTFLAG_IS_REPEAT.0;
        assert_eq!(down.modifiers, flags);
        #[cfg(target_os = "linux")]
        assert_eq!(down.native_key_code, 38);
        let up = key_down_up(&stroke("a"), false, false).unwrap();
        assert_eq!(Kt::from(up.type_), Kt::KEYEVENT_KEYUP);
    }

    #[test]
    fn typed_text_becomes_char_events() {
        let events = char_events("é\n", &Modifiers::default());
        assert_eq!(events.len(), 2);
        assert_eq!(Kt::from(events[0].type_), Kt::KEYEVENT_CHAR);
        assert_eq!(events[0].character, 'é' as u16);
        assert_eq!(events[1].windows_key_code, 13);
        // Outside the BMP: two UTF-16 units.
        assert_eq!(char_events("😀", &Modifiers::default()).len(), 2);
    }

    #[test]
    fn windowed_keys_map_back_to_keystrokes() {
        let down = key_down_up(&stroke("ctrl-k"), true, false).unwrap();
        let keystroke = keystroke_of(&down).unwrap();
        assert!(same_keystroke(&keystroke, &stroke("ctrl-k")));
        let f5 = key_down_up(&stroke("f5"), true, false).unwrap();
        assert_eq!(keystroke_of(&f5).unwrap().key, "f5");
    }
}
