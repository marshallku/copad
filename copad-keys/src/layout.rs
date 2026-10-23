//! Physical keys → the US PC-101 layout, for the protocol's "base layout key", and the macOS
//! virtual key codes of the keys that type nothing.

use crate::{Key, Kp, Modifier};

/// The US-layout character of a Linux evdev key code (`KEY_A` = 30; an XKB/GDK hardware
/// keycode is this plus 8).
pub fn us_base_evdev(code: u32) -> Option<char> {
    const ROW_DIGITS: &[u8] = b"1234567890-="; // 2..=13
    const ROW_Q: &[u8] = b"qwertyuiop[]"; // 16..=27
    const ROW_A: &[u8] = b"asdfghjkl;'`"; // 30..=41
    const ROW_Z: &[u8] = b"zxcvbnm,./"; // 44..=53
    let at = |row: &[u8], first: u32| row.get((code - first) as usize).map(|&b| b as char);
    match code {
        2..=13 => at(ROW_DIGITS, 2),
        16..=27 => at(ROW_Q, 16),
        30..=41 => at(ROW_A, 30),
        43 => Some('\\'),
        44..=53 => at(ROW_Z, 44),
        57 => Some(' '),
        _ => None,
    }
}

/// The US-layout character of a macOS virtual key code (`kVK_ANSI_*`).
pub fn us_base_macos(code: u16) -> Option<char> {
    Some(match code {
        0 => 'a',
        1 => 's',
        2 => 'd',
        3 => 'f',
        4 => 'h',
        5 => 'g',
        6 => 'z',
        7 => 'x',
        8 => 'c',
        9 => 'v',
        11 => 'b',
        12 => 'q',
        13 => 'w',
        14 => 'e',
        15 => 'r',
        16 => 'y',
        17 => 't',
        18 => '1',
        19 => '2',
        20 => '3',
        21 => '4',
        22 => '6',
        23 => '5',
        24 => '=',
        25 => '9',
        26 => '7',
        27 => '-',
        28 => '8',
        29 => '0',
        30 => ']',
        31 => 'o',
        32 => 'u',
        33 => '[',
        34 => 'i',
        35 => 'p',
        37 => 'l',
        38 => 'j',
        39 => '\'',
        40 => 'k',
        41 => ';',
        42 => '\\',
        43 => ',',
        44 => '/',
        45 => 'n',
        46 => 'm',
        47 => '.',
        49 => ' ',
        50 => '`',
        _ => return None,
    })
}

/// The key a macOS virtual key code names when that key types nothing on its own (or is a
/// keypad key, which the protocol reports as its own key). `None` = a text key, identified
/// by the characters it types.
pub fn macos_functional(code: u16) -> Option<Key> {
    Some(match code {
        36 => Key::Enter,
        48 => Key::Tab,
        51 => Key::Backspace,
        53 => Key::Escape,
        114 => Key::Insert, // Help, where a PC keyboard has Insert
        117 => Key::Delete,
        115 => Key::Home,
        119 => Key::End,
        116 => Key::PageUp,
        121 => Key::PageDown,
        123 => Key::Left,
        124 => Key::Right,
        125 => Key::Down,
        126 => Key::Up,
        122 => Key::F(1),
        120 => Key::F(2),
        99 => Key::F(3),
        118 => Key::F(4),
        96 => Key::F(5),
        97 => Key::F(6),
        98 => Key::F(7),
        100 => Key::F(8),
        101 => Key::F(9),
        109 => Key::F(10),
        103 => Key::F(11),
        111 => Key::F(12),
        105 => Key::F(13),
        107 => Key::F(14),
        113 => Key::F(15),
        106 => Key::F(16),
        64 => Key::F(17),
        79 => Key::F(18),
        80 => Key::F(19),
        90 => Key::F(20),
        110 => Key::Menu,
        82 => Key::Kp(Kp::Digit(0)),
        83 => Key::Kp(Kp::Digit(1)),
        84 => Key::Kp(Kp::Digit(2)),
        85 => Key::Kp(Kp::Digit(3)),
        86 => Key::Kp(Kp::Digit(4)),
        87 => Key::Kp(Kp::Digit(5)),
        88 => Key::Kp(Kp::Digit(6)),
        89 => Key::Kp(Kp::Digit(7)),
        91 => Key::Kp(Kp::Digit(8)),
        92 => Key::Kp(Kp::Digit(9)),
        65 => Key::Kp(Kp::Decimal),
        67 => Key::Kp(Kp::Multiply),
        69 => Key::Kp(Kp::Add),
        75 => Key::Kp(Kp::Divide),
        76 => Key::Kp(Kp::Enter),
        78 => Key::Kp(Kp::Subtract),
        81 => Key::Kp(Kp::Equal),
        71 => Key::NumLock, // Clear, where a PC keypad has Num Lock
        56 => Key::Modifier(Modifier::LeftShift),
        60 => Key::Modifier(Modifier::RightShift),
        59 => Key::Modifier(Modifier::LeftControl),
        62 => Key::Modifier(Modifier::RightControl),
        58 => Key::Modifier(Modifier::LeftAlt),
        61 => Key::Modifier(Modifier::RightAlt),
        55 => Key::Modifier(Modifier::LeftSuper),
        54 => Key::Modifier(Modifier::RightSuper),
        57 => Key::CapsLock,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evdev_rows_line_up_with_the_us_layout() {
        assert_eq!(us_base_evdev(2), Some('1'));
        assert_eq!(us_base_evdev(11), Some('0'));
        assert_eq!(us_base_evdev(16), Some('q'));
        assert_eq!(us_base_evdev(30), Some('a'));
        assert_eq!(us_base_evdev(46), Some('c'));
        assert_eq!(us_base_evdev(53), Some('/'));
        assert_eq!(us_base_evdev(41), Some('`'));
        assert_eq!(us_base_evdev(43), Some('\\'));
        assert_eq!(us_base_evdev(57), Some(' '));
        assert_eq!(us_base_evdev(1), None); // Esc
        assert_eq!(us_base_evdev(28), None); // Enter
    }

    #[test]
    fn macos_codes() {
        assert_eq!(us_base_macos(8), Some('c'));
        assert_eq!(us_base_macos(50), Some('`'));
        assert_eq!(us_base_macos(36), None);
        assert_eq!(macos_functional(36), Some(Key::Enter));
        assert_eq!(macos_functional(76), Some(Key::Kp(Kp::Enter)));
        assert_eq!(macos_functional(99), Some(Key::F(3)));
        assert_eq!(macos_functional(0), None);
        assert_eq!(macos_functional(49), None); // Space is a text key
    }
}

/// Name the key of a macOS key event: its virtual key code for the keys that type nothing,
/// else the single character it types with no modifiers in the active layout
/// (`characters(byApplyingModifiers: [])`), lowercased when that stays one character, else
/// the US key in the same place (a dead key types nothing on its own).
pub fn macos_key(code: u16, unmodified: &str) -> Option<Key> {
    if let Some(k) = macos_functional(code) {
        return Some(k);
    }
    let mut chars = unmodified.chars();
    if let (Some(c), None) = (chars.next(), chars.next())
        && !c.is_control()
    {
        let mut lower = c.to_lowercase();
        return Some(Key::Char(match (lower.next(), lower.next()) {
            (Some(l), None) => l,
            _ => c,
        }));
    }
    us_base_macos(code).map(Key::Char)
}

/// The single character of `s`, if it is exactly one (and not a control character).
pub fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() => Some(c),
        _ => None,
    }
}

#[cfg(test)]
mod macos_key_tests {
    use super::*;

    #[test]
    fn names_keys_by_code_then_character_then_position() {
        assert_eq!(macos_key(36, "\r"), Some(Key::Enter));
        assert_eq!(macos_key(0, "a"), Some(Key::Char('a')));
        // A Russian layout's key at the `c` position.
        assert_eq!(macos_key(8, "с"), Some(Key::Char('с')));
        // Caps Lock / a layout that types capitals unmodified still names the lowercase key.
        assert_eq!(macos_key(0, "A"), Some(Key::Char('a')));
        // A dead key types nothing: fall back to the position.
        assert_eq!(macos_key(14, ""), Some(Key::Char('e')));
        // Several characters (a layout ligature): the position, not the first one.
        assert_eq!(macos_key(0, "ab"), Some(Key::Char('a')));
        assert_eq!(macos_key(200, ""), None);
        assert_eq!(single_char("A"), Some('A'));
        assert_eq!(single_char(""), None);
        assert_eq!(single_char("\u{3}"), None);
    }
}

/// The key an X keysym (GDK keyval) names when that key types nothing on its own (or is a
/// keypad key, which the protocol reports as its own key). `None` = a text key.
pub fn xkb_functional(sym: u32) -> Option<Key> {
    Some(match sym {
        0xff1b => Key::Escape,
        0xff0d => Key::Enter,
        0xff09 | 0xfe20 => Key::Tab, // Tab, ISO_Left_Tab (Shift+Tab)
        0xff08 => Key::Backspace,
        0xff63 => Key::Insert,
        0xffff => Key::Delete,
        0xff51 => Key::Left,
        0xff52 => Key::Up,
        0xff53 => Key::Right,
        0xff54 => Key::Down,
        0xff55 => Key::PageUp,
        0xff56 => Key::PageDown,
        0xff50 => Key::Home,
        0xff57 => Key::End,
        0xffe5 => Key::CapsLock,
        0xff14 => Key::ScrollLock,
        0xff7f => Key::NumLock,
        0xff61 => Key::PrintScreen,
        0xff13 => Key::Pause,
        0xff67 => Key::Menu,
        s @ 0xffbe..=0xffe0 => Key::F((s - 0xffbe + 1) as u8),
        s @ 0xffb0..=0xffb9 => Key::Kp(Kp::Digit((s - 0xffb0) as u8)),
        0xffae => Key::Kp(Kp::Decimal),
        0xffaf => Key::Kp(Kp::Divide),
        0xffaa => Key::Kp(Kp::Multiply),
        0xffad => Key::Kp(Kp::Subtract),
        0xffab => Key::Kp(Kp::Add),
        0xff8d => Key::Kp(Kp::Enter),
        0xffbd => Key::Kp(Kp::Equal),
        0xffac => Key::Kp(Kp::Separator),
        0xff96 => Key::Kp(Kp::Left),
        0xff97 => Key::Kp(Kp::Up),
        0xff98 => Key::Kp(Kp::Right),
        0xff99 => Key::Kp(Kp::Down),
        0xff9a => Key::Kp(Kp::PageUp),
        0xff9b => Key::Kp(Kp::PageDown),
        0xff95 => Key::Kp(Kp::Home),
        0xff9c => Key::Kp(Kp::End),
        0xff9e => Key::Kp(Kp::Insert),
        0xff9f => Key::Kp(Kp::Delete),
        0xff9d => Key::Kp(Kp::Begin),
        0xffe1 => Key::Modifier(Modifier::LeftShift),
        0xffe2 => Key::Modifier(Modifier::RightShift),
        0xffe3 => Key::Modifier(Modifier::LeftControl),
        0xffe4 => Key::Modifier(Modifier::RightControl),
        0xffe9 => Key::Modifier(Modifier::LeftAlt),
        0xffea => Key::Modifier(Modifier::RightAlt),
        0xffeb => Key::Modifier(Modifier::LeftSuper),
        0xffec => Key::Modifier(Modifier::RightSuper),
        0xffed => Key::Modifier(Modifier::LeftHyper),
        0xffee => Key::Modifier(Modifier::RightHyper),
        0xffe7 => Key::Modifier(Modifier::LeftMeta),
        0xffe8 => Key::Modifier(Modifier::RightMeta),
        0xfe03 => Key::Modifier(Modifier::IsoLevel3Shift),
        0xfe11 => Key::Modifier(Modifier::IsoLevel5Shift),
        // XF86 media keys → the protocol's media numbers.
        0x1008ff14 => Key::Code(57428), // AudioPlay
        0x1008ff31 => Key::Code(57429), // AudioPause
        0x1008ff15 => Key::Code(57432), // AudioStop
        0x1008ff97 => Key::Code(57433), // AudioForward
        0x1008ff3e => Key::Code(57434), // AudioRewind
        0x1008ff17 => Key::Code(57435), // AudioNext
        0x1008ff16 => Key::Code(57436), // AudioPrev
        0x1008ff1c => Key::Code(57437), // AudioRecord
        0x1008ff11 => Key::Code(57438), // AudioLowerVolume
        0x1008ff13 => Key::Code(57439), // AudioRaiseVolume
        0x1008ff12 => Key::Code(57440), // AudioMute
        _ => return None,
    })
}

#[cfg(test)]
mod xkb_tests {
    use super::*;

    #[test]
    fn keysyms() {
        assert_eq!(xkb_functional(0xff0d), Some(Key::Enter));
        assert_eq!(xkb_functional(0xfe20), Some(Key::Tab));
        assert_eq!(xkb_functional(0xffbe), Some(Key::F(1)));
        assert_eq!(xkb_functional(0xffc0), Some(Key::F(3)));
        assert_eq!(xkb_functional(0xffe0), Some(Key::F(35)));
        assert_eq!(xkb_functional(0xffb5), Some(Key::Kp(Kp::Digit(5))));
        assert_eq!(xkb_functional(0xff8d), Some(Key::Kp(Kp::Enter)));
        assert_eq!(
            xkb_functional(0xffe1),
            Some(Key::Modifier(Modifier::LeftShift))
        );
        assert_eq!(xkb_functional(0x61), None); // `a`
        assert_eq!(xkb_functional(0x20), None); // space types text
    }
}
