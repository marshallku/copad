//! Bounded shell-exec helper for plugin manifest `[[commands]]` and `[[modules]]`.
//!
//! - stdout and stderr drain on dedicated threads, since a > 64 KB stderr flood would deadlock a
//!   single-stream drain.
//! - stdin is written on its own thread so a non-reading child cannot block the daemon thread.
//! - The child gets its own pgid (`pre_exec(setsid)`); on timeout the group is SIGKILLed via
//!   `killpg` BEFORE `wait()` reaps the leader, so there is no pid-reuse race.
//! - On success a backgrounded grandchild may keep the pipe ends open, so readers are collected
//!   with `recv_timeout(SUCCESS_DRAIN_BACKSTOP)` instead of `join()`. Up to 3 threads (stdout,
//!   stderr, stdin writer) linger until the grandchild dies. No `killpg` on this path: the leader
//!   is already reaped and its pgid reusable. Bytes `read_to_end` had buffered are lost, so
//!   capture is best-effort when a grandchild is present.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use wait_timeout::ChildExt;

/// Per-stream capture cap. Commands needing more should write a file and return the path.
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;

/// Success-path cap on waiting for the readers. Only matters when a backgrounded grandchild
/// holds a pipe end open; killpg is unsafe there since the leader is reaped. Output the reader
/// buffered before the cap is not returned (see module docs).
const SUCCESS_DRAIN_BACKSTOP: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct ShellOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug)]
pub enum ShellError {
    Spawn(String),
    Wait(String),
    Timeout { after: Duration, killed: bool },
    NonZero(ShellOutput),
}

pub fn spawn_plugin_shell(
    dir: &Path,
    exec: &str,
    stdin_payload: &[u8],
    env: &HashMap<String, String>,
    timeout: Duration,
) -> Result<ShellOutput, ShellError> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(exec)
        .current_dir(dir)
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: setsid is async-signal-safe, valid between fork() and execve(). The child's own
    // process group lets the timeout path killpg grandchildren (backgrounded subshells etc.).
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| ShellError::Spawn(e.to_string()))?;

    let child_pid = child.id() as libc::pid_t;
    let stdin_handle = child.stdin.take();
    let stdout_pipe = child.stdout.take().expect("piped stdout");
    let stderr_pipe = child.stderr.take().expect("piped stderr");

    // Channels let the success path time out on reader collection without joining a
    // JoinHandle that can block on a still-open grandchild pipe.
    let (stdout_tx, stdout_rx) = mpsc::sync_channel::<Vec<u8>>(1);
    let (stderr_tx, stderr_rx) = mpsc::sync_channel::<Vec<u8>>(1);
    let (stdin_done_tx, stdin_done_rx) = mpsc::sync_channel::<()>(1);

    let stdin_payload_owned = stdin_payload.to_vec();
    std::thread::spawn(move || {
        if let Some(mut handle) = stdin_handle {
            let _ = handle.write_all(&stdin_payload_owned);
            // Drop closes the pipe, required for children that read until EOF.
        }
        let _ = stdin_done_tx.send(());
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.take(MAX_OUTPUT_BYTES).read_to_end(&mut buf);
        let _ = stdout_tx.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.take(MAX_OUTPUT_BYTES).read_to_end(&mut buf);
        let _ = stderr_tx.send(buf);
    });

    let status_opt = child
        .wait_timeout(timeout)
        .map_err(|e| ShellError::Wait(e.to_string()))?;

    match status_opt {
        Some(status) => {
            // wait_timeout has reaped the leader, so the pid is reusable: MUST NOT killpg here, it
            // could SIGKILL an unrelated group. Readers get a bounded drain instead; if a
            // grandchild keeps the pipe open the stream comes back as an empty `Vec` (bytes stay
            // in the reader thread, which lives until the grandchild dies).
            let stdout_bytes = stdout_rx
                .recv_timeout(SUCCESS_DRAIN_BACKSTOP)
                .unwrap_or_default();
            let stderr_bytes = stderr_rx
                .recv_timeout(SUCCESS_DRAIN_BACKSTOP)
                .unwrap_or_default();
            let _ = stdin_done_rx.recv_timeout(Duration::from_millis(100));
            let output = ShellOutput {
                stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
                stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
                exit_code: status.code().unwrap_or(-1),
            };
            if status.success() {
                Ok(output)
            } else {
                Err(ShellError::NonZero(output))
            }
        }
        None => {
            // wait_timeout has NOT reaped the leader, so the pid is still ours and killpg hits the
            // right group (grandchildren inherited the pgid via setsid). Then wait() reaps it.
            let killed = unsafe { libc::killpg(child_pid, libc::SIGKILL) == 0 };
            let _ = child.wait();
            let _ = stdout_rx.recv_timeout(SUCCESS_DRAIN_BACKSTOP);
            let _ = stderr_rx.recv_timeout(SUCCESS_DRAIN_BACKSTOP);
            let _ = stdin_done_rx.recv_timeout(Duration::from_millis(100));
            Err(ShellError::Timeout {
                after: timeout,
                killed,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn empty_env() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn success_captures_stdout_and_exit_zero() {
        let out = spawn_plugin_shell(
            Path::new("/tmp"),
            "echo hello",
            b"",
            &empty_env(),
            Duration::from_secs(5),
        )
        .expect("success");
        assert_eq!(out.stdout.trim(), "hello");
        assert_eq!(out.exit_code, 0);
        assert!(out.stderr.is_empty());
    }

    #[test]
    fn stdin_payload_reaches_child() {
        let out = spawn_plugin_shell(
            Path::new("/tmp"),
            "cat",
            b"payload-x",
            &empty_env(),
            Duration::from_secs(5),
        )
        .expect("cat success");
        assert_eq!(out.stdout, "payload-x");
    }

    #[test]
    fn non_zero_exit_classifies_as_failure() {
        let err = spawn_plugin_shell(
            Path::new("/tmp"),
            "echo oops 1>&2; exit 7",
            b"",
            &empty_env(),
            Duration::from_secs(5),
        )
        .expect_err("must classify non-zero exit");
        match err {
            ShellError::NonZero(out) => {
                assert_eq!(out.exit_code, 7);
                assert!(
                    out.stderr.contains("oops"),
                    "stderr drained even on failure: {:?}",
                    out.stderr
                );
            }
            other => panic!("expected NonZero, got {other:?}"),
        }
    }

    #[test]
    fn stderr_flood_does_not_deadlock_stdout_capture() {
        // Both streams write > 64 KB before the child exits. A single-stream drain would block:
        // the un-drained pipe fills, the child stalls on write, and wait_timeout hits its ceiling.
        let exec = "head -c 100000 /dev/urandom > /tmp/copad-pipetest-$$ \
                    && cat /tmp/copad-pipetest-$$ \
                    && cat /tmp/copad-pipetest-$$ >&2 \
                    && rm -f /tmp/copad-pipetest-$$";
        let started = Instant::now();
        let out = spawn_plugin_shell(
            Path::new("/tmp"),
            exec,
            b"",
            &empty_env(),
            Duration::from_secs(5),
        )
        .expect("must complete without deadlock");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "stderr-flood should not push us toward the timeout window"
        );
        assert!(
            out.stdout.len() >= 100_000,
            "stdout flood drained ({} bytes)",
            out.stdout.len()
        );
        assert!(
            out.stderr.len() >= 100_000,
            "stderr flood drained ({} bytes)",
            out.stderr.len()
        );
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn timeout_kills_runaway_child() {
        let started = Instant::now();
        let err = spawn_plugin_shell(
            Path::new("/tmp"),
            "sleep 30",
            b"",
            &empty_env(),
            Duration::from_millis(200),
        )
        .expect_err("must time out");
        let elapsed = started.elapsed();
        match err {
            ShellError::Timeout { killed, .. } => assert!(killed, "kill must succeed"),
            other => panic!("expected Timeout, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(2),
            "kill+reap must complete promptly; took {elapsed:?}"
        );
    }

    #[test]
    fn ignore_sigterm_child_still_killed_via_sigkill() {
        let started = Instant::now();
        let err = spawn_plugin_shell(
            Path::new("/tmp"),
            "trap '' TERM; sleep 30",
            b"",
            &empty_env(),
            Duration::from_millis(200),
        )
        .expect_err("must time out");
        let elapsed = started.elapsed();
        match err {
            ShellError::Timeout { killed, .. } => assert!(killed),
            other => panic!("expected Timeout, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(2),
            "SIGKILL must reap a SIGTERM-trapping child promptly"
        );
    }

    #[test]
    fn large_stdin_payload_does_not_block_when_child_ignores_stdin() {
        // A child that never reads stdin must not block the daemon thread on `write_all`; the
        // payload exceeds the pipe buffer.
        let payload = vec![b'x'; 128 * 1024];
        let started = Instant::now();
        let out = spawn_plugin_shell(
            Path::new("/tmp"),
            "echo done",
            &payload,
            &empty_env(),
            Duration::from_secs(5),
        )
        .expect("must not block on stdin handoff");
        assert_eq!(out.stdout.trim(), "done");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "stdin-flood ignored child should not push toward timeout"
        );
    }

    #[test]
    fn success_path_bounded_even_with_backgrounded_grandchild() {
        // The shell exits 0 but leaves a `sleep 30` grandchild holding the pipe write-end. The
        // daemon thread must still return within SUCCESS_DRAIN_BACKSTOP; buffered output is lost.
        let started = Instant::now();
        let result = spawn_plugin_shell(
            Path::new("/tmp"),
            "sleep 30 & echo done",
            b"",
            &empty_env(),
            Duration::from_secs(5),
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "success path must return promptly despite backgrounded grandchild; took {elapsed:?}"
        );
        let out = result.expect("must classify as success");
        assert_eq!(out.exit_code, 0);
    }

    #[test]
    fn timeout_reaps_grandchild_in_same_process_group() {
        let started = Instant::now();
        let err = spawn_plugin_shell(
            Path::new("/tmp"),
            "sleep 30 & wait",
            b"",
            &empty_env(),
            Duration::from_millis(200),
        )
        .expect_err("must time out");
        let elapsed = started.elapsed();
        match err {
            ShellError::Timeout { killed, .. } => assert!(killed, "killpg must succeed"),
            other => panic!("expected Timeout, got {other:?}"),
        }
        assert!(
            elapsed < Duration::from_secs(2),
            "killpg must reap whole group promptly; took {elapsed:?}"
        );
    }

    #[test]
    fn env_reaches_child() {
        let mut env = HashMap::new();
        env.insert("COPAD_TEST_KEY".to_string(), "abc-123".to_string());
        let out = spawn_plugin_shell(
            Path::new("/tmp"),
            "printf %s \"$COPAD_TEST_KEY\"",
            b"",
            &env,
            Duration::from_secs(5),
        )
        .expect("success");
        assert_eq!(out.stdout, "abc-123");
    }
}
