//! Spawns `/bin/sh -c` rather than `$SHELL` so the test is reproducible.
//!
//! `#[ignore]`d because it spawns a child and blocks on PTY I/O. Run with
//! `cargo test -p copad-term -- --ignored`.

use std::ffi::CString;
use std::thread::sleep;
use std::time::{Duration, Instant};

use copad_term::*;

fn wait_for<F: Fn(&str) -> bool>(
    handle: *mut CopadHandle,
    predicate: F,
    deadline_ms: u64,
) -> Option<String> {
    let deadline = Instant::now() + Duration::from_millis(deadline_ms);
    loop {
        let snap = unsafe { copad_term_snapshot(handle) };
        if snap.is_null() {
            return None;
        }
        let row0 = unsafe { row_text(snap, 0) };
        unsafe { copad_snapshot_destroy(snap) };
        if predicate(&row0) {
            return Some(row0);
        }
        if Instant::now() >= deadline {
            return Some(row0);
        }
        sleep(Duration::from_millis(10));
    }
}

unsafe fn row_text(snap: *mut CopadSnapshot, row: u16) -> String {
    let mut len: usize = 0;
    let bytes_ptr = unsafe { copad_snapshot_row_utf8(snap, row, &mut len) };
    if bytes_ptr.is_null() || len == 0 {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(bytes_ptr, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
#[ignore]
fn printf_round_trip() {
    let shell = CString::new("/bin/sh").unwrap();
    let handle = unsafe {
        copad_term_create(
            80,
            24,
            shell.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert!(!handle.is_null(), "shell spawn failed");

    let cmd = b"printf 'copad-phase2-marker'\n";
    unsafe { copad_term_input(handle, cmd.as_ptr(), cmd.len()) };

    let result = wait_for(handle, |row| row.contains("copad-phase2-marker"), 2_000);
    unsafe { copad_term_destroy(handle) };

    let row = result.expect("snapshot returned NULL");
    assert!(
        row.contains("copad-phase2-marker"),
        "expected marker in row 0; got: {row:?}"
    );
}

#[test]
#[ignore]
fn resize_round_trip() {
    let shell = CString::new("/bin/sh").unwrap();
    let handle = unsafe {
        copad_term_create(
            80,
            24,
            shell.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert!(!handle.is_null());

    let snap = unsafe { copad_term_snapshot(handle) };
    assert_eq!(unsafe { copad_snapshot_cols(snap) }, 80);
    assert_eq!(unsafe { copad_snapshot_rows(snap) }, 24);
    unsafe { copad_snapshot_destroy(snap) };

    unsafe { copad_term_resize(handle, 100, 40) };

    let deadline = Instant::now() + Duration::from_millis(1_000);
    let (cols, rows) = loop {
        let snap = unsafe { copad_term_snapshot(handle) };
        let cols = unsafe { copad_snapshot_cols(snap) };
        let rows = unsafe { copad_snapshot_rows(snap) };
        unsafe { copad_snapshot_destroy(snap) };
        if (cols, rows) == (100, 40) || Instant::now() >= deadline {
            break (cols, rows);
        }
        sleep(Duration::from_millis(10));
    };

    unsafe { copad_term_destroy(handle) };
    assert_eq!(
        (cols, rows),
        (100, 40),
        "resize never reflected in snapshot"
    );
}

#[test]
fn null_destroy_no_op() {
    unsafe {
        copad_term_destroy(std::ptr::null_mut());
        copad_snapshot_destroy(std::ptr::null_mut());
    }
}

/// A shell that ignores SIGHUP must not block `copad_term_destroy` in `waitpid`: the app calls it
/// on the main thread.
#[test]
fn destroy_does_not_wait_for_a_shell_that_ignores_sighup() {
    let shell = CString::new("/bin/sh").unwrap();
    let handle = unsafe {
        copad_term_create(
            80,
            24,
            shell.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert!(!handle.is_null(), "shell spawn failed");

    let cmd = b"trap '' HUP; clear; printf 'hup-''ignored %s.' $$\n";
    unsafe { copad_term_input(handle, cmd.as_ptr(), cmd.len()) };
    let row =
        wait_for(handle, |row| row.contains("hup-ignored"), 5_000).expect("snapshot returned NULL");
    assert!(row.contains("hup-ignored"), "trap never ran; got: {row:?}");

    let start = Instant::now();
    unsafe { copad_term_destroy(handle) };
    let took = start.elapsed();
    assert!(
        took < Duration::from_millis(500),
        "destroy blocked for {took:?}"
    );

    let pid = row
        .split("hup-ignored ")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .expect("shell pid")
        .trim()
        .to_string();
    let deadline = Instant::now() + Duration::from_secs(5);
    while std::process::Command::new("/bin/kill")
        .args(["-0", &pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
    {
        assert!(
            Instant::now() < deadline,
            "shell {pid} survived its terminal"
        );
        sleep(Duration::from_millis(50));
    }
}
