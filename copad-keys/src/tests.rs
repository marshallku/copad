use super::*;
use flags::*;
use mods::*;

fn ev(key: Key, m: u8) -> KeyEvent<'static> {
    KeyEvent {
        key,
        shifted: None,
        base: None,
        mods: m,
        action: Action::Press,
        text: "",
    }
}

/// A US-layout text key the way a GUI hands it over: the unshifted character, its shifted
/// one, and the text it types (none under Ctrl/Alt).
fn us(c: char, m: u8) -> KeyEvent<'static> {
    let shifted = match c {
        'a'..='z' => c.to_ascii_uppercase(),
        '1' => '!',
        '3' => '#',
        ';' => ':',
        '=' => '+',
        _ => c,
    };
    let text: &'static str = if m & (CTRL | ALT | SUPER) != 0 {
        ""
    } else if m & SHIFT != 0 {
        Box::leak(shifted.to_string().into_boxed_str())
    } else {
        Box::leak(c.to_string().into_boxed_str())
    };
    KeyEvent {
        key: Key::Char(c),
        shifted: Some(shifted),
        base: Some(c),
        mods: m,
        action: Action::Press,
        text,
    }
}

fn bytes(e: &KeyEvent, flags: u8) -> Vec<u8> {
    match encode(e, flags, false) {
        Encoded::Bytes(b) => b,
        other => panic!("{e:?} under {flags}: expected bytes, got {other:?}"),
    }
}

fn s(e: &KeyEvent, flags: u8) -> String {
    String::from_utf8(bytes(e, flags)).unwrap()
}

fn with(mut e: KeyEvent<'static>, action: Action) -> KeyEvent<'static> {
    e.action = action;
    e
}

// ---- legacy (the spec's "Legacy text keys" and "C0 controls" tables) ----

#[test]
fn legacy_text_key_table() {
    // i: plain, shift, alt, ctrl, shift+alt, alt+ctrl, ctrl+shift
    let i = |m| {
        let mut e = us('i', m);
        e.text = "";
        e
    };
    assert_eq!(bytes(&i(SHIFT), 0), b"I");
    assert_eq!(bytes(&i(ALT), 0), b"\x1bi");
    assert_eq!(bytes(&i(CTRL), 0), b"\t");
    assert_eq!(bytes(&i(SHIFT | ALT), 0), b"\x1bI");
    assert_eq!(bytes(&i(ALT | CTRL), 0), b"\x1b\t");
    assert_eq!(s(&i(CTRL | SHIFT), 0), "\x1b[105;6u");
    let three = |m| {
        let mut e = us('3', m);
        e.text = "";
        e
    };
    assert_eq!(bytes(&three(SHIFT), 0), b"#");
    assert_eq!(bytes(&three(CTRL), 0), b"\x1b");
    assert_eq!(bytes(&three(ALT | CTRL), 0), b"\x1b\x1b");
    assert_eq!(s(&three(CTRL | SHIFT), 0), "\x1b[51;6u");
    let semi = |m| {
        let mut e = us(';', m);
        e.text = "";
        e
    };
    assert_eq!(bytes(&semi(SHIFT), 0), b":");
    assert_eq!(bytes(&semi(CTRL), 0), b";");
    assert_eq!(bytes(&semi(SHIFT | ALT), 0), b"\x1b:");
}

#[test]
fn legacy_c0_table() {
    let k = |key, m| bytes(&ev(key, m), 0);
    assert_eq!(k(Key::Enter, 0), b"\r");
    assert_eq!(k(Key::Enter, CTRL), b"\r");
    assert_eq!(k(Key::Enter, ALT), b"\x1b\r");
    assert_eq!(k(Key::Escape, 0), b"\x1b");
    assert_eq!(k(Key::Escape, ALT), b"\x1b\x1b");
    assert_eq!(k(Key::Backspace, 0), b"\x7f");
    assert_eq!(k(Key::Backspace, CTRL), b"\x08");
    assert_eq!(k(Key::Backspace, ALT | CTRL), b"\x1b\x08");
    assert_eq!(k(Key::Tab, 0), b"\t");
    assert_eq!(k(Key::Tab, SHIFT), b"\x1b[Z");
    assert_eq!(k(Key::Tab, ALT | SHIFT), b"\x1b\x1b[Z");
    let space = |m| {
        let mut e = us(' ', m);
        e.text = "";
        e
    };
    assert_eq!(bytes(&space(CTRL), 0), b"\x00");
    assert_eq!(bytes(&space(CTRL | SHIFT), 0), b"\x00");
    assert_eq!(bytes(&space(ALT | SHIFT), 0), b"\x1b ");
}

#[test]
fn legacy_functional_keys() {
    let k = |key, m| s(&ev(key, m), 0);
    assert_eq!(k(Key::Up, 0), "\x1b[A");
    assert_eq!(k(Key::Up, CTRL), "\x1b[1;5A");
    assert_eq!(k(Key::F(1), 0), "\x1bOP");
    assert_eq!(k(Key::F(1), SHIFT), "\x1b[1;2P");
    assert_eq!(k(Key::F(5), 0), "\x1b[15~");
    assert_eq!(k(Key::Delete, ALT), "\x1b[3;3~");
    assert_eq!(k(Key::Menu, 0), "\x1b[29~");
    // Cursor key mode: SS3 only unmodified.
    assert_eq!(
        encode(&ev(Key::Up, 0), 0, true),
        Encoded::Bytes(b"\x1bOA".to_vec())
    );
    assert_eq!(
        encode(&ev(Key::Up, SHIFT), 0, true),
        Encoded::Bytes(b"\x1b[1;2A".to_vec())
    );
    // Keypad keys report as the keys they duplicate.
    assert_eq!(k(Key::Kp(Kp::Enter), 0), "\r");
    assert_eq!(k(Key::Kp(Kp::Up), 0), "\x1b[A");
}

// ---- disambiguate (flag 1) ----

#[test]
fn disambiguate_reports_what_legacy_cannot() {
    let k = |key, m| s(&ev(key, m), DISAMBIGUATE);
    assert_eq!(k(Key::Escape, 0), "\x1b[27u");
    // Shift+Enter: the reason this exists for copad (Claude Code's newline).
    assert_eq!(k(Key::Enter, SHIFT), "\x1b[13;2u");
    assert_eq!(k(Key::Enter, CTRL), "\x1b[13;5u");
    assert_eq!(k(Key::Enter, ALT), "\x1b[13;3u");
    assert_eq!(k(Key::Tab, SHIFT), "\x1b[9;2u");
    assert_eq!(k(Key::Backspace, CTRL), "\x1b[127;5u");
    // Unmodified Enter/Tab/Backspace stay legacy so `reset` can be typed.
    assert_eq!(k(Key::Enter, 0), "\r");
    assert_eq!(k(Key::Tab, 0), "\t");
    assert_eq!(k(Key::Backspace, 0), "\x7f");
    assert_eq!(s(&us('c', CTRL), DISAMBIGUATE), "\x1b[99;5u");
    assert_eq!(s(&us('a', ALT), DISAMBIGUATE), "\x1b[97;3u");
    assert_eq!(s(&us('a', CTRL | SHIFT), DISAMBIGUATE), "\x1b[97;6u");
    assert_eq!(s(&us(' ', CTRL), DISAMBIGUATE), "\x1b[32;5u");
    // Text stays text.
    assert_eq!(encode(&us('a', 0), DISAMBIGUATE, false), Encoded::Text);
    assert_eq!(encode(&us('a', SHIFT), DISAMBIGUATE, false), Encoded::Text);
    // Functional keys keep their CSI forms, never SS3.
    assert_eq!(k(Key::Up, 0), "\x1b[A");
    assert_eq!(k(Key::Left, CTRL), "\x1b[1;5D");
    assert_eq!(k(Key::F(1), 0), "\x1b[P");
    assert_eq!(k(Key::F(3), 0), "\x1b[13~");
    assert_eq!(k(Key::F(5), SHIFT), "\x1b[15;2~");
    assert_eq!(k(Key::F(13), 0), "\x1b[57376u");
    assert_eq!(k(Key::Menu, 0), "\x1b[57363u");
    assert_eq!(
        encode(&ev(Key::Up, 0), DISAMBIGUATE, true),
        Encoded::Bytes(b"\x1b[A".to_vec())
    );
}

#[test]
fn disambiguate_reports_keypad_keys_as_their_own() {
    let k = |key| s(&ev(key, 0), DISAMBIGUATE);
    assert_eq!(k(Key::Kp(Kp::Enter)), "\x1b[57414u");
    assert_eq!(k(Key::Kp(Kp::Up)), "\x1b[57419u");
    assert_eq!(k(Key::Kp(Kp::Begin)), "\x1b[E");
    // A keypad digit that types (Num Lock on) is text.
    let mut five = ev(Key::Kp(Kp::Digit(5)), 0);
    five.text = "5";
    assert_eq!(encode(&five, DISAMBIGUATE, false), Encoded::Text);
}

#[test]
fn lock_modifiers_only_with_all_keys() {
    assert_eq!(s(&us('a', CTRL | CAPS_LOCK), DISAMBIGUATE), "\x1b[97;5u");
    assert_eq!(s(&ev(Key::Up, NUM_LOCK), DISAMBIGUATE), "\x1b[A");
    assert_eq!(
        s(&us('a', CTRL | CAPS_LOCK), DISAMBIGUATE | ALL_KEYS),
        "\x1b[97;69u"
    );
}

#[test]
fn modifier_keys_only_with_all_keys() {
    let shift = ev(Key::Modifier(Modifier::LeftShift), SHIFT);
    assert_eq!(encode(&shift, DISAMBIGUATE, false), Encoded::Nothing);
    assert_eq!(s(&shift, ALL_KEYS), "\x1b[57441;2u");
    let up = with(ev(Key::Modifier(Modifier::LeftShift), 0), Action::Release);
    assert_eq!(s(&up, ALL_KEYS | EVENT_TYPES), "\x1b[57441;1:3u");
    assert_eq!(
        s(&ev(Key::Modifier(Modifier::RightSuper), SUPER), ALL_KEYS),
        "\x1b[57450;9u"
    );
    assert_eq!(
        s(&ev(Key::Modifier(Modifier::IsoLevel5Shift), 0), ALL_KEYS),
        "\x1b[57454u"
    );
    assert_eq!(s(&ev(Key::CapsLock, CAPS_LOCK), ALL_KEYS), "\x1b[57358;65u");
}

// ---- event types (flag 2) ----

#[test]
fn event_types() {
    let f = DISAMBIGUATE | EVENT_TYPES;
    assert_eq!(s(&with(us('a', CTRL), Action::Repeat), f), "\x1b[97;5:2u");
    assert_eq!(s(&with(us('a', CTRL), Action::Release), f), "\x1b[97;5:3u");
    assert_eq!(s(&with(ev(Key::Up, 0), Action::Release), f), "\x1b[1;1:3A");
    assert_eq!(s(&with(ev(Key::Up, 0), Action::Press), f), "\x1b[A");
    assert_eq!(
        s(&with(ev(Key::Escape, 0), Action::Release), f),
        "\x1b[27;1:3u"
    );
    // Enter/Tab/Backspace have no release unless every key is reported.
    assert_eq!(
        encode(&with(ev(Key::Enter, 0), Action::Release), f, false),
        Encoded::Nothing
    );
    assert_eq!(
        s(&with(ev(Key::Enter, 0), Action::Release), f | ALL_KEYS),
        "\x1b[13;1:3u"
    );
    // A key that typed text has no release of its own.
    assert_eq!(
        encode(&with(us('a', 0), Action::Release), f, false),
        Encoded::Nothing
    );
    // Without flag 2: releases vanish and repeats are presses.
    assert_eq!(
        encode(&with(us('a', CTRL), Action::Release), DISAMBIGUATE, false),
        Encoded::Nothing
    );
    assert_eq!(
        s(&with(us('a', CTRL), Action::Repeat), DISAMBIGUATE),
        "\x1b[97;5u"
    );
}

// ---- alternate keys (flag 4) ----

#[test]
fn alternate_keys() {
    let f = DISAMBIGUATE | ALTERNATE_KEYS;
    assert_eq!(s(&us('a', CTRL | SHIFT), f), "\x1b[97:65;6u");
    // The shifted key only with Shift.
    assert_eq!(s(&us('a', CTRL), f), "\x1b[97;5u");
    // ctrl+С on a Russian layout carries the US `c` as its base layout key.
    let ru = KeyEvent {
        key: Key::Char('с'),
        shifted: Some('С'),
        base: Some('c'),
        mods: CTRL,
        action: Action::Press,
        text: "",
    };
    assert_eq!(s(&ru, f), "\x1b[1089::99;5u");
    // Alternates never decorate functional keys.
    assert_eq!(s(&ev(Key::Enter, SHIFT), f), "\x1b[13;2u");
    // Alternates alone do not turn a legacy key into CSI u... but a legacy chord on a
    // non-US layout falls back to the US key.
    assert_eq!(bytes(&ru, 0), b"\x03");
}

// ---- report all keys (flag 8) and associated text (flag 16) ----

#[test]
fn all_keys_and_text() {
    assert_eq!(s(&us('a', 0), ALL_KEYS), "\x1b[97u");
    assert_eq!(s(&us('a', SHIFT), ALL_KEYS), "\x1b[97;2u");
    assert_eq!(s(&ev(Key::Enter, 0), ALL_KEYS), "\x1b[13u");
    assert_eq!(s(&ev(Key::Tab, 0), ALL_KEYS), "\x1b[9u");
    assert_eq!(s(&ev(Key::Escape, 0), ALL_KEYS), "\x1b[27u");
    // The spec's example: shift+a -> CSI 97 ; 2 ; 65 u
    assert_eq!(
        s(&us('a', SHIFT), ALL_KEYS | ASSOCIATED_TEXT),
        "\x1b[97;2;65u"
    );
    assert_eq!(s(&us('a', 0), ALL_KEYS | ASSOCIATED_TEXT), "\x1b[97;;97u");
    // Text without "all keys" is undefined; it stays text.
    assert_eq!(encode(&us('a', 0), ASSOCIATED_TEXT, false), Encoded::Text);
}

#[test]
fn control_characters_are_not_text() {
    let mut e = ev(Key::Enter, 0);
    e.text = "\r";
    assert_eq!(s(&e, DISAMBIGUATE | ALL_KEYS), "\x1b[13u");
    let mut c = us('c', CTRL);
    c.text = "\x03";
    assert_eq!(s(&c, DISAMBIGUATE), "\x1b[99;5u");
}

// ---- the tracker ----

fn feed(t: &mut Tracker, input: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (mut out, mut replies) = (Vec::new(), Vec::new());
    t.feed(input, &mut out, &mut replies);
    (out, replies)
}

#[test]
fn tracker_push_pop_set_and_query() {
    let mut t = Tracker::new();
    let (out, rep) = feed(&mut t, b"a\x1b[?u\x1b[cb");
    assert_eq!(out, b"a\x1b[cb", "the query is taken out, DA1 is not");
    assert_eq!(rep, b"\x1b[?0u");
    feed(&mut t, b"\x1b[>1u");
    assert_eq!(t.flags(), 1);
    feed(&mut t, b"\x1b[>5u");
    assert_eq!(t.flags(), 5);
    feed(&mut t, b"\x1b[=2;2u");
    assert_eq!(t.flags(), 7);
    feed(&mut t, b"\x1b[=1;3u");
    assert_eq!(t.flags(), 6);
    feed(&mut t, b"\x1b[=8u");
    assert_eq!(t.flags(), 8);
    let (_, rep) = feed(&mut t, b"\x1b[?u");
    assert_eq!(rep, b"\x1b[?8u");
    feed(&mut t, b"\x1b[<u");
    assert_eq!(t.flags(), 1);
    feed(&mut t, b"\x1b[<5u");
    assert_eq!(t.flags(), 0, "popping past the bottom resets");
    // `CSI = flags u` on an empty stack sets the flags in force.
    feed(&mut t, b"\x1b[=3u");
    assert_eq!(t.flags(), 3);
    // A plain `CSI u` (SCORC) and `CSI s` are not ours.
    let (out, _) = feed(&mut t, b"\x1b[u\x1b[s");
    assert_eq!(out, b"\x1b[u\x1b[s");
}

#[test]
fn tracker_stack_is_capped() {
    let mut t = Tracker::new();
    for i in 0..40 {
        feed(&mut t, format!("\x1b[>{}u", i % 31).as_bytes());
    }
    feed(&mut t, b"\x1b[<15u");
    assert_ne!(t.flags(), 0, "16 entries kept");
    feed(&mut t, b"\x1b[<u");
    assert_eq!(t.flags(), 0);
}

#[test]
fn tracker_keeps_one_stack_per_screen() {
    let mut t = Tracker::new();
    feed(&mut t, b"\x1b[>1u");
    let (out, _) = feed(&mut t, b"\x1b[?1049h");
    assert_eq!(out, b"\x1b[?1049h", "mode changes pass through");
    assert_eq!(t.flags(), 0, "the alternate screen starts clean");
    feed(&mut t, b"\x1b[>11u");
    assert_eq!(t.flags(), 11);
    // Entering again while already there is not a transition.
    feed(&mut t, b"\x1b[?1049h\x1b[?47h");
    assert_eq!(t.flags(), 11);
    feed(&mut t, b"\x1b[?1049l");
    assert_eq!(t.flags(), 1);
    feed(&mut t, b"\x1b[?1047l");
    assert_eq!(t.flags(), 1, "leaving twice is not a transition either");
    // Several modes in one sequence.
    feed(&mut t, b"\x1b[?1;1049h");
    assert!(t.app_cursor());
    assert_eq!(t.flags(), 11);
    feed(&mut t, b"\x1b[?1l\x1b[?1049l");
    assert!(!t.app_cursor());
    assert_eq!(t.flags(), 1);
}

#[test]
fn tracker_resets() {
    let mut t = Tracker::new();
    feed(&mut t, b"\x1b[>1u\x1b[?1h");
    let (out, _) = feed(&mut t, b"\x1b[!p");
    assert_eq!(out, b"\x1b[!p");
    assert!(!t.app_cursor());
    assert_eq!(t.flags(), 1, "a soft reset leaves the keyboard flags");
    let (out, _) = feed(&mut t, b"\x1bc");
    assert_eq!(out, b"\x1bc");
    assert_eq!(t.flags(), 0);
}

#[test]
fn tracker_follows_sequences_split_across_reads() {
    let mut t = Tracker::new();
    let input = b"x\x1b[>1u\x1b[?u\x1b[31my\x1b]0;t\x07z";
    let (mut out, mut rep) = (Vec::new(), Vec::new());
    for b in input {
        t.feed(&[*b], &mut out, &mut rep);
    }
    assert_eq!(out, b"x\x1b[31my\x1b]0;t\x07z");
    assert_eq!(rep, b"\x1b[?1u");
    assert_eq!(t.flags(), 1);
}

#[test]
fn tracker_never_looks_inside_strings() {
    let mut t = Tracker::new();
    // OSC ends at BEL; what follows is live again.
    let (out, rep) = feed(&mut t, b"\x1b]2;\x1b[>1u\x07");
    // ESC inside the OSC ends it (as in VTE), so `ESC [ > 1 u` is a live sequence there —
    // the parser downstream sees the same thing.
    assert_eq!(out, b"\x1b]2;\x07");
    assert!(rep.is_empty());
    assert_eq!(t.flags(), 1);
    let mut t = Tracker::new();
    // A BEL does not end a DCS: the CSI-looking bytes stay data until ST.
    let (out, _) = feed(&mut t, b"\x1bPq\x07[>1u\x1b\\");
    assert_eq!(out, b"\x1bPq\x07[>1u\x1b\\");
    assert_eq!(t.flags(), 0);
    // APC (kitty graphics) payloads pass untouched.
    let (out, _) = feed(&mut t, b"\x1b_Ga=q;AAAA\x1b\\done");
    assert_eq!(out, b"\x1b_Ga=q;AAAA\x1b\\done");
    // C1 forms in UTF-8.
    let (out, rep) = feed(&mut t, "\u{9b}?u".as_bytes());
    assert!(out.is_empty());
    assert_eq!(rep, b"\x1b[?0u");
    let (out, _) = feed(&mut t, "\u{9d}0;\u{9b}>1u\u{9c}".as_bytes());
    assert_eq!(out, "\u{9d}0;\u{9b}>1u\u{9c}".as_bytes());
    assert_eq!(t.flags(), 0);
    // Ordinary UTF-8 text with a C2 lead byte.
    let (out, _) = feed(&mut t, "©é".as_bytes());
    assert_eq!(out, "©é".as_bytes());
}

#[test]
fn tracker_cancels_and_overlong_sequences_pass_through() {
    let mut t = Tracker::new();
    let (out, _) = feed(&mut t, b"\x1b[>1\x18u");
    assert_eq!(out, b"\x1b[>1\x18u");
    assert_eq!(t.flags(), 0);
    let long = format!("\x1b[>{}1u", "0".repeat(100));
    let (out, rep) = feed(&mut t, long.as_bytes());
    assert_eq!(out, long.as_bytes());
    assert!(rep.is_empty());
    assert_eq!(t.flags(), 0);
    // An ESC restarts: the abandoned prefix passes, the new sequence is read.
    let (out, _) = feed(&mut t, b"\x1b[>\x1b[>3u");
    assert_eq!(out, b"\x1b[>");
    assert_eq!(t.flags(), 3);
    // C0 controls inside a CSI are executed, the CSI goes on.
    let (out, _) = feed(&mut t, b"\x1b[<\x08u");
    assert_eq!(out, b"\x08");
    assert_eq!(t.flags(), 0);
    // Intermediates or sub-parameters make it someone else's sequence.
    let (out, _) = feed(&mut t, b"\x1b[>1:2u\x1b[> u");
    assert_eq!(out, b"\x1b[>1:2u\x1b[> u");
    assert_eq!(t.flags(), 0);
}
