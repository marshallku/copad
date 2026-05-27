//! Bounded shell-exec helper for plugin manifest `[[commands]]` and
//! `[[modules]]`. Production callers go through `plugin.<name>.<cmd>`
//! and `_module.run` in the daemon dispatcher; the helper itself is
//! generic over `(dir, exec, stdin, env, timeout)`.
//!
//! Edge cases handled (verified by tests):
//!
//! - **PIPE buffer deadlock**: drain stdout and stderr on dedicated
//!   threads with `read_to_end` so a > 64 KB stderr flood does not
//!   block a single-stream drain.
//! - **stdin handoff**: write the payload on a dedicated thread so a
//!   non-reading child cannot block the daemon thread on a big payload.
//! - **Grandchild process leak on timeout**: `pre_exec(setsid)` puts
//!   the child in its own pgid; the timeout path SIGKILLs the whole
//!   group via `killpg` BEFORE `wait()` reaps the leader (no pid-reuse
//!   race).
//! - **Grandchild fd leak on success**: a backgrounded grandchild keeps
//!   the inherited pipe ends open. The success path uses
//!   `mpsc::sync_channel + recv_timeout(SUCCESS_DRAIN_BACKSTOP)` instead
//!   of `join()`: the daemon thread returns promptly, while the spawned
//!   I/O threads linger until their respective pipe sees EOF. Worst-case
//!   leak per pathological invocation is 3 threads: stdout reader, stderr
//!   reader, and the stdin writer (if a non-draining descendant keeps
//!   the read end open with a > pipe-buf payload still buffered). All
//!   exit once the grandchild dies.
//!   Crucially, we DO NOT `killpg` on the success path: `wait_timeout`
//!   has already reaped the leader and the pid is reusable, so killpg
//!   could target a wrong group.
//!   Note: bytes already buffered inside `read_to_end`'s thread-local
//!   `Vec` before the backstop fires are NOT returned to the caller —
//!   `read_to_end` waits for EOF and only sends to the channel once it
//!   returns. Output capture is best-effort under grandchild presence.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use wait_timeout::ChildExt;

/// Cap per-stream capture at 1 MiB. Plugin commands that need more
/// than this are misusing the surface — they should write to a file
/// and return the path, or stream via the panel.
const MAX_OUTPUT_BYTES: u64 = 1024 * 1024;

/// Success-path backstop for reader-channel drainage. The normal case
/// (child exits cleanly, no descendants) closes pipes at the same
/// instant as exit, so `read_to_end` EOFs and sends within microseconds.
/// The backstop only matters when a backgrounded grandchild inherits a
/// pipe end: rather than risk killpg targeting a reused pgid after the
/// leader was reaped (pid-reuse race), we time out the channel recv.
/// Bytes the reader buffered before the timeout are NOT returned — they
/// stay in the reader thread's local `Vec` until `read_to_end` finally
/// sees EOF when the grandchild dies. Output capture is therefore
/// best-effort in the pathological case.
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
    // SAFETY: setsid only manipulates the calling process's session/
    // process-group state and is async-signal-safe — valid inside
    // pre_exec's "between fork() and execve()" window. Putting the
    // child in its own process group lets the timeout path send
    // SIGKILL to the whole group via killpg, reaching grandchildren
    // (e.g. backgrounded subshells, trap-wrapped sleep).
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

    // Channels let the main thread time out on the reader collection
    // on the success path without joining the underlying JoinHandle
    // (which can block on a still-open grandchild pipe). On Disconnect,
    // both directions read EOF and the channels close — equivalent to
    // join semantics for our purposes.
    let (stdout_tx, stdout_rx) = mpsc::sync_channel::<Vec<u8>>(1);
    let (stderr_tx, stderr_rx) = mpsc::sync_channel::<Vec<u8>>(1);
    let (stdin_done_tx, stdin_done_rx) = mpsc::sync_channel::<()>(1);

    let stdin_payload_owned = stdin_payload.to_vec();
    std::thread::spawn(move || {
        if let Some(mut handle) = stdin_handle {
            let _ = handle.write_all(&stdin_payload_owned);
            // Drop closes the pipe — required for `cat`-style children
            // that loop until EOF.
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
            // wait_timeout has reaped the leader; the pid is now
            // reusable. We MUST NOT call killpg(child_pid) here —
            // the kernel could reassign that pgid to an unrelated
            // session and we'd SIGKILL an innocent process group.
            //
            // Bounded reader drainage instead: normal case (no
            // grandchild) the readers EOF instantly when the leader's
            // exit closes the pipes. Backstop fires only when a
            // grandchild kept the pipe alive past the leader's exit;
            // in that case `read_to_end` is still buffering on the
            // reader thread and has NOT sent to the channel, so we
            // get an empty `Vec` for that stream (NOT "whatever was
            // buffered" — those bytes stay inside the reader's local
            // Vec until the grandchild eventually dies). The reader
            // thread leaks for the lifetime of the grandchild.
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
            // wait_timeout has NOT reaped the leader (pid is still
            // ours, no reuse race); killpg targets the right group
            // and grandchildren that inherited the pgid from setsid
            // die too. THEN wait() reaps the leader.
            // SAFETY: killpg with SIGKILL is async-signal-safe; we
            // only care best-effort.
            let killed = unsafe { libc::killpg(child_pid, libc::SIGKILL) == 0 };
            let _ = child.wait();
            // After the group is dead, pipes close and the readers
            // EOF promptly. Bounded recv is a defensive backstop.
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
        // Both streams write > 64 KB (default pipe buffer) BEFORE the
        // child exits. Old single-stream-drain code would block here:
        // the un-drained stream fills its pipe, the child blocks on
        // write, the drained side never sees EOF, and we sit in
        // wait_timeout until the 5s ceiling.
        //
        // `head -c 100000 < /dev/urandom` is portable and produces
        // 100 000 bytes to its stdout. We split the stream: one copy
        // to stdout, another to stderr via >&2 with a re-read.
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
        // POSIX `kill(pid)` from std::process::Child sends SIGKILL on
        // Unix, which is uncatchable. Verify that a child that traps
        // SIGTERM still dies.
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
        // A child that never reads stdin used to block the daemon
        // thread on `write_all` until pipe drainage. With off-thread
        // stdin write the daemon thread continues to wait_timeout and
        // we observe a normal success path. Payload > 64 KB ensures
        // the pipe buffer would have backpressured a synchronous write.
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
        // The immediate shell exits 0 but spawns a `sleep 30`
        // grandchild that inherits the pipe write-end. The bounded
        // contract is: daemon thread MUST return within the 500 ms
        // SUCCESS_DRAIN_BACKSTOP after the leader's exit, regardless
        // of grandchild lifetime. Output capture is best-effort in
        // this pathological case: `read_to_end` waits for EOF, the
        // grandchild keeps the pipe open, so the reader's buffered
        // bytes never reach the channel. Acceptable cost — plugin
        // commands and statusbar modules should not background work.
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
        // Backgrounded grandchild (sleep) inherits the child's pgid
        // from setsid. A naive `kill(child_pid)` would only kill the
        // sh process and leave the sleep running, holding pipe fds
        // open and blocking reader joins. killpg targets the whole
        // group and the entire tree dies.
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
