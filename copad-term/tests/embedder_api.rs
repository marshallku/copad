//! The calls an app embedding copad-term uses beyond copad's own GUI: titles, bell, the child's
//! environment and pid, and teardown of the shell's session.
//!
//! `#[ignore]`d like `pty_round_trip.rs` (real shells): `cargo test -p copad-term -- --ignored`.
//! macOS only: the session hangup and `libc` (a macOS-only dependency here) are.
#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::thread::sleep;
use std::time::{Duration, Instant};

use copad_term::*;

fn spawn(set: &[&str], remove: &[&str]) -> *mut CopadHandle {
    let shell = CString::new("/bin/sh").unwrap();
    let set: Vec<CString> = set.iter().map(|s| CString::new(*s).unwrap()).collect();
    let remove: Vec<CString> = remove.iter().map(|s| CString::new(*s).unwrap()).collect();
    let set_ptrs: Vec<_> = set.iter().map(|s| s.as_ptr()).collect();
    let remove_ptrs: Vec<_> = remove.iter().map(|s| s.as_ptr()).collect();
    let handle = unsafe {
        copad_term_create_env(
            80,
            24,
            shell.as_ptr(),
            std::ptr::null(),
            set_ptrs.as_ptr(),
            set_ptrs.len(),
            remove_ptrs.as_ptr(),
            remove_ptrs.len(),
        )
    };
    assert!(!handle.is_null(), "shell spawn failed");
    handle
}

fn send(handle: *mut CopadHandle, text: &str) {
    unsafe { copad_term_input(handle, text.as_ptr(), text.len()) };
}

fn screen(handle: *mut CopadHandle) -> String {
    let snap = unsafe { copad_term_snapshot(handle) };
    assert!(!snap.is_null());
    let rows = unsafe { copad_snapshot_rows(snap) };
    let mut text = String::new();
    for row in 0..rows {
        let mut len = 0;
        let ptr = unsafe { copad_snapshot_row_utf8(snap, row, &mut len) };
        if !ptr.is_null() && len > 0 {
            text.push_str(&String::from_utf8_lossy(unsafe {
                std::slice::from_raw_parts(ptr, len)
            }));
        }
        text.push('\n');
    }
    unsafe { copad_snapshot_destroy(snap) };
    text
}

fn wait_until(what: &str, timeout_ms: u64, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        sleep(Duration::from_millis(20));
    }
}

/// The number after `marker=` on screen, once the shell printed it.
fn read_number(handle: *mut CopadHandle, marker: &str) -> i32 {
    let mut value = None;
    wait_until(marker, 3_000, || {
        value = screen(handle)
            .lines()
            .filter_map(|line| line.trim().strip_prefix(&format!("{marker}=")))
            .find_map(|rest| rest.trim().parse().ok());
        value.is_some()
    });
    value.unwrap()
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
#[ignore]
fn titles_and_bell_are_taken_once() {
    let handle = spawn(&[], &[]);
    send(handle, "printf '\\033]2;hello copad\\007'\n");
    let mut title: *mut CopadString = std::ptr::null_mut();
    wait_until(
        "title",
        3_000,
        || unsafe { copad_term_take_title(handle, &mut title) } == 1,
    );
    let mut len = 0;
    let bytes = unsafe { std::slice::from_raw_parts(copad_string_bytes(title, &mut len), len) };
    assert_eq!(bytes, b"hello copad");
    unsafe { copad_string_destroy(title) };
    assert_eq!(unsafe { copad_term_take_title(handle, &mut title) }, 0);

    send(handle, "printf '\\007'\n");
    wait_until("bell", 3_000, || unsafe { copad_term_take_bell(handle) });
    assert!(!unsafe { copad_term_take_bell(handle) });
    unsafe { copad_term_destroy(handle) };
}

#[test]
#[ignore]
fn the_child_gets_the_set_pairs_and_not_the_removed_keys() {
    // SAFETY: no other thread of this test binary reads the environment meanwhile.
    unsafe { std::env::set_var("COPAD_EMBED_DROPPED", "leak") };
    let handle = spawn(
        &["TERM_PROGRAM=AIDE", "EMBED_SET=1", "not-a-pair"],
        &["COPAD_EMBED_DROPPED"],
    );
    send(
        handle,
        "printf 'env=%s|%s|%s\\n' \"$TERM_PROGRAM\" \"$EMBED_SET\" \"${COPAD_EMBED_DROPPED-unset}\"\n",
    );
    wait_until("env line", 3_000, || {
        screen(handle).contains("env=AIDE|1|unset")
    });
    unsafe { copad_term_destroy(handle) };
}

#[test]
#[ignore]
fn the_pid_is_reported_until_the_shell_exits() {
    let handle = spawn(&[], &[]);
    let pid = unsafe { copad_term_child_pid(handle) };
    assert!(pid > 1);
    send(handle, "printf 'shell=%s\\n' $$\n");
    assert_eq!(read_number(handle, "shell"), pid);
    send(handle, "exit\n");
    wait_until(
        "exit",
        3_000,
        || unsafe { copad_term_child_pid(handle) } == -1,
    );
    unsafe { copad_term_destroy(handle) };
    assert!(copad_term_wait_reaps(3_000));
}

#[test]
#[ignore]
fn closing_hangs_up_the_session_but_nohup_jobs_survive() {
    let handle = spawn(&[], &[]);
    let shell = unsafe { copad_term_child_pid(handle) };
    send(handle, "set -m; sleep 41 & printf 'job=%s\\n' $!\n");
    let job = read_number(handle, "job");
    send(
        handle,
        "nohup sleep 42 >/dev/null 2>&1 & printf 'kept=%s\\n' $!\n",
    );
    let kept = read_number(handle, "kept");
    // The shell itself ignores SIGHUP: only the SIGKILL after the grace ends it.
    send(handle, "trap '' HUP; printf 'trapped=1\\n'\n");
    read_number(handle, "trapped");

    unsafe { copad_term_destroy(handle) };
    assert!(copad_term_wait_reaps(4_000), "the reaper didn't finish");
    assert!(!alive(shell), "the shell outlived the grace");
    wait_until("job hangup", 2_000, || !alive(job));
    assert!(alive(kept), "a nohup job was killed");
    unsafe { libc::kill(kept, libc::SIGKILL) };
}

#[test]
fn null_handles_are_safe() {
    let mut out: *mut CopadString = std::ptr::null_mut();
    unsafe {
        assert_eq!(copad_term_take_title(std::ptr::null_mut(), &mut out), 0);
        assert!(!copad_term_take_bell(std::ptr::null_mut()));
        assert_eq!(copad_term_child_pid(std::ptr::null_mut()), -1);
    }
}
