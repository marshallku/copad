//! `copad_term_create_mirror`: a terminal with no PTY that a host feeds (decisions #211).
//!
//! The PTY comparison is `#[ignore]`d because it spawns a child; run with
//! `cargo test -p copad-term --test mirror -- --ignored`.

use std::ffi::{CString, c_void};
use std::sync::Mutex;
use std::thread::sleep;
use std::time::{Duration, Instant};

use copad_term::*;

/// A program's output touching most of what a screen holds: SGR colours and styles, wide and
/// combining characters, cursor moves, a scroll region, the alternate screen, scrollback, an OSC
/// 8 link, a colour set, a synchronized update, and queries a mirror must not answer.
const STREAM: &[u8] = "plain \x1b[1;31mbold red\x1b[0m \x1b[38;2;10;20;30;48;5;200mtrue\x1b[0m\r\n\
e\u{301} a\u{308}\u{332} \u{1100}\u{1161}\u{11a8}! é 漢字 🙂 \x1b[4:3;58;5;3mcurly\x1b[0m\r\n\
\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\ tab\there\r\n\
\x1b[c\x1b[6n\x1b]11;?\x1b\\\x1b[>q\
\x1b]11;rgb:12/34/56\x1b\\\x1b]4;1;rgb:ab/cd/ef\x07\
line 4\r\nline 5\r\nline 6\r\nline 7\r\nline 8\r\nline 9\r\nline 10\r\nline 11\r\nline 12\r\n\
\x1b[3;6r\x1b[3;1Hregion\x1b[6;1H\n\nscrolled\x1b[r\
\x1b[?1049h\x1b[2J\x1b[Halt screen\x1b[?1049l\
\x1b[?2026h\x1b[10;1Hsynced\x1b[?2026l\
\x1b[8;3H\x1b[7minverse\x1b[0m\x1b[?25l\x1b[5 q\x1b[9;20HEND"
    .as_bytes();

const COLS: u16 = 40;
const ROWS: u16 = 10;

/// What a host can read off a terminal: every row's text and runs, the cursor, the live screen
/// and the scrollback.
#[derive(Debug, PartialEq)]
struct Screen {
    rows: Vec<(String, Vec<[u32; 10]>)>,
    cursor: (u16, u16, u8, u8),
    text: String,
    history: String,
}

fn take_string(s: *mut CopadString) -> String {
    assert!(!s.is_null());
    let mut len = 0;
    let ptr = unsafe { copad_string_bytes(s, &mut len) };
    let text = if len == 0 {
        String::new()
    } else {
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len) }).into_owned()
    };
    unsafe { copad_string_destroy(s) };
    text
}

fn screen(handle: *mut CopadHandle) -> Screen {
    let snap = unsafe { copad_term_snapshot(handle) };
    assert!(!snap.is_null());
    let rows = (0..unsafe { copad_snapshot_rows(snap) })
        .map(|row| {
            let mut len = 0;
            let ptr = unsafe { copad_snapshot_row_utf8(snap, row, &mut len) };
            let text = if len == 0 {
                String::new()
            } else {
                String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(ptr, len) })
                    .into_owned()
            };
            let mut runs = std::ptr::null();
            let n = unsafe { copad_snapshot_row_runs(snap, row, &mut runs) };
            let runs = if n == 0 {
                Vec::new()
            } else {
                unsafe { std::slice::from_raw_parts(runs, n) }
                    .iter()
                    .map(|r| {
                        [
                            r.start_col.into(),
                            r.end_col.into(),
                            r.utf8_offset,
                            r.utf8_len,
                            r.fg_rgba,
                            r.bg_rgba,
                            r.flags.into(),
                            r.underline_style.into(),
                            r.underline_color_rgba,
                            r.hyperlink_id,
                        ]
                    })
                    .collect()
            };
            (text, runs)
        })
        .collect();
    let mut c = CopadCursor {
        row: 0,
        col: 0,
        style: 0,
        blink: 0,
        _reserved: 0,
    };
    unsafe { copad_snapshot_cursor(snap, &mut c) };
    unsafe { copad_snapshot_destroy(snap) };
    Screen {
        rows,
        cursor: (c.row, c.col, c.style, c.blink),
        text: take_string(unsafe { copad_term_screen_text(handle) }),
        history: take_string(unsafe { copad_term_history(handle, 1_000) }),
    }
}

/// Everything a mirror handed its `write_cb`, one entry per call.
#[derive(Default)]
struct Written(Mutex<Vec<Vec<u8>>>);

unsafe extern "C" fn record(ctx: *mut c_void, bytes: *const u8, len: usize) {
    let written = unsafe { &*(ctx as *const Written) };
    let bytes = unsafe { std::slice::from_raw_parts(bytes, len) };
    written.0.lock().unwrap().push(bytes.to_vec());
}

fn mirror(written: &Written) -> *mut CopadHandle {
    let ctx = written as *const Written as *mut c_void;
    let handle = unsafe { copad_term_create_mirror(COLS, ROWS, Some(record), ctx) };
    assert!(!handle.is_null());
    handle
}

fn feed(handle: *mut CopadHandle, bytes: &[u8]) {
    unsafe { copad_term_feed(handle, bytes.as_ptr(), bytes.len()) };
}

#[test]
fn a_mirror_reads_like_a_terminal() {
    let written = Written::default();
    let h = mirror(&written);
    feed(h, STREAM);
    let s = screen(h);
    assert!(s.text.contains("END"), "{}", s.text);
    assert!(s.history.contains("plain bold red"), "{}", s.history);
    assert_eq!(s.cursor.0, 8);
    unsafe { copad_term_destroy(h) };
}

#[test]
fn a_read_may_end_anywhere() {
    let written = Written::default();
    let whole = mirror(&written);
    feed(whole, STREAM);
    let expected = screen(whole);
    unsafe { copad_term_destroy(whole) };
    for cut in 1..STREAM.len() {
        let split = mirror(&written);
        feed(split, &STREAM[..cut]);
        feed(split, &STREAM[cut..]);
        assert_eq!(screen(split), expected, "cut at byte {cut}");
        unsafe { copad_term_destroy(split) };
    }
}

#[test]
fn a_mirror_never_answers_the_program() {
    let written = Written::default();
    let h = mirror(&written);
    unsafe { copad_term_set_cell_pixels(h, 8, 16) };
    // DA1, DA2, DSR, CPR, DECRQM, XTVERSION, the kitty keyboard query, colour queries set and
    // unset, CSI 14/16/18 t, a kitty graphics query, an OSC 52 read, focus reports on.
    feed(
        h,
        b"\x1b[c\x1b[>c\x1b[5n\x1b[6n\x1b[?2004$p\x1b[>q\x1b[?u\x1b]11;?\x1b\\\
          \x1b]10;rgb:11/22/33\x1b\\\x1b]10;?\x1b\\\x1b]4;3;?\x07\x1b[14t\x1b[16t\x1b[18t\
          \x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b]52;c;?\x1b\\\x1b[?1004h",
    );
    unsafe { copad_term_set_focused(h, false) };
    unsafe { copad_term_set_focused(h, true) };
    assert!(written.0.lock().unwrap().is_empty());
    unsafe { copad_term_destroy(h) };
}

#[test]
fn typed_input_goes_to_write_cb() {
    let written = Written::default();
    let h = mirror(&written);
    unsafe { copad_term_input(h, b"ls\r".as_ptr(), 3) };
    // DECCKM from the stream changes what the encoder makes of an arrow key (macOS key 126).
    feed(h, b"\x1b[?1h");
    let input = CopadKeyInput {
        key_code: 126,
        mods: 0,
        action: 0,
        unmodified: std::ptr::null(),
        unmodified_len: 0,
        shifted: std::ptr::null(),
        shifted_len: 0,
        text: std::ptr::null(),
        text_len: 0,
    };
    let mut out = [0u8; 16];
    let mut len = 0;
    let kind = unsafe { copad_term_encode_key(h, &input, out.as_mut_ptr(), out.len(), &mut len) };
    assert_eq!(kind, COPAD_KEY_BYTES);
    unsafe { copad_term_input(h, out.as_ptr(), len) };
    assert_eq!(
        *written.0.lock().unwrap(),
        vec![b"ls\r".to_vec(), b"\x1bOA".to_vec()]
    );
    unsafe { copad_term_destroy(h) };
}

#[test]
fn resize_changes_the_grid_only() {
    let written = Written::default();
    let h = mirror(&written);
    feed(h, b"hello");
    unsafe { copad_term_resize(h, 20, 5) };
    let s = screen(h);
    assert_eq!(s.rows.len(), 5);
    assert!(s.rows[0].0.starts_with("hello"));
    assert_eq!(unsafe { copad_term_child_pid(h) }, -1);
    assert!(unsafe { copad_term_child_cwd(h) }.is_null());
    assert!(!unsafe { copad_term_take_child_exit(h) });
    assert!(written.0.lock().unwrap().is_empty());
    unsafe { copad_term_destroy(h) };
}

#[test]
fn a_synchronized_update_that_never_ends_still_shows() {
    let written = Written::default();
    let h = mirror(&written);
    feed(h, b"\x1b[?2026hpending");
    assert!(!screen(h).text.contains("pending"));
    sleep(Duration::from_millis(300));
    assert!(screen(h).text.contains("pending"));
    unsafe { copad_term_destroy(h) };
}

#[test]
fn feeding_a_null_handle_or_a_pty_terminal_is_a_no_op() {
    unsafe { copad_term_feed(std::ptr::null_mut(), b"x".as_ptr(), 1) };
    let null_cb = unsafe { copad_term_create_mirror(0, 0, None, std::ptr::null_mut()) };
    unsafe { copad_term_input(null_cb, b"x".as_ptr(), 1) };
    unsafe { copad_term_destroy(null_cb) };
}

/// The same bytes through a real PTY (a script that `cat`s them raw) and through a mirror end in
/// the same screen.
#[test]
#[ignore]
fn a_mirror_matches_a_pty_terminal() {
    let dir = std::env::temp_dir().join(format!("copad-mirror-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let stream = dir.join("stream");
    std::fs::write(&stream, STREAM).unwrap();
    let script = dir.join("cat-raw");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nstty raw -echo\ncat '{}'\nexec sleep 30\n",
            stream.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let shell = CString::new(script.to_str().unwrap()).unwrap();
    let pty = unsafe {
        copad_term_create_env(
            COLS,
            ROWS,
            shell.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
        )
    };
    assert!(!pty.is_null());
    let deadline = Instant::now() + Duration::from_secs(5);
    while !screen(pty).text.contains("END") {
        assert!(Instant::now() < deadline, "no END: {}", screen(pty).text);
        sleep(Duration::from_millis(20));
    }
    // A pending wrap or a last cursor move can trail END in the same read; let it land.
    sleep(Duration::from_millis(100));
    let from_pty = screen(pty);
    unsafe { copad_term_destroy(pty) };
    assert!(copad_term_wait_reaps(5_000));
    std::fs::remove_dir_all(&dir).unwrap();

    let written = Written::default();
    let m = mirror(&written);
    feed(m, STREAM);
    assert_eq!(screen(m), from_pty);
    unsafe { copad_term_destroy(m) };
}
