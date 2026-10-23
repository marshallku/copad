//! The kitty keyboard protocol for copad's own terminal windows (decisions #164).
//!
//! A program opts in with `CSI > flags u` and from then on expects keys in the protocol's
//! forms — which is how Claude Code, nvim and helix tell Shift+Enter from Enter. This crate
//! holds the parts both GUIs share:
//!
//! - [`encode`]: one key event → the bytes the program asked for (written from the published
//!   specification, <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>).
//! - [`Tracker`]: the negotiation itself, read off the program's output, for the Linux window
//!   whose VTE widget does not implement the protocol (VTE issue #2601). The macOS window gets
//!   it from alacritty_terminal.
//! - [`layout`]: the physical key → US-layout key tables behind the "base layout key".
//!
//! comux has its own encoder (`copad-mux/src/keys.rs`): its input arrives already decoded by
//! crossterm, with that library's quirks, which is a different job.

pub mod layout;
mod tracker;

pub use tracker::Tracker;

/// The progressive-enhancement flags (`CSI = flags u`).
pub mod flags {
    pub const DISAMBIGUATE: u8 = 1;
    pub const EVENT_TYPES: u8 = 2;
    pub const ALTERNATE_KEYS: u8 = 4;
    pub const ALL_KEYS: u8 = 8;
    pub const ASSOCIATED_TEXT: u8 = 16;
    pub const ALL: u8 = 31;
}

/// The modifier bits (the escape code carries `1 + bits`).
pub mod mods {
    pub const SHIFT: u8 = 1;
    pub const ALT: u8 = 2;
    pub const CTRL: u8 = 4;
    pub const SUPER: u8 = 8;
    pub const HYPER: u8 = 16;
    pub const META: u8 = 32;
    pub const CAPS_LOCK: u8 = 64;
    pub const NUM_LOCK: u8 = 128;
    pub const LOCKS: u8 = CAPS_LOCK | NUM_LOCK;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Action {
    #[default]
    Press,
    Repeat,
    Release,
}

/// A keypad key. Digits and operators produce text while Num Lock is on; the rest are the
/// keypad's own navigation keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kp {
    Digit(u8),
    Decimal,
    Divide,
    Multiply,
    Subtract,
    Add,
    Enter,
    Equal,
    Separator,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Insert,
    Delete,
    Begin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modifier {
    LeftShift,
    LeftControl,
    LeftAlt,
    LeftSuper,
    LeftHyper,
    LeftMeta,
    RightShift,
    RightControl,
    RightAlt,
    RightSuper,
    RightHyper,
    RightMeta,
    IsoLevel3Shift,
    IsoLevel5Shift,
}

impl Modifier {
    /// The modifier bit this key drives (0 for the level shifts, which have none).
    pub fn bit(self) -> u8 {
        use Modifier::*;
        match self {
            LeftShift | RightShift => mods::SHIFT,
            LeftControl | RightControl => mods::CTRL,
            LeftAlt | RightAlt => mods::ALT,
            LeftSuper | RightSuper => mods::SUPER,
            LeftHyper | RightHyper => mods::HYPER,
            LeftMeta | RightMeta => mods::META,
            IsoLevel3Shift | IsoLevel5Shift => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A text key, named by the character it types WITHOUT Shift in the active layout
    /// (`a`, `1`, `;`, `с` on a Russian layout).
    Char(char),
    Escape,
    Enter,
    Tab,
    Backspace,
    Insert,
    Delete,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
    /// F1–F35.
    F(u8),
    Kp(Kp),
    Modifier(Modifier),
    /// A key with only a `CSI number u` form, by its number (the media and volume keys,
    /// 57428–57440).
    Code(u32),
}

impl Key {
    /// Keys the protocol reports only when every key is reported (flag 8).
    fn is_modifier_class(self) -> bool {
        matches!(
            self,
            Key::Modifier(_) | Key::CapsLock | Key::NumLock | Key::ScrollLock
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent<'a> {
    pub key: Key,
    /// What the key types WITH Shift in the active layout (`A`, `!`). Reported with flag 4.
    pub shifted: Option<char>,
    /// The key at the same physical position on a US PC-101 layout. Reported with flag 4 when
    /// it differs from the key (`c` for a Russian `с`), so `ctrl+c` shortcuts survive.
    pub base: Option<char>,
    /// [`mods`] bits active for this event. For a modifier key the caller has already applied
    /// the event itself: set on its press, cleared on its release.
    pub mods: u8,
    pub action: Action,
    /// The text this event types, if any: empty when Ctrl/Alt/Super turned the key into a
    /// chord, or for a key that types nothing. Control characters are not text.
    pub text: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoded {
    /// The key types its text, which the platform's normal text path (and its input method)
    /// should deliver as it always has.
    Text,
    /// Send these bytes instead.
    Bytes(Vec<u8>),
    /// Nothing is sent for this event (a release nobody asked for, a lone modifier).
    Nothing,
}

const ESC: u8 = 0x1b;

/// Encode one key event for a program whose negotiated flags are `flags`. `cursor_key_mode`
/// is DECCKM, which only shapes the legacy forms.
pub fn encode(ev: &KeyEvent, flags: u8, cursor_key_mode: bool) -> Encoded {
    let events = flags & flags::EVENT_TYPES != 0;
    let all = flags & flags::ALL_KEYS != 0;
    if ev.action == Action::Release && !events {
        return Encoded::Nothing;
    }
    let action = if events { ev.action } else { Action::Press };
    // The lock modifiers are reported only when every key is: with disambiguation alone a
    // Num Lock that is simply on would turn every arrow into `CSI 1;129A`.
    let m = if all { ev.mods } else { ev.mods & !mods::LOCKS };
    let has_text = ev.text.chars().next().is_some_and(|c| !c.is_control());

    if ev.key.is_modifier_class() {
        if !all {
            return Encoded::Nothing;
        }
        return Encoded::Bytes(csi(&Fields {
            number: functional_number(ev.key).0,
            mods: m,
            action,
            events,
            ..Fields::default()
        }));
    }
    // Text goes out as text; its key events exist only when every key is reported.
    if has_text && !all {
        return if action == Action::Release {
            Encoded::Nothing
        } else {
            Encoded::Text
        };
    }
    let disamb = flags & flags::DISAMBIGUATE != 0;
    let key = match ev.key {
        Key::Kp(kp) if !disamb && !all => legacy_keypad(kp),
        k => k,
    };
    match key {
        Key::Char(c) => text_key(c, ev, m, action, flags, has_text),
        k => functional_key(k, m, action, flags, cursor_key_mode),
    }
}

/// Without disambiguation the keypad reports as the keys it duplicates.
fn legacy_keypad(kp: Kp) -> Key {
    match kp {
        Kp::Digit(d) => Key::Char((b'0' + d.min(9)) as char),
        Kp::Decimal => Key::Char('.'),
        Kp::Divide => Key::Char('/'),
        Kp::Multiply => Key::Char('*'),
        Kp::Subtract => Key::Char('-'),
        Kp::Add => Key::Char('+'),
        Kp::Equal => Key::Char('='),
        Kp::Separator => Key::Char(','),
        Kp::Enter => Key::Enter,
        Kp::Left => Key::Left,
        Kp::Right => Key::Right,
        Kp::Up => Key::Up,
        Kp::Down => Key::Down,
        Kp::PageUp => Key::PageUp,
        Kp::PageDown => Key::PageDown,
        Kp::Home => Key::Home,
        Kp::End => Key::End,
        Kp::Insert => Key::Insert,
        Kp::Delete => Key::Delete,
        Kp::Begin => Key::Kp(Kp::Begin),
    }
}

fn text_key(c: char, ev: &KeyEvent, m: u8, action: Action, flags: u8, has_text: bool) -> Encoded {
    let events = flags & flags::EVENT_TYPES != 0;
    let all = flags & flags::ALL_KEYS != 0;
    let shift = m & mods::SHIFT != 0;
    let shifted = ev.shifted.filter(|&s| shift && s != c);
    let base = ev.base.filter(|&b| b != c);
    let alternates = flags & flags::ALTERNATE_KEYS != 0 && (shifted.is_some() || base.is_some());
    // Associated text is defined only on top of "report all keys".
    let text = if flags & flags::ASSOCIATED_TEXT != 0 && all && has_text {
        ev.text
    } else {
        ""
    };
    let add_action = events && action != Action::Press;
    if !add_action && !alternates && text.is_empty() {
        if m == 0 {
            if all {
                return Encoded::Bytes(csi(&Fields {
                    number: c as u32,
                    trailer: b'u',
                    ..Fields::default()
                }));
            }
            let mut buf = [0u8; 4];
            return Encoded::Bytes(c.encode_utf8(&mut buf).as_bytes().to_vec());
        }
        if flags & (flags::DISAMBIGUATE | flags::ALL_KEYS) == 0 {
            if let Some(bytes) = legacy_text_key(c, ev.shifted, m) {
                return Encoded::Bytes(bytes);
            }
            // A non-US layout's Ctrl/Alt chord falls back to the US key in the same place,
            // so Ctrl+С on a Russian layout is still Ctrl+C to a legacy program.
            let ctrl_alt = mods::CTRL | mods::ALT;
            if (m == mods::CTRL || m == mods::ALT || m == ctrl_alt)
                && !is_legacy_ascii(c)
                && let Some(b) = base.filter(|&b| is_legacy_ascii(b))
                && let Some(bytes) = legacy_text_key(b, None, m)
            {
                return Encoded::Bytes(bytes);
            }
        }
    }
    Encoded::Bytes(csi(&Fields {
        number: c as u32,
        shifted: if alternates { shifted } else { None },
        base: if alternates { base } else { None },
        mods: m,
        action,
        events,
        text,
        trailer: b'u',
    }))
}

/// The keys the legacy encoding covers: a US keyboard's printable ASCII.
fn is_legacy_ascii(c: char) -> bool {
    c == ' ' || (c.is_ascii_graphic() && !c.is_ascii_uppercase())
}

/// The C0 byte Ctrl turns an ASCII key into, or the key itself where Ctrl does nothing.
fn ctrl_byte(c: char) -> u8 {
    match c {
        ' ' | '@' | '2' => 0,
        'a'..='z' => c as u8 - b'a' + 1,
        '[' | '3' => 27,
        '\\' | '4' => 28,
        ']' | '5' => 29,
        '^' | '~' | '6' => 30,
        '_' | '/' | '7' => 31,
        '?' | '8' => 127,
        _ => c as u8,
    }
}

/// The legacy bytes for an ASCII text key under Shift / Alt / Ctrl / Shift+Alt / Ctrl+Alt, or
/// `None` for a combination the legacy encoding has no form for.
fn legacy_text_key(c: char, shifted: Option<char>, m: u8) -> Option<Vec<u8>> {
    if !is_legacy_ascii(c) && !shifted.is_some_and(is_legacy_ascii) {
        return None;
    }
    // Shift picks the shifted character; the legacy encoding covers only Shift, Alt, Ctrl,
    // Shift+Alt and Ctrl+Alt (and two Space chords). Anything else is CSI u.
    let key = match shifted.filter(|_| m & mods::SHIFT != 0) {
        Some(s) => s,
        None => c,
    };
    if !key.is_ascii() {
        return None;
    }
    let k = key as u8;
    let (shift, alt, ctrl) = (mods::SHIFT, mods::ALT, mods::CTRL);
    Some(match m {
        _ if m == shift => vec![k],
        _ if m == alt || m == shift | alt => vec![ESC, k],
        _ if m == ctrl => vec![ctrl_byte(c)],
        _ if m == ctrl | alt => vec![ESC, ctrl_byte(c)],
        _ if c == ' ' && m == ctrl | shift => vec![0],
        _ => return None,
    })
}

fn functional_key(key: Key, m: u8, action: Action, flags: u8, cursor_key_mode: bool) -> Encoded {
    let events = flags & flags::EVENT_TYPES != 0;
    let disamb = flags & flags::DISAMBIGUATE != 0;
    let all = flags & flags::ALL_KEYS != 0;
    let legacy = !events && !disamb && !all;
    let simple = |b: &[u8]| Encoded::Bytes(b.to_vec());

    if cursor_key_mode && legacy && m == 0 {
        let fin = match key {
            Key::Up => Some(b'A'),
            Key::Down => Some(b'B'),
            Key::Right => Some(b'C'),
            Key::Left => Some(b'D'),
            Key::Kp(Kp::Begin) => Some(b'E'),
            Key::End => Some(b'F'),
            Key::Home => Some(b'H'),
            _ => None,
        };
        if let Some(f) = fin {
            return simple(&[ESC, b'O', f]);
        }
    }
    if m == 0 {
        if key == Key::Escape && !disamb && !all {
            return simple(&[ESC]);
        }
        if legacy && let Key::F(n @ 1..=4) = key {
            return simple(&[ESC, b'O', b"PQRS"[n as usize - 1]]);
        }
        // Enter, Tab and Backspace keep their bytes (and have no release) unless every key
        // is reported — so `reset` can still be typed after a program dies in this mode.
        if !all {
            let legacy_bytes: Option<&[u8]> = match key {
                Key::Enter => Some(b"\r"),
                Key::Backspace => Some(b"\x7f"),
                Key::Tab => Some(b"\t"),
                _ => None,
            };
            if let Some(b) = legacy_bytes {
                return if action == Action::Release {
                    Encoded::Nothing
                } else {
                    simple(b)
                };
            }
        }
    } else if legacy && let Some(bytes) = legacy_modified_c0(key, m) {
        return Encoded::Bytes(bytes);
    }
    let (number, trailer) = match key {
        Key::Menu if legacy => (29, b'~'),
        k => functional_number(k),
    };
    Encoded::Bytes(csi(&Fields {
        number,
        mods: m,
        action,
        events,
        trailer,
        ..Fields::default()
    }))
}

/// Legacy Enter / Escape / Backspace / Tab with modifiers.
fn legacy_modified_c0(key: Key, m: u8) -> Option<Vec<u8>> {
    let alt = m & mods::ALT != 0;
    let mut out = if alt { vec![ESC] } else { vec![] };
    match key {
        Key::Enter => out.push(b'\r'),
        Key::Escape => out.push(ESC),
        Key::Backspace => out.push(if m & mods::CTRL != 0 { 0x08 } else { 0x7f }),
        Key::Tab if m & mods::SHIFT != 0 => out.extend_from_slice(b"\x1b[Z"),
        Key::Tab => out.push(b'\t'),
        _ => return None,
    }
    Some(out)
}

/// The protocol's number and final byte for a functional key.
fn functional_number(key: Key) -> (u32, u8) {
    let u = |n: u32| (n, b'u');
    let tilde = |n: u32| (n, b'~');
    match key {
        Key::Escape => u(27),
        Key::Enter => u(13),
        Key::Tab => u(9),
        Key::Backspace => u(127),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::Left => (1, b'D'),
        Key::Right => (1, b'C'),
        Key::Up => (1, b'A'),
        Key::Down => (1, b'B'),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::Home => (1, b'H'),
        Key::End => (1, b'F'),
        Key::CapsLock => u(57358),
        Key::ScrollLock => u(57359),
        Key::NumLock => u(57360),
        Key::PrintScreen => u(57361),
        Key::Pause => u(57362),
        Key::Menu => u(57363),
        Key::F(1) => (1, b'P'),
        Key::F(2) => (1, b'Q'),
        // Not `CSI R`: that is the cursor position report.
        Key::F(3) => tilde(13),
        Key::F(4) => (1, b'S'),
        Key::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]),
        Key::F(n @ 13..=35) => u(57376 + (n as u32 - 13)),
        Key::F(_) => u(0),
        Key::Kp(kp) => match kp {
            Kp::Digit(d) => u(57399 + d.min(9) as u32),
            Kp::Decimal => u(57409),
            Kp::Divide => u(57410),
            Kp::Multiply => u(57411),
            Kp::Subtract => u(57412),
            Kp::Add => u(57413),
            Kp::Enter => u(57414),
            Kp::Equal => u(57415),
            Kp::Separator => u(57416),
            Kp::Left => u(57417),
            Kp::Right => u(57418),
            Kp::Up => u(57419),
            Kp::Down => u(57420),
            Kp::PageUp => u(57421),
            Kp::PageDown => u(57422),
            Kp::Home => u(57423),
            Kp::End => u(57424),
            Kp::Insert => u(57425),
            Kp::Delete => u(57426),
            Kp::Begin => (1, b'E'),
        },
        Key::Modifier(m) => u(57441 + m as u32),
        Key::Code(n) => u(n),
        Key::Char(c) => u(c as u32),
    }
}

#[derive(Default)]
struct Fields<'a> {
    number: u32,
    shifted: Option<char>,
    base: Option<char>,
    mods: u8,
    action: Action,
    events: bool,
    text: &'a str,
    trailer: u8,
}

/// `CSI number[:shifted[:base]] [; mods[:event]] [; text] trailer`, with every optional field
/// left out when it carries its default.
fn csi(f: &Fields) -> Vec<u8> {
    let trailer = if f.trailer == 0 { b'u' } else { f.trailer };
    let add_action = f.events && f.action != Action::Press;
    let has_mods = f.mods != 0;
    let second = has_mods || add_action;
    let third = !f.text.is_empty();
    let alternates = f.shifted.is_some() || f.base.is_some();
    let mut s = String::from("\x1b[");
    if f.number != 1 || alternates || second || third {
        s.push_str(&f.number.to_string());
    }
    if alternates {
        s.push(':');
        if let Some(c) = f.shifted {
            s.push_str(&(c as u32).to_string());
        }
        if let Some(c) = f.base {
            s.push(':');
            s.push_str(&(c as u32).to_string());
        }
    }
    if second || third {
        s.push(';');
        if second {
            s.push_str(&(f.mods as u32 + 1).to_string());
            if add_action {
                s.push(':');
                s.push(match f.action {
                    Action::Press => '1',
                    Action::Repeat => '2',
                    Action::Release => '3',
                });
            }
        }
    }
    if third {
        let cps: Vec<String> = f
            .text
            .chars()
            .filter(|c| !c.is_control())
            .map(|c| (c as u32).to_string())
            .collect();
        s.push(';');
        s.push_str(&cps.join(":"));
    }
    let mut out = s.into_bytes();
    out.push(trailer);
    out
}

#[cfg(test)]
mod tests;
