//! Key events → the bytes a pane's program expects (decisions #155).
//!
//! A pane is its own terminal, so the encoding depends on what ITS program asked for: the
//! application-cursor mode (`DECCKM`, arrows as `ESC O A`) and the kitty keyboard protocol
//! (`CSI > flags u`, which is how Claude Code and nvim tell Shift+Enter from Enter). Without
//! the kitty protocol the xterm encoding is used, modifiers included (`Ctrl+Left` →
//! `ESC [ 1 ; 5 D`), which is what tmux sends with `extended-keys` off.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The input modes of the pane a key is for.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KeyModes {
    /// `DECCKM`: unmodified arrows / Home / End as `SS3` (`ESC O A`) instead of `CSI`.
    pub app_cursor: bool,
    /// kitty flag 1, "disambiguate escape codes": a key that is ambiguous in the legacy
    /// encoding (Esc, any Ctrl/Alt chord, a modified Enter/Tab/Backspace) is sent as
    /// `CSI code ; mods u`.
    pub kitty_disambiguate: bool,
    /// kitty flag 8, "report all keys as escape codes": plain text keys and Enter/Tab/
    /// Backspace too.
    pub kitty_all_keys: bool,
}

impl KeyModes {
    fn kitty(self) -> bool {
        self.kitty_disambiguate || self.kitty_all_keys
    }
}

/// The xterm/kitty modifier parameter: `1 + shift + 2·alt + 4·ctrl + 8·super`.
fn mod_param(mods: KeyModifiers) -> u8 {
    let mut m = 0;
    if mods.contains(KeyModifiers::SHIFT) {
        m |= 1;
    }
    if mods.contains(KeyModifiers::ALT) {
        m |= 2;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        m |= 4;
    }
    if mods.contains(KeyModifiers::SUPER) {
        m |= 8;
    }
    1 + m
}

/// The legacy control byte for `Ctrl+ch`, if there is one (xterm's table, digits
/// included: Ctrl+2 is NUL, Ctrl+3 ESC, Ctrl+4..7 the `0x1c`–`0x1f` group, Ctrl+8 DEL).
fn ctrl_byte(ch: char) -> Option<u8> {
    Some(match ch {
        ' ' | '@' | '2' => 0x00,
        'a'..='z' => ch as u8 & 0x1f,
        'A'..='Z' => ch.to_ascii_lowercase() as u8 & 0x1f,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '~' | '6' => 0x1e,
        '_' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// Undo crossterm's reading of a LEGACY terminal's C0 bytes `0x1c`–`0x1f` as `Ctrl+4`..`7`:
/// what was typed was Ctrl-\\ / Ctrl-] / Ctrl-^ / Ctrl-_ (SIGQUIT, telnet escape, undo). The
/// client applies it only when its terminal is NOT reporting kitty keys — there a `Ctrl+4`
/// is a real Ctrl+4 and must reach a kitty-mode pane as one.
pub fn unalias_legacy_c0(k: KeyEvent) -> KeyEvent {
    if !k.modifiers.contains(KeyModifiers::CONTROL) {
        return k;
    }
    let ch = match k.code {
        KeyCode::Char('4') => '\\',
        KeyCode::Char('5') => ']',
        KeyCode::Char('6') => '^',
        KeyCode::Char('7') => '_',
        _ => return k,
    };
    KeyEvent::new(KeyCode::Char(ch), k.modifiers)
}

/// The unshifted key of a shifted ASCII symbol on a US keyboard (`!` → `1`).
fn us_unshifted(ch: char) -> Option<char> {
    const SHIFTED: &str = "!@#$%^&*()_+{}|:\"<>?~";
    const BASE: &str = "1234567890-=[]\\;',./`";
    SHIFTED.find(ch).and_then(|i| BASE[i..].chars().next())
}

fn utf8(ch: char) -> Vec<u8> {
    let mut s = [0u8; 4];
    ch.encode_utf8(&mut s).as_bytes().to_vec()
}

/// `CSI code [; mods] u`.
fn csi_u(code: u32, mods: KeyModifiers) -> Vec<u8> {
    let m = mod_param(mods);
    if m == 1 {
        format!("\x1b[{code}u").into_bytes()
    } else {
        format!("\x1b[{code};{m}u").into_bytes()
    }
}

/// A cursor-style key with a final letter: arrows, Home, End, F1–F4.
fn letter_key(fin: char, mods: KeyModifiers, ss3: bool) -> Vec<u8> {
    let m = mod_param(mods);
    if m > 1 {
        format!("\x1b[1;{m}{fin}").into_bytes()
    } else if ss3 {
        format!("\x1bO{fin}").into_bytes()
    } else {
        format!("\x1b[{fin}").into_bytes()
    }
}

/// A `CSI n ~` key: Insert, Delete, PgUp, PgDn, F5–F12.
fn tilde_key(n: u8, mods: KeyModifiers) -> Vec<u8> {
    let m = mod_param(mods);
    if m > 1 {
        format!("\x1b[{n};{m}~").into_bytes()
    } else {
        format!("\x1b[{n}~").into_bytes()
    }
}

/// Encode one key for a pane in `modes`. `None` = a key with no encoding (media keys, a
/// lone modifier, F13+), which is dropped.
pub fn encode(code: KeyCode, mods: KeyModifiers, modes: KeyModes) -> Option<Vec<u8>> {
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let sup = mods.contains(KeyModifiers::SUPER);
    // Legacy: Alt is an ESC in front of the key's own bytes.
    let meta = |mut v: Vec<u8>| {
        if alt {
            v.insert(0, 0x1b);
        }
        v
    };
    let cursor_ss3 = modes.app_cursor && !modes.kitty();
    let modified = mods.intersects(
        KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER,
    );

    Some(match code {
        KeyCode::Char(ch) => {
            if modes.kitty() && (ctrl || alt || sup || modes.kitty_all_keys) {
                // kitty reports the UNSHIFTED key with Shift in the modifiers, which is also
                // how crossterm hands over a kitty-reporting client's chords (`Ctrl+Shift+1`
                // = `Char('1')` + CONTROL|SHIFT). A legacy client's capital letter folds back.
                let mut mods = mods;
                let base = if ch.is_ascii_uppercase() {
                    mods |= KeyModifiers::SHIFT;
                    ch.to_ascii_lowercase()
                } else if let Some(b) = us_unshifted(ch) {
                    // The client asks its terminal for alternate keys (so legacy panes get
                    // `ESC !`), and crossterm then hands over only the SHIFTED symbol. kitty
                    // wants the base key + Shift: recover it for the US layout, the one a
                    // `Ctrl+Shift+1`-style binding is written against.
                    mods |= KeyModifiers::SHIFT;
                    b
                } else {
                    ch
                };
                return Some(csi_u(base as u32, mods));
            }
            // A kitty-reporting client sends Shift+letter chords unshifted (`Alt+Shift+a` =
            // `Char('a')` + ALT|SHIFT); a legacy pane expects the capital.
            let ch = if mods.contains(KeyModifiers::SHIFT) {
                ch.to_ascii_uppercase()
            } else {
                ch
            };
            if ctrl {
                match ctrl_byte(ch) {
                    Some(b) => meta(vec![b]),
                    None => meta(utf8(ch)),
                }
            } else {
                meta(utf8(ch))
            }
        }
        KeyCode::Enter | KeyCode::Tab | KeyCode::Backspace | KeyCode::Esc | KeyCode::BackTab => {
            let (cp, legacy): (u32, &[u8]) = match code {
                KeyCode::Enter => (13, b"\r"),
                KeyCode::Tab => (9, b"\t"),
                KeyCode::BackTab => (9, b"\x1b[Z"),
                KeyCode::Backspace => (127, if ctrl { b"\x08" } else { b"\x7f" }),
                _ => (27, b"\x1b"),
            };
            let kitty_form = modes.kitty_all_keys
                || (modes.kitty_disambiguate && (modified || code == KeyCode::Esc));
            if kitty_form {
                let mut mods = mods;
                if code == KeyCode::BackTab {
                    mods |= KeyModifiers::SHIFT;
                }
                return Some(csi_u(cp, mods));
            }
            if code == KeyCode::BackTab {
                return Some(legacy.to_vec());
            }
            meta(legacy.to_vec())
        }
        // SS3 is a legacy-only form: under the kitty protocol these keys are always CSI.
        KeyCode::Left => letter_key('D', mods, cursor_ss3),
        KeyCode::Right => letter_key('C', mods, cursor_ss3),
        KeyCode::Up => letter_key('A', mods, cursor_ss3),
        KeyCode::Down => letter_key('B', mods, cursor_ss3),
        KeyCode::Home => letter_key('H', mods, cursor_ss3),
        KeyCode::End => letter_key('F', mods, cursor_ss3),
        // F1–F4 are SS3 keys in every legacy mode.
        // kitty moved F3 off `CSI R`, which is the cursor-position report.
        KeyCode::F(3) if modes.kitty() => tilde_key(13, mods),
        KeyCode::F(n @ 1..=4) => {
            letter_key(['P', 'Q', 'R', 'S'][n as usize - 1], mods, !modes.kitty())
        }
        KeyCode::F(n @ 5..=12) => tilde_key([15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5], mods),
        KeyCode::Insert => tilde_key(2, mods),
        KeyCode::Delete => tilde_key(3, mods),
        KeyCode::PageUp => tilde_key(5, mods),
        KeyCode::PageDown => tilde_key(6, mods),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: KeyModifiers = KeyModifiers::NONE;
    const S: KeyModifiers = KeyModifiers::SHIFT;
    const A: KeyModifiers = KeyModifiers::ALT;
    const C: KeyModifiers = KeyModifiers::CONTROL;

    fn legacy(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        encode(code, mods, KeyModes::default()).unwrap()
    }

    fn kitty(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        let m = KeyModes {
            kitty_disambiguate: true,
            ..KeyModes::default()
        };
        encode(code, mods, m).unwrap()
    }

    #[test]
    fn text_and_control_bytes() {
        assert_eq!(legacy(KeyCode::Char('a'), N), b"a");
        assert_eq!(legacy(KeyCode::Char('A'), S), b"A");
        assert_eq!(legacy(KeyCode::Char('한'), N), "한".as_bytes());
        assert_eq!(legacy(KeyCode::Char('c'), C), [0x03]);
        assert_eq!(legacy(KeyCode::Char('C'), C | S), [0x03]);
        assert_eq!(legacy(KeyCode::Char(' '), C), [0x00]);
        assert_eq!(legacy(KeyCode::Char('x'), A), b"\x1bx");
        assert_eq!(legacy(KeyCode::Char('c'), C | A), b"\x1b\x03");
    }

    #[test]
    fn the_c0_digits_a_legacy_terminal_reports_go_back_out_as_their_bytes() {
        // crossterm reads 0x1c..0x1f as Ctrl+4..7 — Ctrl-\ is SIGQUIT, Ctrl-_ is undo.
        assert_eq!(legacy(KeyCode::Char('4'), C), [0x1c]);
        assert_eq!(legacy(KeyCode::Char('5'), C), [0x1d]);
        assert_eq!(legacy(KeyCode::Char('6'), C), [0x1e]);
        assert_eq!(legacy(KeyCode::Char('7'), C), [0x1f]);
        assert_eq!(legacy(KeyCode::Char('\\'), C), [0x1c]);
        assert_eq!(legacy(KeyCode::Char(']'), C), [0x1d]);
        assert_eq!(legacy(KeyCode::Char('_'), C), [0x1f]);
        // In a kitty pane a Ctrl+digit is a Ctrl+digit: the client has already folded a
        // legacy terminal's aliases back (`unalias_legacy_c0`).
        assert_eq!(kitty(KeyCode::Char('3'), C), b"\x1b[51;5u");
        let k = |c| unalias_legacy_c0(KeyEvent::new(KeyCode::Char(c), C)).code;
        assert_eq!(k('4'), KeyCode::Char('\\'));
        assert_eq!(k('7'), KeyCode::Char('_'));
        assert_eq!(k('3'), KeyCode::Char('3'));
        assert_eq!(kitty(KeyCode::Char('\\'), C), b"\x1b[92;5u");
    }

    #[test]
    fn named_keys_carry_their_modifiers() {
        assert_eq!(legacy(KeyCode::Left, N), b"\x1b[D");
        assert_eq!(legacy(KeyCode::Left, C), b"\x1b[1;5D");
        assert_eq!(legacy(KeyCode::Right, A), b"\x1b[1;3C");
        assert_eq!(legacy(KeyCode::Up, S), b"\x1b[1;2A");
        assert_eq!(legacy(KeyCode::Home, N), b"\x1b[H");
        assert_eq!(legacy(KeyCode::End, C | S), b"\x1b[1;6F");
        assert_eq!(legacy(KeyCode::Delete, N), b"\x1b[3~");
        assert_eq!(legacy(KeyCode::PageUp, S), b"\x1b[5;2~");
        assert_eq!(legacy(KeyCode::BackTab, S), b"\x1b[Z");
        assert_eq!(legacy(KeyCode::Backspace, N), [0x7f]);
        assert_eq!(legacy(KeyCode::Backspace, C), [0x08]);
        assert_eq!(legacy(KeyCode::Backspace, A), b"\x1b\x7f");
        assert_eq!(legacy(KeyCode::Enter, A), b"\x1b\r");
    }

    #[test]
    fn function_keys() {
        assert_eq!(legacy(KeyCode::F(1), N), b"\x1bOP");
        assert_eq!(legacy(KeyCode::F(4), N), b"\x1bOS");
        assert_eq!(legacy(KeyCode::F(1), S), b"\x1b[1;2P");
        assert_eq!(legacy(KeyCode::F(5), N), b"\x1b[15~");
        assert_eq!(legacy(KeyCode::F(10), N), b"\x1b[21~");
        assert_eq!(legacy(KeyCode::F(12), C), b"\x1b[24;5~");
        assert_eq!(encode(KeyCode::F(13), N, KeyModes::default()), None);
    }

    #[test]
    fn application_cursor_mode_switches_unmodified_cursor_keys_to_ss3() {
        let m = KeyModes {
            app_cursor: true,
            ..KeyModes::default()
        };
        assert_eq!(encode(KeyCode::Up, N, m).unwrap(), b"\x1bOA");
        assert_eq!(encode(KeyCode::Home, N, m).unwrap(), b"\x1bOH");
        // A modified key keeps the CSI form (xterm does the same).
        assert_eq!(encode(KeyCode::Up, C, m).unwrap(), b"\x1b[1;5A");
        assert_eq!(encode(KeyCode::PageUp, N, m).unwrap(), b"\x1b[5~");
    }

    #[test]
    fn kitty_disambiguates_what_legacy_cannot() {
        // Shift+Enter: the whole reason (Claude Code's newline).
        assert_eq!(legacy(KeyCode::Enter, S), b"\r");
        assert_eq!(kitty(KeyCode::Enter, S), b"\x1b[13;2u");
        assert_eq!(kitty(KeyCode::Enter, C), b"\x1b[13;5u");
        assert_eq!(kitty(KeyCode::Enter, N), b"\r");
        assert_eq!(kitty(KeyCode::Tab, N), b"\t");
        assert_eq!(kitty(KeyCode::BackTab, S), b"\x1b[9;2u");
        assert_eq!(kitty(KeyCode::Esc, N), b"\x1b[27u");
        assert_eq!(kitty(KeyCode::Backspace, A), b"\x1b[127;3u");
        assert_eq!(kitty(KeyCode::Char('c'), C), b"\x1b[99;5u");
        assert_eq!(kitty(KeyCode::Char('C'), C | S), b"\x1b[99;6u");
        assert_eq!(kitty(KeyCode::Char('x'), A), b"\x1b[120;3u");
        assert_eq!(kitty(KeyCode::Char(' '), C), b"\x1b[32;5u");
        // Plain and shifted text stays text.
        assert_eq!(kitty(KeyCode::Char('a'), N), b"a");
        assert_eq!(kitty(KeyCode::Char('A'), S), b"A");
        assert_eq!(kitty(KeyCode::Char('!'), S), b"!");
        // A kitty-reporting client's chords arrive unshifted with Shift set; keep it.
        assert_eq!(kitty(KeyCode::Char('1'), C | S), b"\x1b[49;6u");
        // …and the same chord as crossterm delivers it once alternate keys are on.
        assert_eq!(kitty(KeyCode::Char('!'), C), b"\x1b[49;6u");
        assert_eq!(kitty(KeyCode::Char('>'), A), b"\x1b[46;4u");
        assert_eq!(kitty(KeyCode::Char('"'), C), b"\x1b[39;6u");
        assert_eq!(kitty(KeyCode::Char('a'), A | S), b"\x1b[97;4u");
        assert_eq!(legacy(KeyCode::Char('a'), A | S), b"\x1bA");
        // Named keys keep their legacy forms.
        assert_eq!(kitty(KeyCode::Left, C), b"\x1b[1;5D");
        assert_eq!(kitty(KeyCode::F(5), N), b"\x1b[15~");
        // No SS3 under the kitty protocol, application cursor mode or not.
        assert_eq!(kitty(KeyCode::F(1), N), b"\x1b[P");
        assert_eq!(kitty(KeyCode::F(3), N), b"\x1b[13~");
        assert_eq!(kitty(KeyCode::F(3), S), b"\x1b[13;2~");
        let m = KeyModes {
            app_cursor: true,
            kitty_disambiguate: true,
            ..KeyModes::default()
        };
        assert_eq!(encode(KeyCode::Up, N, m).unwrap(), b"\x1b[A");
    }

    #[test]
    fn kitty_report_all_keys_encodes_text_too() {
        let m = KeyModes {
            kitty_disambiguate: true,
            kitty_all_keys: true,
            ..KeyModes::default()
        };
        assert_eq!(encode(KeyCode::Char('a'), N, m).unwrap(), b"\x1b[97u");
        assert_eq!(encode(KeyCode::Char('A'), S, m).unwrap(), b"\x1b[97;2u");
        assert_eq!(encode(KeyCode::Enter, N, m).unwrap(), b"\x1b[13u");
    }
}
