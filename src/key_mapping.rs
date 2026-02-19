//! Convert GDK keyvals (stored in GSettings) to evdev keycodes for raw input monitoring.

use std::collections::HashSet;

/// Modifier flags matching GDK modifier masks used by the settings layer.
pub const MOD_SHIFT: u32 = 1;
pub const MOD_CTRL: u32 = 4;
pub const MOD_ALT: u32 = 8;
pub const MOD_SUPER: u32 = 64;

/// Evdev key codes for modifier keys (left variants).
pub const EV_KEY_LEFTCTRL: u16 = evdev::Key::KEY_LEFTCTRL.code();
pub const EV_KEY_RIGHTCTRL: u16 = evdev::Key::KEY_RIGHTCTRL.code();
pub const EV_KEY_LEFTALT: u16 = evdev::Key::KEY_LEFTALT.code();
pub const EV_KEY_RIGHTALT: u16 = evdev::Key::KEY_RIGHTALT.code();
pub const EV_KEY_LEFTSHIFT: u16 = evdev::Key::KEY_LEFTSHIFT.code();
pub const EV_KEY_RIGHTSHIFT: u16 = evdev::Key::KEY_RIGHTSHIFT.code();
pub const EV_KEY_LEFTMETA: u16 = evdev::Key::KEY_LEFTMETA.code();
pub const EV_KEY_RIGHTMETA: u16 = evdev::Key::KEY_RIGHTMETA.code();

/// A resolved keybinding for evdev matching: a primary key code and required modifier state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvdevKeybinding {
    /// The evdev `Key` code for the primary key (e.g., `KEY_SPACE`).
    pub key_code: u16,
    /// Which modifier flags (MOD_*) must be held.
    pub modifiers: u32,
}

/// Check if an evdev key code is a modifier key.
pub fn is_modifier_key(code: u16) -> bool {
    matches!(
        code,
        x if x == EV_KEY_LEFTCTRL
            || x == EV_KEY_RIGHTCTRL
            || x == EV_KEY_LEFTALT
            || x == EV_KEY_RIGHTALT
            || x == EV_KEY_LEFTSHIFT
            || x == EV_KEY_RIGHTSHIFT
            || x == EV_KEY_LEFTMETA
            || x == EV_KEY_RIGHTMETA
    )
}

/// Get the GDK modifier flag for a held modifier evdev key code.
pub fn modifier_flag_for_key(code: u16) -> Option<u32> {
    match code {
        x if x == EV_KEY_LEFTCTRL || x == EV_KEY_RIGHTCTRL => Some(MOD_CTRL),
        x if x == EV_KEY_LEFTALT || x == EV_KEY_RIGHTALT => Some(MOD_ALT),
        x if x == EV_KEY_LEFTSHIFT || x == EV_KEY_RIGHTSHIFT => Some(MOD_SHIFT),
        x if x == EV_KEY_LEFTMETA || x == EV_KEY_RIGHTMETA => Some(MOD_SUPER),
        _ => None,
    }
}

/// Compute the current modifier bitmask from a set of held modifier key codes.
pub fn modifiers_from_held_keys(held: &HashSet<u16>) -> u32 {
    let mut mods = 0u32;
    for &code in held {
        if let Some(flag) = modifier_flag_for_key(code) {
            mods |= flag;
        }
    }
    mods
}

/// Convert a GDK keyval (e.g., `GDK_KEY_a` = 0x61) to an evdev `Key` code.
///
/// This uses a static mapping table covering common keys. The GDK keyvals for
/// ASCII characters map 1:1 to their ASCII codes, while special keys like
/// F1-F12, arrow keys, etc. have specific GDK constants.
pub fn gdk_keyval_to_evdev(keyval: u32) -> Option<u16> {
    // Normalize uppercase ASCII letters to lowercase
    let keyval = if (0x41..=0x5A).contains(&keyval) {
        keyval + 0x20
    } else {
        keyval
    };

    let key = match keyval {
        // Letters a-z (GDK lowercase = ASCII 0x61-0x7a)
        0x61 => evdev::Key::KEY_A,
        0x62 => evdev::Key::KEY_B,
        0x63 => evdev::Key::KEY_C,
        0x64 => evdev::Key::KEY_D,
        0x65 => evdev::Key::KEY_E,
        0x66 => evdev::Key::KEY_F,
        0x67 => evdev::Key::KEY_G,
        0x68 => evdev::Key::KEY_H,
        0x69 => evdev::Key::KEY_I,
        0x6a => evdev::Key::KEY_J,
        0x6b => evdev::Key::KEY_K,
        0x6c => evdev::Key::KEY_L,
        0x6d => evdev::Key::KEY_M,
        0x6e => evdev::Key::KEY_N,
        0x6f => evdev::Key::KEY_O,
        0x70 => evdev::Key::KEY_P,
        0x71 => evdev::Key::KEY_Q,
        0x72 => evdev::Key::KEY_R,
        0x73 => evdev::Key::KEY_S,
        0x74 => evdev::Key::KEY_T,
        0x75 => evdev::Key::KEY_U,
        0x76 => evdev::Key::KEY_V,
        0x77 => evdev::Key::KEY_W,
        0x78 => evdev::Key::KEY_X,
        0x79 => evdev::Key::KEY_Y,
        0x7a => evdev::Key::KEY_Z,

        // Digits 0-9 (GDK = ASCII 0x30-0x39)
        0x30 => evdev::Key::KEY_0,
        0x31 => evdev::Key::KEY_1,
        0x32 => evdev::Key::KEY_2,
        0x33 => evdev::Key::KEY_3,
        0x34 => evdev::Key::KEY_4,
        0x35 => evdev::Key::KEY_5,
        0x36 => evdev::Key::KEY_6,
        0x37 => evdev::Key::KEY_7,
        0x38 => evdev::Key::KEY_8,
        0x39 => evdev::Key::KEY_9,

        // Function keys (GDK: 0xffbe-0xffc9 for F1-F12)
        0xffbe => evdev::Key::KEY_F1,
        0xffbf => evdev::Key::KEY_F2,
        0xffc0 => evdev::Key::KEY_F3,
        0xffc1 => evdev::Key::KEY_F4,
        0xffc2 => evdev::Key::KEY_F5,
        0xffc3 => evdev::Key::KEY_F6,
        0xffc4 => evdev::Key::KEY_F7,
        0xffc5 => evdev::Key::KEY_F8,
        0xffc6 => evdev::Key::KEY_F9,
        0xffc7 => evdev::Key::KEY_F10,
        0xffc8 => evdev::Key::KEY_F11,
        0xffc9 => evdev::Key::KEY_F12,

        // Special keys
        0xff0d => evdev::Key::KEY_ENTER,     // GDK_KEY_Return
        0xff1b => evdev::Key::KEY_ESC,       // GDK_KEY_Escape
        0xff09 => evdev::Key::KEY_TAB,       // GDK_KEY_Tab
        0xff08 => evdev::Key::KEY_BACKSPACE, // GDK_KEY_BackSpace
        0xffff => evdev::Key::KEY_DELETE,    // GDK_KEY_Delete
        0xff63 => evdev::Key::KEY_INSERT,    // GDK_KEY_Insert
        0xff50 => evdev::Key::KEY_HOME,      // GDK_KEY_Home
        0xff57 => evdev::Key::KEY_END,       // GDK_KEY_End
        0xff55 => evdev::Key::KEY_PAGEUP,    // GDK_KEY_Page_Up
        0xff56 => evdev::Key::KEY_PAGEDOWN,  // GDK_KEY_Page_Down
        0x0020 => evdev::Key::KEY_SPACE,     // GDK_KEY_space

        // Arrow keys
        0xff51 => evdev::Key::KEY_LEFT,  // GDK_KEY_Left
        0xff52 => evdev::Key::KEY_UP,    // GDK_KEY_Up
        0xff53 => evdev::Key::KEY_RIGHT, // GDK_KEY_Right
        0xff54 => evdev::Key::KEY_DOWN,  // GDK_KEY_Down

        // Punctuation/symbols (ASCII code = GDK keyval)
        0x2d => evdev::Key::KEY_MINUS,      // '-'
        0x3d => evdev::Key::KEY_EQUAL,      // '='
        0x5b => evdev::Key::KEY_LEFTBRACE,  // '['
        0x5d => evdev::Key::KEY_RIGHTBRACE, // ']'
        0x5c => evdev::Key::KEY_BACKSLASH,  // '\'
        0x3b => evdev::Key::KEY_SEMICOLON,  // ';'
        0x27 => evdev::Key::KEY_APOSTROPHE, // '\''
        0x60 => evdev::Key::KEY_GRAVE,      // '`'
        0x2c => evdev::Key::KEY_COMMA,      // ','
        0x2e => evdev::Key::KEY_DOT,        // '.'
        0x2f => evdev::Key::KEY_SLASH,      // '/'

        // Print screen, scroll lock, pause
        0xff61 => evdev::Key::KEY_PRINT,      // GDK_KEY_Print
        0xff14 => evdev::Key::KEY_SCROLLLOCK, // GDK_KEY_Scroll_Lock
        0xff13 => evdev::Key::KEY_PAUSE,      // GDK_KEY_Pause

        // Caps lock, Num Lock
        0xffe5 => evdev::Key::KEY_CAPSLOCK, // GDK_KEY_Caps_Lock
        0xff7f => evdev::Key::KEY_NUMLOCK,  // GDK_KEY_Num_Lock

        // Menu key
        0xff67 => evdev::Key::KEY_COMPOSE, // GDK_KEY_Menu

        _ => return None,
    };

    Some(key.code())
}

/// Convert a `ShortcutConfig` (GDK keyval + modifiers) to an `EvdevKeybinding`.
pub fn resolve_keybinding(keyval: u32, modifiers: u32) -> Option<EvdevKeybinding> {
    let key_code = gdk_keyval_to_evdev(keyval)?;
    Some(EvdevKeybinding {
        key_code,
        modifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_letter_mapping() {
        // 'a' = 0x61
        assert_eq!(gdk_keyval_to_evdev(0x61), Some(evdev::Key::KEY_A.code()));
        // 'A' = 0x41 should normalize to lowercase
        assert_eq!(gdk_keyval_to_evdev(0x41), Some(evdev::Key::KEY_A.code()));
    }

    #[test]
    fn test_function_key_mapping() {
        assert_eq!(gdk_keyval_to_evdev(0xffbe), Some(evdev::Key::KEY_F1.code()));
        assert_eq!(
            gdk_keyval_to_evdev(0xffc9),
            Some(evdev::Key::KEY_F12.code())
        );
    }

    #[test]
    fn test_space_mapping() {
        assert_eq!(
            gdk_keyval_to_evdev(0x0020),
            Some(evdev::Key::KEY_SPACE.code())
        );
    }

    #[test]
    fn test_unknown_keyval_returns_none() {
        assert_eq!(gdk_keyval_to_evdev(0x9999), None);
    }

    #[test]
    fn test_resolve_keybinding() {
        let kb = resolve_keybinding(0x61, MOD_CTRL).unwrap();
        assert_eq!(kb.key_code, evdev::Key::KEY_A.code());
        assert_eq!(kb.modifiers, MOD_CTRL);
    }

    #[test]
    fn test_modifier_flags() {
        let mut held = HashSet::new();
        held.insert(EV_KEY_LEFTCTRL);
        held.insert(EV_KEY_LEFTSHIFT);
        let mods = modifiers_from_held_keys(&held);
        assert_eq!(mods, MOD_CTRL | MOD_SHIFT);
    }
}
