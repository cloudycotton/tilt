//! Keysym constants and char <-> keysym conversion.

pub const NO_SYMBOL: u32 = 0;
pub const XK_BACKSPACE: u32 = 0xff08;
pub const XK_TAB: u32 = 0xff09;
pub const XK_RETURN: u32 = 0xff0d;
pub const XK_SCROLL_LOCK: u32 = 0xff14;
pub const XK_ESCAPE: u32 = 0xff1b;
pub const XK_DELETE: u32 = 0xffff;
pub const XK_MODE_SWITCH: u32 = 0xff7e;
pub const XK_NUM_LOCK: u32 = 0xff7f;
pub const XK_KP_SPACE: u32 = 0xff80;
pub const XK_KP_EQUAL: u32 = 0xffbd;
pub const XK_SHIFT_L: u32 = 0xffe1;
pub const XK_SHIFT_R: u32 = 0xffe2;
pub const XK_CONTROL_L: u32 = 0xffe3;
pub const XK_CONTROL_R: u32 = 0xffe4;
pub const XK_META_L: u32 = 0xffe7;
pub const XK_HYPER_R: u32 = 0xffee;
pub const XK_ISO_LOCK: u32 = 0xfe01;
pub const XK_ISO_LEVEL3_SHIFT: u32 = 0xfe03;
pub const XK_ISO_LEVEL5_LOCK: u32 = 0xfe13;
/// Placeholder keysym that never names a key.
pub const XK_VOID_SYMBOL: u32 = 0x00ff_ffff;

/// Keysyms for code points outside Latin-1 are this plus the code point.
const UNICODE_KEYSYM: u32 = 0x0100_0000;
/// Vendor keysyms (XF86 media keys and the like) start here.
const VENDOR_KEYSYMS: u32 = 0x1000_0000;
/// The largest keysym value; keysyms are 29-bit.
const MAX_KEYSYM: u32 = 0x1fff_ffff;

/// The keysym that types `c`: printable Latin-1 maps to its code point, '\n' to Return, '\t' to
/// Tab, BS/ESC/DEL to their function keysyms, every other control character (including '\r', so
/// CRLF text types one Return) to None, and everything else to `0x0100_0000 | code point`.
pub fn char_to_keysym(c: char) -> Option<u32> {
    match c {
        '\n' => Some(XK_RETURN),
        '\t' => Some(XK_TAB),
        '\u{8}' => Some(XK_BACKSPACE),
        '\u{1b}' => Some(XK_ESCAPE),
        '\u{7f}' => Some(XK_DELETE),
        ' '..='~' | '\u{a0}'..='\u{ff}' => Some(c as u32),
        c if c.is_control() => None,
        c => Some(UNICODE_KEYSYM | c as u32),
    }
}

/// The character a Latin-1 or Unicode keysym stands for; None for function keys and the legacy
/// non-Latin-1 ranges.
pub fn keysym_to_char(keysym: u32) -> Option<char> {
    match keysym {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(keysym),
        0x0100_0000..=0x0110_ffff => char::from_u32(keysym - UNICODE_KEYSYM),
        _ => None,
    }
}

/// Whether a client may send `keysym`: not NoSymbol or VoidSymbol, and within the 29-bit range.
pub fn is_valid(keysym: u32) -> bool {
    keysym != NO_SYMBOL && keysym != XK_VOID_SYMBOL && keysym <= MAX_KEYSYM
}

/// Modifiers, lock keys and the ISO level shifts: keys whose repeated KEY down is not autorepeat.
pub fn is_modifier(keysym: u32) -> bool {
    matches!(
        keysym,
        XK_SHIFT_L..=XK_HYPER_R
            | XK_ISO_LOCK..=XK_ISO_LEVEL5_LOCK
            | XK_MODE_SWITCH
            | XK_NUM_LOCK
            | XK_SCROLL_LOCK
    )
}

/// Keypad keysyms, whose two levels NumLock swaps.
pub fn is_keypad(keysym: u32) -> bool {
    (XK_KP_SPACE..=XK_KP_EQUAL).contains(&keysym)
}

/// Function keysyms, which stand for a key rather than for text: TTY, cursor, keypad and
/// function keys, modifiers, the ISO keys such as ISO_Left_Tab, and vendor keys. Dead keys
/// count as text: they type with the next key.
pub fn is_function(keysym: u32) -> bool {
    match keysym {
        // dead_grave to dead_currency, and dead_a to dead_longsolidusoverlay.
        0xfe50..=0xfe6f | 0xfe80..=0xfe93 => false,
        0xfd00..=0xffff => true,
        _ => keysym >= VENDOR_KEYSYMS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_maps_to_its_code_point() {
        for c in ' '..='~' {
            assert_eq!(char_to_keysym(c), Some(c as u32), "{c:?}");
        }
        assert_eq!(char_to_keysym('a'), Some(0x61));
        assert_eq!(char_to_keysym('A'), Some(0x41));
        assert_eq!(char_to_keysym('!'), Some(0x21));
    }

    #[test]
    fn latin1_maps_to_its_code_point() {
        assert_eq!(char_to_keysym('\u{a0}'), Some(0xa0)); // nobreakspace
        assert_eq!(char_to_keysym('é'), Some(0xe9)); // eacute
        assert_eq!(char_to_keysym('ü'), Some(0xfc)); // udiaeresis
        assert_eq!(char_to_keysym('Ü'), Some(0xdc)); // Udiaeresis
        assert_eq!(char_to_keysym('ß'), Some(0xdf)); // ssharp
        assert_eq!(char_to_keysym('ÿ'), Some(0xff)); // ydiaeresis
    }

    #[test]
    fn everything_else_maps_to_a_unicode_keysym() {
        assert_eq!(char_to_keysym('€'), Some(0x0100_20ac));
        assert_eq!(char_to_keysym('你'), Some(0x0100_4f60));
        assert_eq!(char_to_keysym('ж'), Some(0x0100_0436));
        assert_eq!(char_to_keysym('Ā'), Some(0x0100_0100));
        // Astral code points stay whole: never split into UTF-16 surrogates.
        assert_eq!(char_to_keysym('😀'), Some(0x0101_f600));
        assert_eq!(char_to_keysym('\u{10ffff}'), Some(0x0110_ffff));
    }

    #[test]
    fn control_characters_use_function_keysyms_or_none() {
        assert_eq!(char_to_keysym('\n'), Some(XK_RETURN));
        assert_eq!(char_to_keysym('\t'), Some(XK_TAB));
        assert_eq!(char_to_keysym('\u{8}'), Some(XK_BACKSPACE));
        assert_eq!(char_to_keysym('\u{1b}'), Some(XK_ESCAPE));
        assert_eq!(char_to_keysym('\u{7f}'), Some(XK_DELETE));
        assert_eq!(char_to_keysym('\r'), None);
        assert_eq!(char_to_keysym('\0'), None);
        assert_eq!(char_to_keysym('\u{85}'), None); // C1 NEL
        assert_eq!(char_to_keysym('\u{9f}'), None);
    }

    #[test]
    fn keysym_to_char_inverts_the_printable_ranges() {
        for c in ['a', 'Z', ' ', '~', '\u{a0}', 'é', 'ÿ', '€', '你', '😀'] {
            assert_eq!(keysym_to_char(char_to_keysym(c).unwrap()), Some(c), "{c:?}");
        }
        assert_eq!(keysym_to_char(XK_RETURN), None);
        assert_eq!(keysym_to_char(0x6c1), None); // Cyrillic_a: legacy range
        assert_eq!(keysym_to_char(0x7f), None);
        assert_eq!(keysym_to_char(0x0100_d800), None); // a surrogate is not a char
    }

    #[test]
    fn validity() {
        assert!(is_valid(0x61));
        assert!(is_valid(0x0110_ffff));
        assert!(is_valid(0x1008_ff11)); // XF86AudioLowerVolume
        assert!(!is_valid(NO_SYMBOL));
        assert!(!is_valid(XK_VOID_SYMBOL));
        assert!(!is_valid(0x2000_0000));
    }

    #[test]
    fn modifiers_locks_and_level_shifts_are_modifiers() {
        for keysym in [
            XK_SHIFT_L,
            XK_SHIFT_R,
            XK_CONTROL_L,
            XK_CONTROL_R,
            0xffe5, // Caps_Lock
            0xffe6, // Shift_Lock
            0xffe7, // Meta_L
            0xffe9, // Alt_L
            0xffea, // Alt_R
            0xffeb, // Super_L
            0xffec, // Super_R
            XK_HYPER_R,
            XK_ISO_LEVEL3_SHIFT,
            0xfe11, // ISO_Level5_Shift
            XK_MODE_SWITCH,
            XK_NUM_LOCK,
            XK_SCROLL_LOCK,
        ] {
            assert!(is_modifier(keysym), "{keysym:#x}");
        }
        // a, Return, Tab, Escape, Left, F1
        for keysym in [0x61, XK_RETURN, XK_TAB, XK_ESCAPE, 0xff51, 0xffbe] {
            assert!(!is_modifier(keysym), "{keysym:#x}");
        }
    }

    #[test]
    fn function_keysyms_are_keys_and_dead_keys_are_text() {
        // Tab, ISO_Left_Tab, KP_7, F1, Shift_L, ISO_Level3_Shift, AccessX_Enable,
        // XF86AudioLowerVolume
        for keysym in [
            XK_TAB,
            0xfe20,
            0xffb7,
            0xffbe,
            XK_SHIFT_L,
            0xfe03,
            0xfe70,
            0x1008_ff11,
        ] {
            assert!(is_function(keysym), "{keysym:#x}");
        }
        // a, ntilde, Cyrillic_ef and seveneighths (legacy), Unicode ж, dead_grave,
        // dead_belowmacron, dead_longsolidusoverlay
        for keysym in [
            0x61,
            0xf1,
            0x6c6,
            0xac6,
            0x0100_0436,
            0xfe50,
            0xfe68,
            0xfe93,
        ] {
            assert!(!is_function(keysym), "{keysym:#x}");
        }
    }

    #[test]
    fn keypad_range() {
        assert!(is_keypad(0xffb7)); // KP_7
        assert!(is_keypad(0xff95)); // KP_Home
        assert!(is_keypad(XK_KP_SPACE));
        assert!(is_keypad(XK_KP_EQUAL));
        assert!(!is_keypad(0x37)); // 7
        assert!(!is_keypad(0xffbe)); // F1
    }
}
