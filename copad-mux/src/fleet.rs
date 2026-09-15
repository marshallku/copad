//! Reading a fleet of comux servers over plain OpenSSH.
//!
//! One comux server per machine, reached by running `comux <verb> --json` there over ordinary
//! `ssh`. No daemon-to-daemon protocol, no credentials stored anywhere in comux — a machine
//! entry holds only a name and an SSH destination, so authentication, jump hosts, keys and
//! multiplexing are whatever the user's `~/.ssh/config` already says. That is herdr's model
//! and it is the right one: the credential story is someone else's solved problem.
//!
//! **Scope.** This is the READ half — "what is every machine's agent doing" — which is the
//! question the rest of this session's work already answers locally (`list-agents`, #105;
//! `detail`, #111). Attaching to a remote pane is deliberately NOT here: rendering one needs
//! the per-pane semantic grid protocol that decision #66 defers, and faking it by proxying the
//! composed cell-diff would tie every machine's view to one client's size.
//!
//! **A machine that does not answer is REPORTED, never omitted.** In a fleet this is the whole
//! game: "no agents on build-box" and "could not reach build-box" look identical in a list
//! that drops failures, and the second is the one you need to act on.

use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Per-machine wall-clock budget. A stalled machine must not hold up the rest, so every query
/// runs on its own thread under this deadline (herdr's independent-reconnect property).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Most bytes accepted from one machine's reply. A fleet readout must not be a way for a
/// remote host — or a misconfigured one echoing a login script forever — to exhaust memory
/// here. Generous next to any real `list-agents` payload.
const MAX_REPLY_BYTES: u64 = 4 * 1024 * 1024;

/// One configured machine. Deliberately just a name and an SSH destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub name: String,
    /// Anything `ssh` accepts as a destination: `host`, `user@host`, or an alias from
    /// `~/.ssh/config`.
    pub ssh: String,
    /// `$COPAD_MUX_SOCK` to use on the far side, when that machine's server is not at the
    /// default path.
    pub socket: Option<String>,
}

/// What one machine answered.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// Its `list-agents --json` payload, verbatim.
    Agents(Vec<serde_json::Value>),
    /// It could not be reached, or could not be understood. Carries a short reason so the
    /// readout can SAY why rather than leaving a blank row.
    Unreachable(String),
}

/// The command run on the far side for a given verb.
///
/// `COPAD_MUX_SOCK` is exported rather than passed as a flag because that is the only way
/// comux accepts a socket path, and it has to survive the remote login shell — hence the
/// single-quoted assignment. The remote `comux` is invoked by bare name so the far side's
/// PATH decides which build answers, exactly as an interactive `ssh host comux` would.
pub fn remote_command(socket: Option<&str>, verb: &str) -> String {
    match socket {
        Some(s) => format!("COPAD_MUX_SOCK={} comux {verb} --json", shell_quote(s)),
        None => format!("comux {verb} --json"),
    }
}

/// POSIX single-quote one argument (the remote side hands it to a shell).
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// The `ssh` argument list for one machine.
///
/// `BatchMode=yes` is load-bearing: without it a machine whose key needs a passphrase, or whose
/// host key is unknown, PROMPTS — and since this runs detached from any terminal the query
/// would hang until the deadline instead of failing with a reason.
pub fn ssh_args(m: &Machine, verb: &str, timeout: Duration) -> Vec<String> {
    vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        format!("ConnectTimeout={}", timeout.as_secs().max(1)),
        m.ssh.clone(),
        remote_command(m.socket.as_deref(), verb),
    ]
}

/// Parse a remote `list-agents --json` payload.
///
/// An `ok: false` response, a missing `agents` key and unparseable output are all
/// [`Reply::Unreachable`] with a distinct reason: from here they are the same kind of event —
/// "that machine did not tell us what its agents are doing" — and the reason is what makes it
/// actionable.
pub fn parse_agents(stdout: &str, stderr: &str) -> Reply {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        let why = stderr.lines().next().unwrap_or("no output").trim();
        return Reply::Unreachable(clip(why));
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        // Almost always a login banner or an MOTD ahead of the JSON, which is worth saying
        // plainly — it is fixed on the far side, not here.
        return Reply::Unreachable(clip(&format!("unparseable reply: {}", clip(trimmed))));
    };
    if v.get("ok").and_then(|o| o.as_bool()) == Some(false) {
        let err = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("refused")
            .to_string();
        return Reply::Unreachable(clip(&err));
    }
    match v.get("agents").and_then(|a| a.as_array()) {
        Some(a) => Reply::Agents(a.clone()),
        // `agents` absent is NOT "no agents": an older comux, or a different verb entirely.
        // `Some([])` vs absent is the same distinction #105 draws on the local wire.
        None => Reply::Unreachable("reply carried no agent list".into()),
    }
}

/// Keep a reason short enough for one row.
fn clip(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= 80 {
        return one;
    }
    one.chars().take(79).collect::<String>() + "…"
}

/// Query every machine CONCURRENTLY and collect what each said.
///
/// Returned in configured order (a `BTreeMap` by name), not completion order, so the readout
/// is stable between runs — a list that reshuffles by latency is unreadable.
pub fn query_all(machines: &[Machine], verb: &str, timeout: Duration) -> BTreeMap<String, Reply> {
    let handles: Vec<_> = machines
        .iter()
        .cloned()
        .map(|m| {
            let verb = verb.to_string();
            std::thread::spawn(move || {
                let reply = query_one(&m, &verb, timeout);
                (m.name, reply)
            })
        })
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok()).collect()
}

/// Query one machine. Never panics and never blocks past `timeout`.
fn query_one(m: &Machine, verb: &str, timeout: Duration) -> Reply {
    // `ssh` honours ConnectTimeout for the CONNECT only; a machine that connects and then
    // stalls needs our own deadline, so the child is waited on with one and killed past it.
    let child = Command::new("ssh")
        .args(ssh_args(m, verb, timeout))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return Reply::Unreachable(clip(&format!("could not run ssh: {e}"))),
    };
    // BOTH pipes must be drained WHILE we wait, on their own threads.
    //
    // The first version polled `try_wait()` and only called `wait_with_output()` afterwards.
    // A pipe holds ~64 KiB; once the child fills one it blocks in `write` and can never exit,
    // so `try_wait` never reports it done, and at the deadline we killed a perfectly reachable
    // machine and reported "timed out". It failed exactly where the feature earns its keep:
    // the more agents a machine has, the bigger its reply and the surer the deadlock.
    // BOTH pipes are drained WHILE we wait, on their own threads, and collected over a CHANNEL
    // rather than by joining.
    //
    // Joining is not safe here, and "we killed the child so the readers see EOF" is wrong:
    // killing `ssh` does not close a pipe write end that its DESCENDANTS inherited. An ordinary
    // `ProxyCommand` keeps stderr open, so the reader stays blocked after the child is reaped —
    // measured at 2.8s against `ProxyCommand=sleep 3`, and unbounded against a proxy that never
    // exits. A join there would hold up the whole fleet, defeating the one property this module
    // promises.
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    // Each reader appends into a SHARED buffer as bytes arrive and signals completion on the
    // channel. Two different needs, hence two mechanisms:
    //
    // * the channel is how the normal path knows both pipes reached EOF;
    // * the shared buffer is how the TIMEOUT path still gets what the machine managed to say.
    //   Sending only the finished buffer would strand it: a proxy that prints
    //   "Permission denied" and then hangs holds stderr open, so the reader never completes and
    //   the reason dies inside an abandoned thread while we report a bare deadline.
    let so = Arc::new(Mutex::new(Vec::<u8>::new()));
    let se = Arc::new(Mutex::new(Vec::<u8>::new()));
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let (o_pipe, o_sink, o_tx) = (out.take(), so.clone(), tx.clone());
    std::thread::spawn(move || {
        if let Some(p) = o_pipe {
            read_into(p, &o_sink);
        }
        let _ = o_tx.send(());
    });
    let (e_pipe, e_sink) = (err.take(), se.clone());
    std::thread::spawn(move || {
        if let Some(p) = e_pipe {
            read_into(p, &e_sink);
        }
        let _ = tx.send(());
    });

    let deadline = std::time::Instant::now() + timeout;
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(_)) => break false,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Reply::Unreachable(clip(&format!("ssh failed: {e}")));
            }
        }
    };
    // Wait for both readers within the REMAINING budget. One still blocked on a pipe its
    // descendant inherited is abandoned, not waited on — it holds an fd in a CLI process that
    // is about to exit. (If `fleet` ever moves into the long-lived server, this is the line
    // that has to become a process-group teardown.)
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if rx
            .recv_timeout(left.max(Duration::from_millis(50)))
            .is_err()
        {
            break;
        }
    }
    // Decode ONCE, over the whole snapshot. Decoding each 8 KiB read on its own would mangle
    // any multibyte character that happened to straddle a read boundary — `é` arriving as
    // `C3` then `A9` becomes `??`, and the JSON still parses, so a corrupted agent detail would
    // sail through silently. Agent details here are routinely non-ASCII.
    let take = |b: &Arc<Mutex<Vec<u8>>>| {
        b.lock()
            .map(|g| String::from_utf8_lossy(&g).into_owned())
            .unwrap_or_default()
    };
    let stdout = take(&so);
    let stderr = take(&se);
    if timed_out {
        // Report the timeout, but keep whatever the machine managed to say: a truncated stderr
        // ("Permission denied", a banner) is usually the actual reason, and it is more useful
        // than the deadline we happened to pick.
        let why = stderr
            .lines()
            .next()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| format!("timed out after {}s: {l}", timeout.as_secs()))
            .unwrap_or_else(|| format!("timed out after {}s", timeout.as_secs()));
        return Reply::Unreachable(clip(&why));
    }
    parse_agents(&stdout, &stderr)
}

/// Read a child pipe to EOF, appending RAW BYTES into `sink` as they arrive so a caller that
/// gives up at its deadline still sees what was said. Bytes, not text: decoding per read would
/// corrupt any multibyte character split across a read boundary. Bounded by
/// [`MAX_REPLY_BYTES`], so a machine that floods us — or echoes a login script forever — cannot
/// exhaust memory here.
fn read_into(mut r: impl std::io::Read, sink: &Arc<Mutex<Vec<u8>>>) {
    let mut buf = [0u8; 8192];
    let mut total: u64 = 0;
    while total < MAX_REPLY_BYTES {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                total += n as u64;
                if let Ok(mut g) = sink.lock() {
                    g.extend_from_slice(&buf[..n]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> Machine {
        Machine {
            name: "build".into(),
            ssh: "me@build.local".into(),
            socket: None,
        }
    }

    #[test]
    fn ssh_is_never_allowed_to_prompt() {
        let a = ssh_args(&m(), "list-agents", Duration::from_secs(7));
        // Without BatchMode a machine with an unknown host key or a passphrased key PROMPTS,
        // and since this runs detached the query hangs to the deadline instead of failing
        // with a reason the user can act on.
        assert!(a.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));
        assert!(a.contains(&"ConnectTimeout=7".to_string()));
        assert_eq!(a[a.len() - 2], "me@build.local");
        assert_eq!(a[a.len() - 1], "comux list-agents --json");
    }

    #[test]
    fn a_custom_socket_survives_the_remote_login_shell() {
        let mut mm = m();
        mm.socket = Some("/tmp/it's here/sock".into());
        let cmd = remote_command(mm.socket.as_deref(), "list-agents");
        assert_eq!(
            cmd, r#"COPAD_MUX_SOCK='/tmp/it'\''s here/sock' comux list-agents --json"#,
            "a socket path with a quote or a space must not split into extra words"
        );
    }

    #[test]
    fn a_machine_that_says_nothing_is_unreachable_with_a_reason() {
        // The case the whole module exists for: silence must never render as "no agents".
        assert_eq!(
            parse_agents(
                "",
                "ssh: connect to host build.local port 22: No route to host\n"
            ),
            Reply::Unreachable("ssh: connect to host build.local port 22: No route to host".into())
        );
        assert_eq!(
            parse_agents("   \n", ""),
            Reply::Unreachable("no output".into())
        );
    }

    #[test]
    fn an_empty_agent_list_is_an_answer_but_a_missing_one_is_not() {
        assert_eq!(
            parse_agents(r#"{"ok":true,"agents":[]}"#, ""),
            Reply::Agents(vec![])
        );
        // No `agents` key at all means an older comux or a different verb — NOT a machine
        // sitting idle. Same distinction the local wire draws (#105).
        match parse_agents(r#"{"ok":true}"#, "") {
            Reply::Unreachable(why) => assert!(why.contains("no agent list"), "{why}"),
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    #[test]
    fn a_login_banner_ahead_of_the_json_says_so() {
        match parse_agents("Welcome to build.local!\n{\"ok\":true,\"agents\":[]}", "") {
            Reply::Unreachable(why) => assert!(why.contains("unparseable"), "{why}"),
            other => panic!("a banner must not be mistaken for a reply: {other:?}"),
        }
    }

    #[test]
    fn a_refusal_carries_the_far_sides_reason() {
        match parse_agents(r#"{"ok":false,"error":"no server running"}"#, "") {
            Reply::Unreachable(why) => assert_eq!(why, "no server running"),
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }

    #[test]
    fn a_long_reason_is_clipped_to_one_row() {
        let long = "x".repeat(500);
        match parse_agents("", &long) {
            Reply::Unreachable(why) => {
                assert_eq!(why.chars().count(), 80);
                assert!(why.ends_with('…'));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_multibyte_reply_survives_being_split_across_reads() {
        // `read_into` fills an 8 KiB buffer, so a character can straddle two reads. Decoding
        // each read on its own turns a split `한` into replacement characters — and the JSON
        // still parses, so a corrupted agent detail would sail through silently. Agent details
        // here are routinely non-ASCII.
        //
        // Driven through a real pipe with a writer that pauses MID-CHARACTER, because the bug
        // is invisible to any fixture that hands the reader whole strings.
        use std::io::Write;
        let detail = "테스트를 실행하는 중".repeat(400); // comfortably over one read
        let payload = serde_json::json!({
            "ok": true,
            "agents": [{ "tool": "codex", "status": "working", "detail": detail }],
        })
        .to_string();

        let (r, mut w) = std::io::pipe().expect("pipe");
        let bytes = payload.clone().into_bytes();
        let writer = std::thread::spawn(move || {
            // Split at a byte offset that lands inside a multibyte character.
            let cut = bytes
                .iter()
                .position(|b| *b >= 0x80)
                .map(|i| i + 1)
                .unwrap_or(1);
            let _ = w.write_all(&bytes[..cut]);
            let _ = w.flush();
            std::thread::sleep(Duration::from_millis(40));
            let _ = w.write_all(&bytes[cut..]);
        });
        let sink = Arc::new(Mutex::new(Vec::<u8>::new()));
        read_into(r, &sink); // the REAL reader, so a regression in it fails here
        writer.join().unwrap();
        let text = String::from_utf8_lossy(&sink.lock().unwrap()).into_owned();
        assert!(
            !text.contains('\u{FFFD}'),
            "a character split across reads was replaced"
        );
        let Reply::Agents(a) = parse_agents(&text, "") else {
            panic!("expected agents from {}", &text[..60.min(text.len())])
        };
        assert_eq!(a[0]["detail"].as_str().unwrap(), detail);
    }

    #[test]
    fn agents_are_returned_verbatim() {
        let Reply::Agents(a) = parse_agents(
            r#"{"ok":true,"agents":[{"tool":"codex","status":"blocked"}]}"#,
            "",
        ) else {
            panic!("expected agents")
        };
        assert_eq!(a.len(), 1);
        assert_eq!(a[0]["tool"], "codex");
        assert_eq!(a[0]["status"], "blocked");
    }
}
