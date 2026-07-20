//! `coctl agent status` — a thin, LOCAL passthrough to `tmx agents --json`.
//!
//! `tmx` (≥1.1) is the ecosystem's source of truth for Claude/Codex process
//! classification (process-tree walk, zombie filtering, attention queue) — the
//! web-bridge already shells out to it rather than re-deriving. We do the same
//! so a tmux `status-right` can show live agent state next to `coctl usage`.
//!
//! The subprocess is TIME-BOUNDED (codex review C3): a wedged `tmx`/tmux must
//! never hang a 15s-cadence status-line call, so we kill it past a short budget
//! and degrade to an empty/"unavailable" readout (exit 0 — a status bar widget
//! should never error out the bar).

use serde::Deserialize;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

const TMX_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Deserialize, Default)]
struct Snapshot {
    #[serde(default)]
    agents: Vec<Agent>,
}

#[derive(Deserialize, Clone)]
struct Agent {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    repo_name: String,
}

/// Run `tmx agents --json`, bounded by `TMX_TIMEOUT`. `None` = not installed,
/// timed out, or empty output (all handled as "unavailable" by the caller).
///
/// The MAIN thread's wait is bounded by `rx.recv_timeout` — never by a thread
/// `join()`. A separate reader drains stdout concurrently (so a full pipe can't
/// wedge `tmx` before exit) and is DETACHED: if a lingering descendant keeps
/// the stdout write-end open past `tmx`'s own exit/kill, the reader may block,
/// but we never wait on it, so the status bar can't hang (codex R3/C1). On
/// timeout we kill+reap `tmx`; the orphaned reader unblocks when the pipe
/// finally closes, or exits with the process.
fn run_tmx(timeout: Duration) -> Option<String> {
    let mut child = Command::new("tmx")
        .args(["agents", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let mut out = stdout;
        let _ = out.read_to_string(&mut s);
        let _ = tx.send(s); // ignored if the receiver already gave up
    });

    match rx.recv_timeout(timeout) {
        Ok(s) => {
            // Full stdout received (EOF) → tmx is done. Validity is decided by
            // the caller's JSON parse, so exit status isn't consulted.
            reap_detached(child);
            (!s.trim().is_empty()).then_some(s)
        }
        Err(_) => {
            reap_detached(child);
            None
        }
    }
}

/// Kill `child` and reap it on a THROWAWAY thread, so the caller returns within
/// its `recv_timeout` bound even if the process lingers after SIGKILL (e.g.
/// wedged in uninterruptible I/O). Only the detached thread ever blocks on
/// `wait()` — never the status-bar path (codex R5/C1).
fn reap_detached(mut child: std::process::Child) {
    let _ = child.kill();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// The supervision-relevant agents: Claude/Codex only (shell/other rows exist
/// in the snapshot but aren't "agents" for a status readout — codex I1).
fn is_agent(a: &Agent) -> bool {
    a.kind == "claude" || a.kind == "codex"
}

/// Collapse control chars (newlines, tabs, …) to spaces so a repo name / cwd —
/// which the filesystem allows to contain almost any byte — can't break the
/// `--oneline` single-line guarantee for a tmux `status-right`.
fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cleaned.trim().to_string()
}

fn repo_label(a: &Agent) -> String {
    let raw = if !a.repo_name.is_empty() {
        a.repo_name.clone()
    } else {
        std::path::Path::new(&a.cwd)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string()
    };
    sanitize(&raw)
}

/// Normalize `tmx`'s status vocabulary (`working` / `awaiting-decision` /
/// `ready` / `idle`, and whatever it grows next) into the three states a
/// status bar cares about. Substring-matched so a renamed variant
/// (`waiting-for-input`, `active`, …) still lands in the right bucket instead
/// of silently counting as idle.
#[derive(PartialEq)]
enum Class {
    Busy,
    Attention,
    Idle,
}

fn classify(status: &str) -> Class {
    let s = status.to_ascii_lowercase();
    // `background` is tmx's status for a LIVE codex background job (kind codex,
    // pane null) — active work, so it must bucket Busy, not Idle.
    if s.contains("work")
        || s == "busy"
        || s.contains("active")
        || s.contains("run")
        || s.contains("background")
    {
        Class::Busy
    } else if s.contains("await")
        || s.contains("wait")
        || s.contains("decision")
        || s.contains("attention")
        || s.contains("block")
    {
        Class::Attention
    } else {
        Class::Idle
    }
}

fn oneline(agents: &[Agent]) -> String {
    if agents.is_empty() {
        return "no agents".to_string();
    }
    let busy = agents
        .iter()
        .filter(|a| classify(&a.status) == Class::Busy)
        .count();
    let waiting = agents
        .iter()
        .filter(|a| classify(&a.status) == Class::Attention)
        .count();
    let mut segs = Vec::new();
    if busy > 0 {
        segs.push(format!("▶{busy} busy"));
    }
    if waiting > 0 {
        segs.push(format!("⏸{waiting} waiting"));
    }
    if segs.is_empty() {
        // Everything idle — a repo list here is just noise on the bar.
        return format!("{} idle", agents.len());
    }
    // List only the repos with something actually happening (busy/attention).
    let mut repos: Vec<String> = Vec::new();
    for a in agents {
        if classify(&a.status) == Class::Idle {
            continue;
        }
        let r = repo_label(a);
        if !repos.contains(&r) {
            repos.push(r);
        }
    }
    format!("{}  ·  {}", segs.join("  "), repos.join(", "))
}

fn human(agents: &[Agent]) -> String {
    if agents.is_empty() {
        return "no running agents\n".to_string();
    }
    let mut out = String::new();
    for a in agents {
        // Status column sized for tmx's longest current label
        // ("awaiting-decision" = 17 chars).
        out.push_str(&format!(
            "{:<7} {:<18} {}\n",
            a.kind,
            a.status,
            repo_label(a)
        ));
    }
    out
}

pub fn run(oneline_mode: bool, json: bool) -> i32 {
    // A parse failure is treated as "unavailable" (not empty) and noted on
    // stderr, so tmx format drift surfaces instead of reading as "no agents"
    // (codex I2).
    let (available, agents): (bool, Vec<Agent>) = match run_tmx(TMX_TIMEOUT) {
        Some(s) => match serde_json::from_str::<Snapshot>(&s) {
            Ok(snap) => (true, snap.agents.into_iter().filter(is_agent).collect()),
            Err(e) => {
                eprintln!("coctl agent status: tmx output was not parseable JSON: {e}");
                (false, Vec::new())
            }
        },
        None => (false, Vec::new()),
    };

    if json {
        let arr: Vec<_> = agents
            .iter()
            .map(|a| {
                serde_json::json!({
                    "kind": a.kind,
                    "status": a.status,
                    "repo": repo_label(a),
                    "cwd": a.cwd,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "available": available,
                "agents": arr,
            }))
            .unwrap()
        );
        return 0;
    }

    if !available {
        // Don't error a status bar; note it on stderr and print nothing/empty.
        eprintln!("coctl agent status: tmx unavailable (not installed, errored, or timed out)");
        if oneline_mode {
            println!("—");
        }
        return 0;
    }

    if oneline_mode {
        println!("{}", oneline(&agents));
    } else {
        print!("{}", human(&agents));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(kind: &str, status: &str, repo: &str) -> Agent {
        Agent {
            kind: kind.into(),
            status: status.into(),
            cwd: format!("/home/x/{repo}"),
            repo_name: repo.into(),
        }
    }

    #[test]
    fn classify_maps_tmx_vocabulary() {
        assert!(classify("working") == Class::Busy);
        assert!(classify("awaiting-decision") == Class::Attention);
        assert!(classify("ready") == Class::Idle);
        assert!(classify("idle") == Class::Idle);
        // a live codex background job is active, not idle
        assert!(classify("background") == Class::Busy);
        // future-proofing: unseen-but-related labels still bucket correctly
        assert!(classify("active") == Class::Busy);
        assert!(classify("waiting-for-input") == Class::Attention);
    }

    #[test]
    fn oneline_counts_busy_and_waiting() {
        let agents = vec![
            agent("claude", "working", "copad"),
            agent("codex", "working", "copad"),
            agent("claude", "awaiting-decision", "life-assistant"),
            agent("claude", "ready", "sssup"), // idle repo not listed
        ];
        let s = oneline(&agents);
        assert!(s.contains("▶2 busy"), "got: {s}");
        assert!(s.contains("⏸1 waiting"), "got: {s}");
        assert!(s.contains("copad"));
        assert!(s.contains("life-assistant"));
        assert!(!s.contains("sssup"), "idle repos should be omitted: {s}");
    }

    #[test]
    fn oneline_all_idle_omits_repos() {
        let agents = vec![
            agent("claude", "idle", "copad"),
            agent("claude", "ready", "docs"),
        ];
        assert_eq!(oneline(&agents), "2 idle");
    }

    #[test]
    fn oneline_empty() {
        assert_eq!(oneline(&[]), "no agents");
    }

    #[test]
    fn is_agent_filters_shell() {
        assert!(is_agent(&agent("claude", "busy", "r")));
        assert!(!is_agent(&agent("shell", "idle", "r")));
    }

    #[test]
    fn repo_label_sanitizes_control_chars() {
        let a = Agent {
            kind: "claude".into(),
            status: "working".into(),
            cwd: String::new(),
            repo_name: "evil\nrepo\tname".into(),
        };
        let label = repo_label(&a);
        assert!(!label.contains('\n'));
        assert!(!label.contains('\t'));
        // and the oneline stays single-line
        assert!(!oneline(std::slice::from_ref(&a)).contains('\n'));
    }

    #[test]
    fn repo_label_falls_back_to_cwd_basename() {
        let a = Agent {
            kind: "claude".into(),
            status: "busy".into(),
            cwd: "/home/x/myproj".into(),
            repo_name: String::new(),
        };
        assert_eq!(repo_label(&a), "myproj");
    }
}
