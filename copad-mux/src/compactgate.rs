//! `comux hook pre-compact`: a Claude Code `PreCompact` hook that moves an automatic compaction
//! to a better moment instead of wherever the context happens to fill up.
//!
//! Claude asks before EVERY request once the context is past its auto-compact threshold, and a
//! hook that answers `{"decision":"block"}` makes it carry on uncompacted and ask again before
//! the next request — mid-turn included, so a `/goal` loop is covered too (measured on Claude
//! Code 2.1.309; nothing here types into the pane). Lower the threshold with
//! `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE` and this hook spends the room between it and the window
//! waiting for the work tree to be committed.
//!
//! It only ever DEFERS, and it fails open. Anything it cannot establish lets the compaction
//! through: a manual `/compact`, an agent comux does not know, a window nobody reported (Claude
//! names its window only in its statusline — see `comux agent-report`), a slow or failing `git`,
//! an unreadable deferral count. Two escapes bound a deferral even when everything is known:
//! less than `--headroom` tokens left in the window (one response's input is the last thing
//! measured, so the next can grow past it), and `--max-deferrals` blocked asks in a row.
//!
//! "Committed" is a git-state PREFERENCE, not a work-unit detector: a clean tracked tree is
//! what a commit leaves behind, and also what a turn that only creates untracked files or only
//! reads looks like.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The whole decision, `git` included, runs inside this. A hook sits in front of every request.
const BUDGET: Duration = Duration::from_millis(1500);

pub const DEFAULT_HEADROOM: u64 = 60_000;
pub const DEFAULT_MAX_DEFERRALS: u32 = 40;

/// What the hook was asked (Claude's `PreCompact` input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub trigger: String,
    pub session_id: String,
    pub cwd: String,
}

/// The tracked work tree, as `git status` saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tree {
    Clean,
    Dirty,
    /// Not a repository, `git` failed, or it ran out of time.
    Unknown,
}

/// What the decision rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// Tokens in the context at the last response, and the window, from comux's reading of
    /// this agent — only when both are known for THIS conversation.
    pub fill: Option<(u64, u64)>,
    /// Blocked asks so far in this streak.
    pub deferred: u32,
    pub tree: Tree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub headroom: u64,
    pub max_deferrals: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Let it compact; the reason is for the log only.
    Allow(&'static str),
    /// Defer it, saying why (Claude shows the reason in verbose mode).
    Block(String),
}

/// Decide. Pure, so every escape is testable.
pub fn decide(ask: &Ask, facts: &Facts, limits: Limits) -> Verdict {
    if ask.trigger != "auto" {
        return Verdict::Allow("asked for by hand");
    }
    let Some((used, window)) = facts.fill else {
        return Verdict::Allow("window unknown");
    };
    if window.saturating_sub(used) < limits.headroom {
        return Verdict::Allow("near the window");
    }
    if facts.deferred >= limits.max_deferrals {
        return Verdict::Allow("deferred long enough");
    }
    match facts.tree {
        Tree::Clean => Verdict::Allow("work tree committed"),
        Tree::Unknown => Verdict::Allow("work tree unknown"),
        Tree::Dirty => Verdict::Block(format!(
            "comux: work in progress, compacting once it is committed ({}/{} deferred, {}k/{}k)",
            facts.deferred + 1,
            limits.max_deferrals,
            used / 1000,
            window / 1000,
        )),
    }
}

/// The hook's stdin → [`Ask`], `None` when it is not a `PreCompact` input we can read.
pub fn parse_ask(raw: &[u8]) -> Option<Ask> {
    let v: serde_json::Value = serde_json::from_slice(raw).ok()?;
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Some(Ask {
        trigger: s("trigger"),
        session_id: s("session_id"),
        cwd: s("cwd"),
    })
}

/// Run the hook: read the ask, gather what can be known in time, print the verdict. Always
/// exits 0 — a failing hook must never be what stops a compaction or a session.
pub fn run(limits: Limits) -> i32 {
    let deadline = Instant::now() + BUDGET;
    let mut raw = Vec::new();
    let _ = std::io::stdin().lock().take(1 << 20).read_to_end(&mut raw);
    let verdict = match parse_ask(&raw) {
        Some(ask) => judge(&ask, limits, deadline),
        None => Verdict::Allow("unreadable input"),
    };
    match &verdict {
        Verdict::Allow(_) => println!("{{}}"),
        Verdict::Block(reason) => println!(
            "{}",
            serde_json::json!({ "decision": "block", "reason": reason })
        ),
    }
    0
}

fn judge(ask: &Ask, limits: Limits, deadline: Instant) -> Verdict {
    if ask.trigger != "auto" {
        // A manual compaction ends any streak.
        if let Some(p) = counter_path(&ask.session_id) {
            let _ = std::fs::remove_file(p);
        }
        return Verdict::Allow("asked for by hand");
    }
    let fill = crate::control::agent_fill(&ask.session_id, deadline);
    let counter = counter_path(&ask.session_id);
    // An unusable counter lets it through rather than deferring without a bound.
    let deferred = counter.as_deref().map_or(Some(u32::MAX), read_count);
    let facts = Facts {
        fill,
        deferred: deferred.unwrap_or(u32::MAX),
        tree: if fill.is_some() {
            work_tree(Path::new(&ask.cwd), deadline)
        } else {
            Tree::Unknown
        },
    };
    let verdict = decide(ask, &facts, limits);
    if let Some(p) = &counter {
        let stored = match verdict {
            Verdict::Block(_) => write_count(p, facts.deferred + 1),
            Verdict::Allow(_) => std::fs::remove_file(p).or_else(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(e)
                }
            }),
        };
        // A block we could not count would be unbounded.
        if stored.is_err() && matches!(verdict, Verdict::Block(_)) {
            return Verdict::Allow("deferral count unwritable");
        }
    }
    verdict
}

/// `<state dir>/compact-gate/<session id>`, for a well-formed session id only (it becomes a
/// file name).
fn counter_path(session_id: &str) -> Option<PathBuf> {
    if !crate::agentstate::is_session_uuid(session_id) {
        return None;
    }
    Some(
        crate::persist::state_path()
            .with_file_name("compact-gate")
            .join(session_id),
    )
}

/// The stored count; absent = 0, unreadable = `None`.
fn read_count(p: &Path) -> Option<u32> {
    match std::fs::read_to_string(p) {
        Ok(s) => s.trim().parse().ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(0),
        Err(_) => None,
    }
}

/// Replace the count atomically (write a sibling, rename over), so a reader never sees half a
/// number. Two hooks for one session cannot run at once: Claude awaits the hook before the
/// request it guards.
fn write_count(p: &Path, n: u32) -> std::io::Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, n.to_string())?;
    std::fs::rename(&tmp, p)
}

/// Whether `cwd`'s tracked files have uncommitted changes, within the deadline. `git` runs
/// without optional locks or an fsmonitor, and is killed if it outlasts the budget.
fn work_tree(cwd: &Path, deadline: Instant) -> Tree {
    if cwd.as_os_str().is_empty() {
        return Tree::Unknown;
    }
    let child = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args([
            "-c",
            "core.fsmonitor=false",
            "status",
            "--porcelain",
            "--untracked-files=no",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return Tree::Unknown;
    };
    // Drained on its own thread while we wait: a big status fills the pipe, and a git blocked
    // on writing it would never exit. All we need is whether there was any.
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut so) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = so.read_to_end(&mut out);
            let _ = tx.send(!out.iter().all(u8::is_ascii_whitespace));
        });
    }
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let left = deadline.saturating_duration_since(Instant::now());
                return match (status.success(), rx.recv_timeout(left)) {
                    (true, Ok(false)) => Tree::Clean,
                    (true, Ok(true)) => Tree::Dirty,
                    _ => Tree::Unknown,
                };
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Tree::Unknown;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: Limits = Limits {
        headroom: DEFAULT_HEADROOM,
        max_deferrals: DEFAULT_MAX_DEFERRALS,
    };

    fn ask(trigger: &str) -> Ask {
        Ask {
            trigger: trigger.into(),
            session_id: "11111111-1111-4111-8111-111111111111".into(),
            cwd: "/x".into(),
        }
    }

    fn facts(fill: Option<(u64, u64)>, deferred: u32, tree: Tree) -> Facts {
        Facts {
            fill,
            deferred,
            tree,
        }
    }

    #[test]
    fn only_a_dirty_tree_with_room_and_budget_left_is_deferred() {
        let room = Some((120_000, 200_000));
        assert!(matches!(
            decide(&ask("auto"), &facts(room, 0, Tree::Dirty), LIMITS),
            Verdict::Block(_)
        ));
        for (a, f) in [
            (ask("manual"), facts(room, 0, Tree::Dirty)),
            (ask("auto"), facts(None, 0, Tree::Dirty)),
            // 150k of 200k leaves 50k, under the 60k headroom.
            (ask("auto"), facts(Some((150_000, 200_000)), 0, Tree::Dirty)),
            (ask("auto"), facts(room, DEFAULT_MAX_DEFERRALS, Tree::Dirty)),
            (ask("auto"), facts(room, 0, Tree::Clean)),
            (ask("auto"), facts(room, 0, Tree::Unknown)),
        ] {
            assert!(
                matches!(decide(&a, &f, LIMITS), Verdict::Allow(_)),
                "{a:?} {f:?}"
            );
        }
        // A 1M window defers far later than a 200k one.
        assert!(matches!(
            decide(
                &ask("auto"),
                &facts(Some((900_000, 1_000_000)), 3, Tree::Dirty),
                LIMITS
            ),
            Verdict::Block(_)
        ));
    }

    #[test]
    fn the_hook_input_is_read_leniently() {
        let a = parse_ask(
            br#"{"session_id":"s","transcript_path":"/t","cwd":"/w","hook_event_name":"PreCompact","trigger":"auto","custom_instructions":null}"#,
        )
        .unwrap();
        assert_eq!((a.trigger.as_str(), a.cwd.as_str()), ("auto", "/w"));
        assert!(parse_ask(b"nope").is_none());
    }

    #[test]
    fn the_count_survives_between_asks_and_a_bad_one_reads_as_unknown() {
        let dir = std::env::temp_dir().join(format!("compactgate-{}", std::process::id()));
        let p = dir.join("compact-gate").join("x");
        assert_eq!(read_count(&p), Some(0));
        write_count(&p, 3).unwrap();
        assert_eq!(read_count(&p), Some(3));
        std::fs::write(&p, "garbage").unwrap();
        assert_eq!(read_count(&p), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_reports_a_tracked_change_and_not_an_untracked_file() {
        let dir = std::env::temp_dir().join(format!("compactgate-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap()
        };
        let far = Instant::now() + Duration::from_secs(10);
        git(&["init", "-q"]);
        std::fs::write(dir.join("a"), "1").unwrap();
        git(&["add", "a"]);
        git(&["commit", "-q", "-m", "a"]);
        assert_eq!(work_tree(&dir, far), Tree::Clean);
        std::fs::write(dir.join("new"), "x").unwrap();
        assert_eq!(
            work_tree(&dir, far),
            Tree::Clean,
            "untracked files do not count"
        );
        std::fs::write(dir.join("a"), "2").unwrap();
        assert_eq!(work_tree(&dir, far), Tree::Dirty);
        assert_eq!(work_tree(&dir.join("missing"), far), Tree::Unknown);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
