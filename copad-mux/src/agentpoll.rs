//! Background poller for the "what is this agent actually DOING" line in the `Ctrl-f`
//! switcher.
//!
//! [`crate::agentstate`] takes screen scraping as far as it goes: it classifies
//! working/ready/blocked from stable UI furniture (#109). It cannot say *what* — the content
//! area is arbitrary, it reflows, and it is exactly what scrolls away. Both CLIs do write
//! structured logs, so the detail is read from those instead:
//!
//! * **Claude** — `~/.claude/sessions/<pid>.json` gives `sessionId` and `cwd`; the transcript
//!   is `~/.claude/projects/<cwd-slug>/<sessionId>.jsonl`, whose `assistant` records carry
//!   `message.content[].type == "tool_use"` with a `name` and an `input.description` /
//!   `input.command` / `input.file_path`.
//! * **Codex** — the rollout file the process holds open, whose `item_completed` events carry
//!   `CommandExecution` / `Reasoning` / `AgentMessage`.
//!
//! **Why a thread.** `procinfo::open_files` forks `lsof` on macOS and decision #88 forbids
//! forking on the sweep cadence; Claude's path needs no fork but does need a bounded tail read
//! of a transcript that runs to megabytes, and at 2 Hz per pane that is real I/O on the
//! single-writer loop. So this has the same shape as `usagepoll`/`versionpoll`: `spawn() ->
//! Shared`, `idle()` for the client/no-server case, and the lock is never held across I/O.
//!
//! **Absence is reported as absence.** No entry for a pid means "we do not know", and the row
//! renders exactly as it did before this module existed. This is inference from another
//! program's private on-disk format — twice — and both formats have already moved under us
//! once, so the failure mode is deliberately bounded to "no detail" rather than a stale or
//! invented one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// How often the thread wakes. Skipped entirely when nothing is being tracked.
const TICK: Duration = Duration::from_secs(1);

/// Most agent pids tracked at once. A mux with more agents than this is not a case worth
/// growing unbounded I/O for.
const MAX_TRACKED: usize = 64;

/// Source-file resolutions attempted per tick, for the tools whose resolution costs a FORK.
/// Bounds the burst when several codex panes appear at once. It deliberately does NOT gate
/// Claude, whose resolution is one small read: capping a fork-free path at 4/tick would defeat
/// the every-tick revalidation below as soon as there were five Claude agents.
const RESOLVE_BUDGET: usize = 4;

/// How long between re-checks of a source that costs a fork to resolve.
///
/// A conversation can change inside the SAME pid — Claude `/clear` mints a new `sessionId`,
/// codex `/new` rotates to a fresh rollout — and in both cases the previous log stays on disk,
/// so "the file is still there" is not evidence that it is still the right file. Claude has a
/// cheap identity oracle and is re-checked every tick; codex has none (the check IS the
/// `lsof`), and decision #88 forbids forking on the sweep cadence, so it waits this out.
const SOURCE_RECHECK: Duration = Duration::from_secs(30);

/// How long before retrying a resolution that failed. Without this a non-interactive `codex
/// exec` (which holds no rollout open) would make us fork every single tick, forever.
const RESOLVE_RETRY: Duration = Duration::from_secs(5);

/// Tail budget for a Claude transcript. Measured on a live one: 5296 lines with a largest
/// single line of 30 010 bytes, so 64 KiB could hold as few as two records — and if the newest
/// are large tool RESULTS, none of them a `tool_use`. 256 KiB is ~8 worst-case records.
const CLAUDE_TAIL: u64 = 256 * 1024;

/// Tail budget for a codex rollout. Its records are far smaller.
const CODEX_TAIL: u64 = 64 * 1024;

/// Longest detail string published. Cut on a CHAR boundary — tool descriptions here are
/// routinely non-ASCII.
const MAX_DETAIL: usize = 72;

/// What an agent was last seen doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    /// Display-ready, already trimmed (`"Bash: cargo test"`).
    pub detail: String,
}

#[derive(Default)]
pub struct Inner {
    /// Agent panes the render loop wants tracked: `(pid, tool basename)`.
    wanted: Vec<(u32, String)>,
    /// Published readings, keyed by agent pid.
    seen: HashMap<u32, Activity>,
}

pub type Shared = Arc<Mutex<Inner>>;

/// A poller that never runs — the client, and any path with no server.
pub fn idle() -> Shared {
    Arc::new(Mutex::new(Inner::default()))
}

/// Tell the poller which agent pids to track. Called from the render loop at the label
/// cadence; holds the lock only long enough to store a short `Vec`.
pub fn set_wanted(shared: &Shared, wanted: Vec<(u32, String)>) {
    if let Ok(mut g) = shared.lock() {
        g.wanted = wanted;
    }
}

/// The activity last read for `pid`, if any.
pub fn activity(shared: &Shared, pid: u32) -> Option<Activity> {
    shared.lock().ok()?.seen.get(&pid).cloned()
}

/// What we know about one tracked pid between ticks.
struct Entry {
    tool: String,
    /// The log we read for it, once resolved.
    path: Option<PathBuf>,
    /// `(mtime, len)` at the last read, so an unchanged file costs one `stat`.
    stamp: Option<(SystemTime, u64)>,
    /// The reading `stamp` was taken from, so an unchanged file costs the `stat` and
    /// NOTHING ELSE. Without this the stamp was computed and then ignored: every tick
    /// re-tailed 256 KiB per Claude agent, lossy-decoded it and parsed JSON, only to
    /// publish the same string again. At 30 agents that is ~7.7 MB/s of pure waste.
    ///
    /// `None` means "read fine, nothing parseable in the tail" — which is exactly what
    /// gets republished, so the distinction from a FAILED read matters and is why
    /// [`read_detail`] returns a nested `Option`.
    last: Option<Activity>,
    /// When resolution last produced no path, for [`RESOLVE_RETRY`].
    failed_at: Option<Instant>,
    /// When a source that costs a FORK to resolve was last re-resolved, for
    /// [`SOURCE_RECHECK`]. `None` = never attempted, so the first resolve is immediate.
    checked_at: Option<Instant>,
}

/// What the CHEAP re-check can say about the source we hold.
///
/// Only Claude has one. Its `~/.claude/sessions/<pid>.json` names the conversation the pid is
/// CURRENTLY in, and the transcript path's file stem is that same id, so comparing them is one
/// ~520-byte read and no path work at all.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    /// No usable signal — KEEP what we have. The session file is rewritten in place constantly
    /// (`updatedAt`), so a read landing mid-write must not blow away a working source.
    Unknown,
    /// Still the conversation we are reading.
    Same,
    /// A DIFFERENT conversation, positively identified.
    Changed,
}

/// Whether resolving this tool's source costs a fork. See [`RESOLVE_BUDGET`] and
/// [`SOURCE_RECHECK`], which both branch on it.
fn forks(tool: &str) -> bool {
    tool == "codex"
}

pub fn spawn() -> Shared {
    let shared = idle();
    let out = shared.clone();
    let _ = std::thread::Builder::new()
        .name("agent-poll".into())
        .spawn(move || {
            // Read `HOME` ONCE, here, and pass it down. Everything below then takes it as a
            // parameter, which is what makes the resolver testable: `std::env::set_var` is
            // `unsafe` in edition 2024 and races the other test threads. An absent `HOME`
            // leaves an empty path, whose reads fail, which this module already reports as
            // "no detail" rather than a wrong one.
            let home = std::env::var_os("HOME").map_or_else(PathBuf::new, PathBuf::from);
            let mut cache: HashMap<u32, Entry> = HashMap::new();
            let mut cursor = 0usize;
            loop {
                // Copy the request out and RELEASE the lock before any I/O: the render loop
                // takes the same lock to publish `wanted` and to read results, and must never
                // wait on a file read.
                let wanted: Vec<(u32, String)> = match out.lock() {
                    Ok(g) => g.wanted.iter().take(MAX_TRACKED).cloned().collect(),
                    Err(_) => Vec::new(),
                };
                if !wanted.is_empty() {
                    let seen = sweep(&wanted, &mut cache, &home, &mut cursor);
                    if let Ok(mut g) = out.lock() {
                        g.seen = seen;
                    }
                } else if !cache.is_empty() {
                    cache.clear();
                    if let Ok(mut g) = out.lock() {
                        g.seen.clear();
                    }
                }
                std::thread::sleep(TICK);
            }
        });
    shared
}

/// One pass over the tracked pids. Pure except for the filesystem, so it is testable with a
/// hand-built cache.
///
/// `cursor` rotates the visit order by one each tick. `wanted` is otherwise walked in a fixed
/// order, so with more unresolvable entries than [`RESOLVE_BUDGET`] the ones at the front eat
/// every tick's budget and an entry behind them never re-checks — it would keep publishing a
/// conversation that had already ended. Rotating makes every position the head within
/// `wanted.len()` ticks. The result is a `HashMap`, so visit order is not otherwise observable.
fn sweep(
    wanted: &[(u32, String)],
    cache: &mut HashMap<u32, Entry>,
    home: &Path,
    cursor: &mut usize,
) -> HashMap<u32, Activity> {
    cache.retain(|pid, e| wanted.iter().any(|(p, tool)| p == pid && tool == &e.tool));
    let mut budget = RESOLVE_BUDGET;
    let mut out = HashMap::new();
    let n = wanted.len();
    let start = *cursor % n.max(1);
    *cursor = cursor.wrapping_add(1);
    for k in 0..n {
        let (pid, tool) = &wanted[(start + k) % n];
        let e = cache.entry(*pid).or_insert_with(|| Entry {
            tool: tool.clone(),
            path: None,
            stamp: None,
            last: None,
            failed_at: None,
            checked_at: None,
        });
        // The file we were reading was DELETED. Distinct from the checks below, which ask
        // whether a file that still exists is still the right one.
        //
        // NOT `Path::exists()`: it collapses every `stat` error into `false`, so a permission
        // or I/O hiccup would read as "deleted" and throw away a source that is fine — and for
        // codex the 30s timer would then leave the row blank until the next recheck. Only a
        // confirmed `NotFound` counts.
        let vanished = e.path.as_ref().is_some_and(|p| {
            matches!(std::fs::metadata(p), Err(err) if err.kind() == std::io::ErrorKind::NotFound)
        });
        if vanished {
            e.path = None;
            e.stamp = None;
            e.last = None;
        }
        // The cheap oracle, for the tool that has one. codex has none, so it is the timer in
        // `want` that decides for it and this stays `false`.
        let changed = !forks(tool)
            && match claude_source(home, *pid, e.path.as_deref()) {
                Source::Unknown | Source::Same => false,
                Source::Changed => {
                    // A positively different conversation invalidates the reading AT ONCE —
                    // before, and independently of, whether the new transcript can be resolved
                    // yet. Claude writes the session-file entry before the first transcript
                    // record exists, so "B named, B not yet on disk" is a real window, and
                    // republishing A across it is the exact bug being fixed.
                    e.path = None;
                    e.stamp = None;
                    e.last = None;
                    true
                }
            };
        let due = e.failed_at.is_none_or(|t| t.elapsed() >= RESOLVE_RETRY);
        // What makes a re-resolve WANTED differs by tool, and for a forking tool it must not
        // include "we have no path": an observed absence clears the path, so keying off that
        // would fork `lsof` every single tick for a codex pane that simply has no rollout open.
        // For codex the timer is the only gate; `checked_at == None` means never attempted, so
        // the first resolve is still immediate.
        let want = if forks(tool) {
            e.checked_at.is_none_or(|t| t.elapsed() >= SOURCE_RECHECK)
        } else {
            e.path.is_none() || changed
        };
        if want && due && (!forks(tool) || budget > 0) {
            if forks(tool) {
                budget -= 1;
            }
            e.checked_at = Some(Instant::now());
            match resolve_log(home, *pid, tool) {
                // Could not observe: keep whatever we hold, and back off so a hard failure
                // does not retry every tick.
                None => e.failed_at = Some(Instant::now()),
                Some(found) => {
                    e.failed_at = found.is_none().then(Instant::now);
                    if found.as_ref() != e.path.as_ref() {
                        // The cached reading belongs to the OLD source.
                        e.path = found;
                        e.stamp = None;
                        e.last = None;
                    }
                }
            }
        }
        let Some(path) = e.path.clone() else { continue };
        let now = std::fs::metadata(&path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        if now.is_some() && now == e.stamp {
            // Unchanged: republish the last reading rather than dropping it, so an idle agent
            // does not flicker between a detail and nothing. This is the whole point of
            // `stamp` — see [`Entry::last`] for what it used to cost.
            if let Some(a) = e.last.clone() {
                out.insert(*pid, a);
            }
            continue;
        }
        // A read that SUCCEEDED commits both the stamp and the reading, whether or not the
        // tail held anything parseable — an inner `None` legitimately blanks the row.
        //
        // A read that FAILED falls through, leaving `stamp` uncommitted so the very next tick
        // retries. Committing it would suppress the pid permanently: a file restored to
        // readability without its mtime or length changing would never re-stamp. What we
        // already had is republished below rather than blanking the row over a transient
        // failure.
        if let Some(detail) = read_detail(&path, tool) {
            e.stamp = now;
            e.last = detail;
        }
        if let Some(a) = e.last.clone() {
            out.insert(*pid, a);
        }
    }
    out
}

/// Locate the log for an agent pid.
///
/// The nested `Option` is the same convention [`read_detail`] uses, one level up, and the two
/// negatives must NOT be conflated:
/// * `None` — could not OBSERVE (session file unreadable, `lsof`/`/proc` enumeration failed,
///   or the tool has no log we know how to find). The caller keeps whatever it holds.
/// * `Some(None)` — observed, and there is no log for this pid. The caller DROPS what it holds.
///
/// Codex closes its old rollout before opening the new one, so a re-check landing in that
/// window sees a successful enumeration with no rollout in it. Reading that as "could not
/// observe" would pin the previous conversation forever.
fn resolve_log(home: &Path, pid: u32, tool: &str) -> Option<Option<PathBuf>> {
    match tool {
        "claude" => claude_transcript(home, pid),
        "codex" => crate::agentstate::codex_rollout_path(pid),
        _ => None,
    }
}

/// Whether the transcript we hold is still the conversation `pid` is in.
///
/// The cached path's file stem IS the `sessionId`, so this is a string compare against one
/// small read — no path work, and in particular never the `read_dir` scan in
/// [`claude_transcript`]. Only a genuinely changed id pays for a resolve.
fn claude_source(home: &Path, pid: u32, path: Option<&Path>) -> Source {
    let Some(id) = claude_session_id(home, pid) else {
        return Source::Unknown;
    };
    match path.and_then(|p| p.file_stem()).and_then(|s| s.to_str()) {
        Some(stem) if stem == id => Source::Same,
        _ => Source::Changed,
    }
}

/// `(sessionId, cwd)` out of `~/.claude/sessions/<pid>.json`, the file Claude keeps current for
/// the conversation a pid is in. `None` when it cannot be read or does not name a valid id —
/// which callers treat as "cannot observe", never as "no session".
fn claude_session(home: &Path, pid: u32) -> Option<(String, Option<String>)> {
    let json = std::fs::read_to_string(
        home.join(".claude")
            .join("sessions")
            .join(format!("{pid}.json")),
    )
    .ok()?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let id = v.get("sessionId").and_then(|s| s.as_str())?;
    if !crate::agentstate::is_session_uuid(id) {
        return None;
    }
    let cwd = v.get("cwd").and_then(|c| c.as_str()).map(str::to_string);
    Some((id.to_string(), cwd))
}

fn claude_session_id(home: &Path, pid: u32) -> Option<String> {
    claude_session(home, pid).map(|(id, _)| id)
}

/// Claude's transcript for `pid`: guess the project slug from the session file's `cwd`, and
/// fall back to scanning `~/.claude/projects/*` for `<sessionId>.jsonl`.
///
/// The slug is another program's private encoding. Deriving it and trusting the result fails
/// SILENTLY — a missing file is indistinguishable from an agent that has run no tools — so the
/// guess is only ever used when it actually exists. `agentsessions.rs`, the other reader of
/// these files, enumerates for the same reason.
///
/// `Some(None)` means the session file named a conversation whose transcript is not on disk:
/// a real, observed absence (Claude creates the session entry before the first record), and
/// the caller must drop the previous conversation's transcript rather than keep reading it.
fn claude_transcript(home: &Path, pid: u32) -> Option<Option<PathBuf>> {
    let (id, cwd) = claude_session(home, pid)?;
    let projects = home.join(".claude").join("projects");
    let want = format!("{id}.jsonl");
    if let Some(cwd) = cwd {
        let guess = projects.join(project_slug(&cwd)).join(&want);
        if guess.is_file() {
            return Some(Some(guess));
        }
    }
    // The guess was wrong (or there was no cwd): find it. Bounded by the directory listing,
    // and only reached when the conversation actually changed.
    //
    // Neither `flatten()` nor `is_file()` is used to walk it: both turn an error into "not this
    // one", so one unreadable entry would make an INCOMPLETE listing look like a confirmed
    // absence — and `Some(None)` is exactly what tells the caller to drop the source it holds.
    let rd = std::fs::read_dir(&projects).ok()?;
    for entry in rd {
        let candidate = entry.ok()?.path().join(&want);
        match std::fs::metadata(&candidate) {
            Ok(m) if m.is_file() => return Some(Some(candidate)),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
    }
    Some(None)
}

/// Claude's project-directory encoding for a cwd, as observed: path separators become `-`.
/// Only ever used as a guess that is verified to exist (see [`claude_transcript`]).
fn project_slug(cwd: &str) -> String {
    cwd.replace('/', "-")
}

/// Read the newest activity out of `path`'s tail.
///
/// The nested `Option` separates two outcomes the caller must NOT conflate:
/// * `None` — could not read (unknown tool, open/seek/read failed). The caller leaves its
///   stamp uncommitted so the next tick retries; caching this would suppress the pid
///   forever once the file became readable again without its mtime/len changing.
/// * `Some(None)` — read fine, but the tail held nothing parseable. A real, cacheable
///   reading of "no activity".
fn read_detail(path: &Path, tool: &str) -> Option<Option<Activity>> {
    let (budget, parse): (u64, fn(&str) -> Option<String>) = match tool {
        "claude" => (CLAUDE_TAIL, claude_line_detail),
        "codex" => (CODEX_TAIL, codex_line_detail),
        _ => return None,
    };
    let text = tail(path, budget)?;
    Some(text.lines().rev().find_map(parse).map(|detail| Activity {
        detail: truncate(&detail, MAX_DETAIL),
    }))
}

/// The last `budget` bytes of `path` as UTF-8, with a leading PARTIAL line discarded.
///
/// The partial line is dropped rather than parsed: a JSON record cut mid-way is not merely
/// unparseable, it can be a valid prefix that parses to something else.
///
/// `None` means the file could not be READ, and nothing else — see the caller in
/// [`read_detail`] for why a successful read with no usable content must not share it.
fn tail(path: &Path, budget: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(budget);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::with_capacity(budget.min(len) as usize);
    f.take(budget).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if from == 0 {
        return Some(text);
    }
    // Not at the start of the file, so the first line is a fragment. NO newline at all means
    // the budget landed entirely inside one record — there is nothing complete to parse, but
    // the read still SUCCEEDED. That distinction is load-bearing now that the caller uses
    // `None` to mean "could not read, retry next tick": reporting a failure here would make an
    // oversized record re-read its file on every single tick, forever.
    Some(
        text.find('\n')
            .map_or(String::new(), |i| text[i + 1..].to_string()),
    )
}

/// One Claude transcript line → `"Bash: commit the fix"`, or `None` when it names no tool use.
fn claude_line_detail(line: &str) -> Option<String> {
    // Pre-filter before paying for JSON: most lines are not tool uses, and a line here runs
    // to 30 KB.
    if !line.contains("\"tool_use\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let content = v.pointer("/message/content")?.as_array()?;
    // The LAST tool_use in the record: one assistant turn may call several.
    let c = content
        .iter()
        .rev()
        .find(|c| c.get("type").and_then(|t| t.as_str()) == Some("tool_use"))?;
    let name = c.get("name").and_then(|n| n.as_str())?;
    let hint = ["description", "command", "file_path", "pattern", "prompt"]
        .iter()
        .find_map(|k| c.pointer(&format!("/input/{k}")).and_then(|x| x.as_str()))
        .map(clean)
        .filter(|h| !h.is_empty());
    Some(match hint {
        Some(h) => format!("{name}: {h}"),
        None => name.to_string(),
    })
}

/// One codex rollout line → `"running: cargo test"` / `"thinking"` / `"replying"`.
fn codex_line_detail(line: &str) -> Option<String> {
    if !line.contains("\"item_completed\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.pointer("/payload/type").and_then(|t| t.as_str()) != Some("item_completed") {
        return None;
    }
    let item = v.pointer("/payload/item")?;
    match item.get("type").and_then(|t| t.as_str())? {
        "CommandExecution" => Some(format!("running: {}", command_text(item)?)),
        "Reasoning" => Some("thinking".to_string()),
        "AgentMessage" => Some("replying".to_string()),
        _ => None,
    }
}

/// The human-readable command out of a codex `CommandExecution` item.
///
/// `command` is an ARRAY, not a string — `["/bin/zsh", "-lc", "git status --short"]` — so the
/// interesting part is the LAST element, the script the wrapper was handed. codex also
/// pre-parses it into `parsed_cmd[].cmd`, which is cleaner still, so that is preferred.
/// (The first version of this read `command` as a string. It passed against a fixture that
/// was invented rather than captured, and produced nothing at all against a real rollout.)
fn command_text(item: &serde_json::Value) -> Option<String> {
    let parsed = item
        .pointer("/parsed_cmd/0/cmd")
        .and_then(|c| c.as_str())
        .map(clean)
        .filter(|c| !c.is_empty());
    if parsed.is_some() {
        return parsed;
    }
    let raw = item.get("command")?;
    let text = match raw {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a.last()?.as_str()?.to_string(),
        _ => return None,
    };
    Some(clean(&text)).filter(|c| !c.is_empty())
}

/// Collapse a multi-line hint to one line of single-spaced text.
fn clean(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut to `max` CHARS (not bytes — these strings are routinely non-ASCII), with an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transcript line that parses to `"Bash: <what>"`.
    fn tool_use_line(what: &str) -> String {
        format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Bash","input":{{"description":"{what}"}}}}]}}}}"#
        )
    }

    fn entry_for(path: &std::path::Path) -> Entry {
        Entry {
            tool: "claude".into(),
            path: Some(path.to_path_buf()),
            stamp: None,
            last: None,
            failed_at: None,
            checked_at: None,
        }
    }

    /// A home with no `sessions/` in it, so the cheap oracle returns [`Source::Unknown`] and a
    /// hand-built path stands. That is what the tests below mean by "this pane reads this
    /// file"; the conversation-change tests build a real home instead.
    fn no_home() -> &'static Path {
        Path::new("/nonexistent/agentpoll-test-home")
    }

    fn sweep_once(
        wanted: &[(u32, String)],
        cache: &mut HashMap<u32, Entry>,
    ) -> HashMap<u32, Activity> {
        let mut cursor = 0usize;
        sweep(wanted, cache, no_home(), &mut cursor)
    }

    /// The bug this module's `stamp` was always supposed to prevent: an unchanged transcript
    /// was re-tailed, lossy-decoded and JSON-parsed on EVERY tick.
    ///
    /// Proving a read did NOT happen needs the read to be observable, so the second pass runs
    /// against a file whose CONTENT has been swapped while its length and mtime are restored.
    /// Anything that re-reads publishes the new content; only the cache publishes the old.
    #[test]
    fn an_unchanged_transcript_is_not_re_read() {
        let dir = std::env::temp_dir().join(format!("agentpoll-unchanged-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");

        std::fs::write(&path, tool_use_line("first") + "\n").unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let (mtime, len) = (meta.modified().unwrap(), meta.len());

        let mut cache = HashMap::from([(1u32, entry_for(&path))]);
        let wanted = vec![(1u32, "claude".to_string())];
        let first = sweep_once(&wanted, &mut cache);
        assert_eq!(first[&1].detail, "Bash: first");

        // Same length ("first" and "wrong" are both 5 bytes), same mtime → same stamp.
        std::fs::write(&path, tool_use_line("wrong") + "\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            len,
            "stamp must match"
        );

        let second = sweep_once(&wanted, &mut cache);
        assert_eq!(
            second[&1].detail, "Bash: first",
            "an unchanged stamp must republish the cached reading, not re-read the file"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build a fake `$HOME` holding Claude's two files: the per-pid session record naming a
    /// conversation, and that conversation's transcript.
    struct FakeHome {
        dir: PathBuf,
    }

    impl FakeHome {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "agentpoll-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join(".claude").join("sessions")).unwrap();
            std::fs::create_dir_all(dir.join(".claude").join("projects").join("-w")).unwrap();
            Self { dir }
        }

        /// Point `pid`'s session record at conversation `id`, as Claude does on `/clear`.
        fn in_conversation(&self, pid: u32, id: &str) {
            std::fs::write(
                self.dir
                    .join(".claude")
                    .join("sessions")
                    .join(format!("{pid}.json")),
                format!(r#"{{"pid":{pid},"sessionId":"{id}","cwd":"/w"}}"#),
            )
            .unwrap();
        }

        /// Write a transcript for `id` whose newest tool use says `what`.
        fn transcript(&self, id: &str, what: &str) -> PathBuf {
            let path = self
                .dir
                .join(".claude")
                .join("projects")
                .join("-w")
                .join(format!("{id}.jsonl"));
            std::fs::write(&path, tool_use_line(what) + "\n").unwrap();
            path
        }

        fn sweep(&self, cache: &mut HashMap<u32, Entry>) -> HashMap<u32, Activity> {
            let mut cursor = 0usize;
            sweep(
                &[(1u32, "claude".to_string())],
                cache,
                &self.dir,
                &mut cursor,
            )
        }
    }

    impl Drop for FakeHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const A: &str = "aaaaaaaa-1111-2222-3333-444444444444";
    const B: &str = "bbbbbbbb-1111-2222-3333-444444444444";

    /// The bug: a conversation can change inside the SAME pid — Claude `/clear` mints a new
    /// `sessionId` — and the previous transcript stays on disk. Re-resolving only when the
    /// cached path has VANISHED therefore never fires, and the old conversation's activity is
    /// republished indefinitely.
    #[test]
    fn a_new_conversation_in_the_same_pid_stops_republishing_the_old_one() {
        let home = FakeHome::new("cleared");
        let old = home.transcript(A, "first");
        home.in_conversation(1, A);
        home.transcript(B, "second");

        let mut cache = HashMap::from([(1u32, entry_for(&old))]);
        assert_eq!(home.sweep(&mut cache)[&1].detail, "Bash: first");

        // `/clear`: same pid, new conversation, and A is STILL THERE.
        home.in_conversation(1, B);
        assert!(
            old.is_file(),
            "the old transcript must survive, or this proves nothing"
        );
        assert_eq!(
            home.sweep(&mut cache)[&1].detail,
            "Bash: second",
            "a changed sessionId must re-resolve the transcript, not keep reading the old one"
        );
    }

    /// The window the plan review caught: Claude writes the session record naming B before B's
    /// transcript exists. Publishing A's activity across that gap is the same bug wearing a
    /// hat — absence is what this module reports when it does not know.
    #[test]
    fn a_named_conversation_with_no_transcript_yet_publishes_nothing() {
        let home = FakeHome::new("nascent");
        let old = home.transcript(A, "first");
        home.in_conversation(1, A);

        let mut cache = HashMap::from([(1u32, entry_for(&old))]);
        assert_eq!(home.sweep(&mut cache)[&1].detail, "Bash: first");

        home.in_conversation(1, B); // B has no transcript on disk yet
        assert!(
            !home.sweep(&mut cache).contains_key(&1),
            "a conversation we cannot read yet must publish nothing, not the previous one"
        );
    }

    /// The other side of the same coin, and why the cheap oracle is TRI-state: the session file
    /// is rewritten in place on every status change, so a read landing mid-write yields nothing
    /// — which must not be mistaken for "the conversation changed".
    #[test]
    fn an_unreadable_session_file_keeps_the_source_we_have() {
        let home = FakeHome::new("unreadable");
        let path = home.transcript(A, "first");
        home.in_conversation(1, A);

        let mut cache = HashMap::from([(1u32, entry_for(&path))]);
        assert_eq!(home.sweep(&mut cache)[&1].detail, "Bash: first");

        std::fs::write(
            home.dir.join(".claude").join("sessions").join("1.json"),
            "{ truncated",
        )
        .unwrap();
        assert_eq!(
            home.sweep(&mut cache)[&1].detail,
            "Bash: first",
            "an unobservable identity must keep the source, not drop it"
        );
        assert_eq!(cache[&1].path.as_deref(), Some(path.as_path()));
    }

    /// `Path::exists()` answers `false` for "it is not there" AND for "I could not look", and
    /// the second must not discard a working source — for codex it would then be blank until
    /// the 30s recheck. Made observable by putting the transcript behind a directory the test
    /// process cannot traverse, which fails `stat` with `EACCES` rather than `ENOENT`.
    #[test]
    #[cfg(unix)]
    fn a_source_we_cannot_stat_is_not_treated_as_deleted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("agentpoll-nostat-{}", std::process::id()));
        let inner = dir.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let path = inner.join("t.jsonl");
        std::fs::write(&path, tool_use_line("first") + "\n").unwrap();

        let wanted = vec![(1u32, "claude".to_string())];
        let mut cache = HashMap::from([(1u32, entry_for(&path))]);
        assert_eq!(sweep_once(&wanted, &mut cache)[&1].detail, "Bash: first");

        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o000)).unwrap();
        let blocked = std::fs::metadata(&path).is_err();
        let got = sweep_once(&wanted, &mut cache);
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        if !blocked {
            return; // running as root: the premise does not hold, so assert nothing
        }
        assert_eq!(
            got.get(&1).map(|a| a.detail.as_str()),
            Some("Bash: first"),
            "a source we merely could not stat must be kept, not treated as deleted"
        );
    }

    fn codex_entry(path: Option<PathBuf>, checked_at: Option<Instant>) -> Entry {
        Entry {
            tool: "codex".into(),
            path,
            stamp: None,
            last: None,
            // Old enough that [`RESOLVE_RETRY`] never gates these tests: the point is to
            // isolate the SOURCE_RECHECK timer.
            failed_at: Instant::now().checked_sub(Duration::from_secs(3600)),
            checked_at,
        }
    }

    fn ago(secs: u64) -> Option<Instant> {
        Instant::now().checked_sub(Duration::from_secs(secs))
    }

    /// Codex's source can only be re-checked by forking `lsof`, which decision #88 forbids on
    /// the sweep cadence — so unlike Claude's it waits out [`SOURCE_RECHECK`].
    #[test]
    fn a_forking_source_is_not_re_resolved_every_tick() {
        let dir = std::env::temp_dir().join(format!("agentpoll-timer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        let wanted = vec![(7u32, "codex".to_string())];

        let fresh = ago(1);
        let mut cache = HashMap::from([(7u32, codex_entry(Some(path.clone()), fresh))]);
        sweep_once(&wanted, &mut cache);
        assert_eq!(
            cache[&7].checked_at, fresh,
            "a fresh check must not fork again"
        );

        let stale = ago(SOURCE_RECHECK.as_secs() + 1);
        cache.get_mut(&7).unwrap().checked_at = stale;
        sweep_once(&wanted, &mut cache);
        assert_ne!(cache[&7].checked_at, stale, "a stale check must re-resolve");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A codex pane with no rollout open at all (`codex exec`, or the moment between closing
    /// one and opening the next) resolves to an OBSERVED ABSENCE, which clears the cached path.
    /// Keying the re-resolve off "we have no path" would then fork `lsof` every single tick,
    /// forever — the exact cadence the timer exists to prevent.
    #[test]
    fn a_codex_pane_with_no_rollout_does_not_fork_every_tick() {
        let wanted = vec![(7u32, "codex".to_string())];
        let fresh = ago(1);
        let mut cache = HashMap::from([(7u32, codex_entry(None, fresh))]);
        sweep_once(&wanted, &mut cache);
        assert_eq!(
            cache[&7].checked_at, fresh,
            "an observed absence must wait out the timer like a cached path does"
        );
    }

    /// `wanted` is walked in a fixed order, so the entries at the front get first claim on
    /// [`RESOLVE_BUDGET`] every tick. Under today's gates that cannot actually starve anyone —
    /// each codex entry wants a resolve at most once per [`SOURCE_RECHECK`], so the queue
    /// drains — but the bias is one gate change away from mattering, and rotating the head
    /// costs three lines. This test forces the pathological case (every entry wanting a resolve
    /// on every tick) to exercise the mechanism, since nothing reachable does.
    #[test]
    fn the_resolve_budget_rotates_so_the_tail_is_not_starved() {
        let n = RESOLVE_BUDGET + 2;
        let wanted: Vec<(u32, String)> = (0..n as u32).map(|i| (i, "codex".to_string())).collect();
        let mut cache: HashMap<u32, Entry> = wanted
            .iter()
            .map(|(p, _)| (*p, codex_entry(None, None)))
            .collect();
        let mut cursor = 0usize;
        let mut served: Vec<u32> = Vec::new();
        let home = no_home();

        // Rotating by one means every position becomes the head within `n` ticks.
        for _ in 0..n {
            sweep(&wanted, &mut cache, home, &mut cursor);
            for (pid, e) in cache.iter_mut() {
                if e.checked_at.take().is_some() {
                    served.push(*pid);
                }
                e.failed_at = Instant::now().checked_sub(Duration::from_secs(3600));
            }
        }
        served.sort_unstable();
        served.dedup();
        assert_eq!(
            served.len(),
            n,
            "every tracked pid must get a turn at the budget; served {served:?}"
        );
    }

    /// A transient read failure must not be cached as "no activity": the stamp stays
    /// uncommitted so the next tick retries. Caching it would suppress the pid until the
    /// file's mtime or length happened to change again.
    #[test]
    #[cfg(unix)]
    fn a_failed_read_is_retried_and_never_poisons_the_cache() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("agentpoll-failed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let wanted = vec![(1u32, "claude".to_string())];

        std::fs::write(&path, tool_use_line("first") + "\n").unwrap();
        let mut cache = HashMap::from([(1u32, entry_for(&path))]);
        assert_eq!(sweep_once(&wanted, &mut cache)[&1].detail, "Bash: first");

        // Content AND length change (so the stamp differs), but the file cannot be read.
        std::fs::write(&path, tool_use_line("second reading") + "\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let blocked = sweep_once(&wanted, &mut cache);
        assert_eq!(
            blocked[&1].detail, "Bash: first",
            "a transient failure republishes the last good reading rather than blanking the row"
        );

        // Readable again, with mtime and length untouched since the failed pass. Only an
        // UNCOMMITTED stamp can notice.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let recovered = sweep_once(&wanted, &mut cache);
        assert_eq!(
            recovered[&1].detail, "Bash: second reading",
            "the failed read must have left the stamp uncommitted so this tick retries"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A codex `/new` rotates to a fresh rollout. The previous conversation's activity must
    /// not be attributed to the new one while the new path is still unresolved.
    #[test]
    fn a_vanished_transcript_drops_its_cached_reading() {
        let dir = std::env::temp_dir().join(format!("agentpoll-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let wanted = vec![(1u32, "claude".to_string())];

        std::fs::write(&path, tool_use_line("old conversation") + "\n").unwrap();
        let mut cache = HashMap::from([(1u32, entry_for(&path))]);
        assert_eq!(
            sweep_once(&wanted, &mut cache)[&1].detail,
            "Bash: old conversation"
        );

        std::fs::remove_file(&path).unwrap();
        assert!(
            !sweep_once(&wanted, &mut cache).contains_key(&1),
            "a reading must never outlive the source it was read from"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_claude_tool_use_becomes_a_named_detail() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"},{"type":"tool_use","name":"Bash","input":{"command":"cargo test","description":"Run the tests"}}]}}"#;
        assert_eq!(
            claude_line_detail(line).as_deref(),
            Some("Bash: Run the tests"),
            "description is preferred over command — it is what the agent meant, not how"
        );
    }

    #[test]
    fn a_claude_detail_falls_back_through_the_input_keys() {
        let cmd = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la"}}]}}"#;
        assert_eq!(claude_line_detail(cmd).as_deref(), Some("Bash: ls -la"));
        let path = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/a/b.rs"}}]}}"#;
        assert_eq!(claude_line_detail(path).as_deref(), Some("Read: /a/b.rs"));
        // A tool with no recognised hint still names itself rather than vanishing.
        let bare = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"TodoWrite","input":{"todos":[]}}]}}"#;
        assert_eq!(claude_line_detail(bare).as_deref(), Some("TodoWrite"));
    }

    #[test]
    fn a_claude_line_with_no_tool_use_is_not_a_detail() {
        let text =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"just talking"}]}}"#;
        assert_eq!(claude_line_detail(text), None);
        assert_eq!(claude_line_detail("{}"), None);
        assert_eq!(claude_line_detail("not json at all"), None);
        // The pre-filter must not be the only gate: a line MENTIONING the string but shaped
        // differently has to fall out of the parse, not be reported.
        assert_eq!(
            claude_line_detail(r#"{"type":"user","text":"what is \"tool_use\"?"}"#),
            None
        );
    }

    #[test]
    fn the_last_tool_use_in_a_record_wins() {
        // One assistant turn may call several tools; the newest is what it is doing NOW.
        let line = r#"{"message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/a"}},{"type":"tool_use","name":"Bash","input":{"description":"second"}}]}}"#;
        assert_eq!(claude_line_detail(line).as_deref(), Some("Bash: second"));
    }

    #[test]
    fn codex_items_map_to_what_the_user_would_call_them() {
        // The REAL shape, captured from a rollout on this machine: `command` is an ARRAY
        // (`["/bin/zsh", "-lc", "<script>"]`) and codex also pre-parses it into `parsed_cmd`.
        let run = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":["/bin/zsh","-lc","git status --short"],"parsed_cmd":[{"type":"unknown","cmd":"git status --short"}]}}}"#;
        assert_eq!(
            codex_line_detail(run).as_deref(),
            Some("running: git status --short")
        );
        // Without `parsed_cmd`, the LAST array element is the script, not `/bin/zsh`.
        let arr = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":["/bin/zsh","-lc","cargo  test\n--all"]}}}"#;
        assert_eq!(
            codex_line_detail(arr).as_deref(),
            Some("running: cargo test --all"),
            "a multi-line command collapses to one line"
        );
        // A scalar `command` (an older/plainer shape) still works.
        let scalar = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":"ls"}}}"#;
        assert_eq!(codex_line_detail(scalar).as_deref(), Some("running: ls"));
        let think = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning"}}}"#;
        assert_eq!(codex_line_detail(think).as_deref(), Some("thinking"));
        let reply = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage"}}}"#;
        assert_eq!(codex_line_detail(reply).as_deref(), Some("replying"));
        // An item type we do not know yields nothing rather than a guess.
        let other = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Extension"}}}"#;
        assert_eq!(codex_line_detail(other), None);
        // A different event entirely.
        let tok = r#"{"type":"event_msg","payload":{"type":"token_count","info":null}}"#;
        assert_eq!(codex_line_detail(tok), None);
    }

    #[test]
    fn a_detail_is_cut_on_a_char_boundary() {
        // Tool descriptions here are routinely non-ASCII; slicing by BYTES would panic.
        let korean = "한국어".repeat(60);
        let cut = truncate(&korean, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'));
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactlyten", 10), "exactlyten");
    }

    #[test]
    fn a_tail_drops_the_leading_partial_line() {
        let dir = std::env::temp_dir().join(format!("agentpoll-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.jsonl");
        std::fs::write(&f, "AAAA\nBBBB\nCCCC\n").unwrap();
        // Budget lands mid-way through the first line, so it must be discarded whole — a
        // JSON record cut mid-way is not merely unparseable, it can be a valid prefix that
        // parses to something else.
        assert_eq!(tail(&f, 12).as_deref(), Some("BBBB\nCCCC\n"));
        // A budget covering the file keeps everything.
        assert_eq!(tail(&f, 999).as_deref(), Some("AAAA\nBBBB\nCCCC\n"));

        // A single record LARGER than the budget leaves no complete line — but the read
        // succeeded, so this must be an empty tail, NOT the `None` that means "could not
        // read". `sweep` leaves its stamp uncommitted on `None`, so conflating the two would
        // make an oversized record re-read its file on every tick forever: exactly the bug
        // the stamp exists to prevent.
        let big = dir.join("big.jsonl");
        std::fs::write(&big, "x".repeat(4096)).unwrap();
        assert_eq!(tail(&big, 64).as_deref(), Some(""));

        // And `None` still means unreadable.
        assert_eq!(tail(&dir.join("absent.jsonl"), 64), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_slug_guess_is_only_ever_a_guess() {
        assert_eq!(project_slug("/Users/me/dev/copad"), "-Users-me-dev-copad");
        assert_eq!(project_slug("/private/tmp"), "-private-tmp");
    }
}

/// Checks against the REAL logs on this machine rather than hand-written fixtures. Ignored by
/// default: they depend on the developer's own `~/.claude` / `~/.codex`, so they are a
/// development oracle for format drift, not part of the suite. Run with
/// `cargo test -p copad-mux -- --ignored live_`.
#[cfg(test)]
mod live_tests {
    use super::*;

    fn newest(dir: &Path, pat: &str) -> Option<PathBuf> {
        let mut best: Option<(SystemTime, PathBuf)> = None;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).ok()?.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.to_string_lossy().contains(pat)
                    && let Ok(m) = e.metadata().and_then(|m| m.modified())
                    && best.as_ref().is_none_or(|(b, _)| m > *b)
                {
                    best = Some((m, p));
                }
            }
        }
        best.map(|(_, p)| p)
    }

    #[test]
    #[ignore = "reads the developer's own ~/.claude"]
    fn live_claude_transcript_still_yields_a_detail() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let Some(f) = newest(&home.join(".claude/projects"), ".jsonl") else {
            return;
        };
        let d = read_detail(&f, "claude");
        assert!(
            // `Some(None)` = read fine but nothing parseable, which is the failure this
            // test exists to catch. Only a nested `Some` means the format still matches.
            matches!(d, Some(Some(_))),
            "no tool_use found in the tail of {f:?} — Claude's transcript format may have moved"
        );
        println!("claude: {:?} -> {:?}", f.file_name().unwrap(), d);
    }

    #[test]
    #[ignore = "reads the developer's own ~/.codex"]
    fn live_codex_rollout_still_yields_a_detail() {
        // Pick a rollout that actually RAN something. The newest rollout is often a session
        // that only reached the trust prompt, and asserting against that would make this
        // oracle green while the parser was broken — which is exactly how the first version
        // of `command_text` shipped reading an array as a string.
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let mut best: Option<(SystemTime, PathBuf)> = None;
        let mut stack = vec![home.join(".codex/sessions")];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if !p.to_string_lossy().contains("rollout-") {
                    continue;
                }
                if !std::fs::read_to_string(&p).is_ok_and(|t| t.contains("\"CommandExecution\"")) {
                    continue;
                }
                if let Ok(m) = e.metadata().and_then(|m| m.modified())
                    && best.as_ref().is_none_or(|(b, _)| m > *b)
                {
                    best = Some((m, p));
                }
            }
        }
        let Some((_, f)) = best else { return };
        let d = read_detail(&f, "codex");
        println!("codex: {:?} -> {:?}", f.file_name().unwrap(), d);
        assert!(
            d.flatten().is_some_and(|a| !a.detail.is_empty()),
            "a rollout containing CommandExecution yielded no detail — codex's rollout \
             format may have moved ({f:?})"
        );
    }
}
