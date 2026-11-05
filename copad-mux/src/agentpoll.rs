//! Background poller for the "what is this agent actually DOING" line in the `Ctrl-f` switcher.
//! [`crate::agentstate`] classifies working/ready/blocked from screen furniture but cannot say
//! *what*, so the detail is read from each CLI's structured log:
//!
//! * **Claude** — `~/.claude/sessions/<pid>.json` gives `sessionId` and `cwd`; the transcript
//!   is `~/.claude/projects/<cwd-slug>/<sessionId>.jsonl`, whose `assistant` records carry
//!   `message.content[].type == "tool_use"` with a `name` and an `input.description` /
//!   `input.command` / `input.file_path`.
//! * **Codex** — the rollout file the process holds open, whose `item_completed` events carry
//!   `CommandExecution` / `Reasoning` / `AgentMessage`.
//! * **grok** — `$GROK_HOME/active_sessions.json` names the pid's session; its transcript is
//!   `sessions/<encoded cwd>/<id>/chat_history.jsonl`, whose `assistant` records carry
//!   `tool_calls[]` with a `name` and JSON-string `arguments`.
//! * **agy** — the conversation whose presence lock the process holds (a fork, like codex); its
//!   transcript is `brain/<id>/.system_generated/logs/transcript.jsonl`, whose
//!   `PLANNER_RESPONSE` steps carry `tool_calls[]` with a `toolSummary`.
//!
//! **Why a thread.** `procinfo::open_files` forks `lsof` on macOS and decision #88 forbids forking
//! on the sweep cadence, and a transcript tail read at 2 Hz per pane is real I/O on the
//! single-writer loop. Same shape as `usagepoll`/`versionpoll`: `spawn() -> Shared`, `idle()` for
//! the client/no-server case, and the lock is never held across I/O.
//!
//! **Absence is reported as absence.** No entry for a pid means "we do not know" and the row
//! renders without detail. The formats are another program's private on-disk ones and have moved
//! before, so the failure mode is bounded to "no detail", never a stale or invented one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const TICK: Duration = Duration::from_secs(1);

const MAX_TRACKED: usize = 64;

/// Source-file resolutions attempted per tick, for tools whose resolution costs a FORK. Claude is
/// deliberately not gated: its resolution is one small read, and capping it would defeat the
/// every-tick revalidation.
const RESOLVE_BUDGET: usize = 4;

/// How long between re-checks of a source that costs a fork to resolve. A conversation can change
/// inside the SAME pid (Claude `/clear`, codex `/new`) while the old log stays on disk, so "the
/// file is still there" proves nothing. Claude has a cheap identity oracle and is re-checked every
/// tick; codex's check IS the `lsof`, which decision #88 forbids on the sweep cadence.
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

/// Tail budget for a grok `chat_history.jsonl`: its `tool_result` records carry whole file
/// reads, like Claude's.
const GROK_TAIL: u64 = 256 * 1024;

/// Tail budget for an agy `transcript.jsonl`: one record per step, results included.
const AGY_TAIL: u64 = 256 * 1024;

/// Longest detail string published. Cut on a CHAR boundary — tool descriptions here are
/// routinely non-ASCII.
const MAX_DETAIL: usize = 72;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activity {
    pub detail: String,
}

/// How full an agent's context window was at its last model response.
///
/// `used` is the INPUT side of that response — fresh input plus cache reads and writes —
/// which is what the next request starts from and what Claude's own statusline calls
/// `used_percentage`. `window` is known only where the log says it (Codex's
/// `model_context_window`); Claude's transcript never names its window (200k and 1M models
/// share ids), so it comes from the statusline push instead, never from a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    pub used: u64,
    pub window: Option<u64>,
}

/// What one log line says about the context, newest first: nothing, a reading, or that a
/// compaction reset it (to the post-compaction size, when the log records one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mark {
    Used(Context),
    Reset(Option<u64>),
}

#[derive(Default)]
pub struct Inner {
    wanted: Vec<(u32, String)>,
    seen: HashMap<u32, Activity>,
    /// The log each pid was resolved to, published whether or not it has yielded a detail
    /// yet — a fresh conversation that has run no tool still HAS a conversation.
    sources: HashMap<u32, PathBuf>,
    contexts: HashMap<u32, Context>,
}

pub type Shared = Arc<Mutex<Inner>>;

pub fn idle() -> Shared {
    Arc::new(Mutex::new(Inner::default()))
}

pub fn set_wanted(shared: &Shared, wanted: Vec<(u32, String)>) {
    if let Ok(mut g) = shared.lock() {
        g.wanted = wanted;
    }
}

/// The conversation `pid` is in, as `(id, transcript path)`, when its log has been resolved.
///
/// As fresh as the poller's resolution: Claude's is re-checked against its session file every
/// tick, Codex's (and agy's) every [`SOURCE_RECHECK`], so a Codex pane that just switched
/// conversation can report the previous one for that long. A Codex TUI attached to the shared
/// app-server daemon reports the daemon-held rollout [`crate::codexdaemon`] correlates to it,
/// and nothing when that correlation is ambiguous.
pub fn conversation(shared: &Shared, pid: u32) -> Option<(String, PathBuf)> {
    let path = shared.lock().ok()?.sources.get(&pid).cloned()?;
    Some((transcript_session_id(&path)?, path))
}

/// The conversation id a transcript path belongs to. Each tool puts it somewhere else — the
/// trailing uuid of a codex `rollout-<ts>-<uuid>`, Claude's file stem, the session DIRECTORY
/// for grok (`…/<id>/chat_history.jsonl`) and agy (`brain/<id>/.system_generated/logs/…`) — so
/// it is the nearest path component that IS a session uuid, never just "the stem".
fn transcript_session_id(path: &Path) -> Option<String> {
    if let Some(id) = crate::agentstate::rollout_session_id(path) {
        return Some(id);
    }
    let stem = path.file_stem().and_then(|s| s.to_str());
    stem.into_iter()
        .chain(
            path.ancestors()
                .skip(1)
                .filter_map(|a| a.file_name()?.to_str()),
        )
        .find(|c| crate::agentstate::is_session_uuid(c))
        .map(str::to_string)
}

pub fn activity(shared: &Shared, pid: u32) -> Option<Activity> {
    shared.lock().ok()?.seen.get(&pid).cloned()
}

pub fn context(shared: &Shared, pid: u32) -> Option<Context> {
    shared.lock().ok()?.contexts.get(&pid).copied()
}

struct Entry {
    tool: String,
    path: Option<PathBuf>,
    stamp: Option<(SystemTime, u64)>,
    /// The reading `stamp` was taken from, so an unchanged file costs only the `stat` instead of
    /// re-tailing and re-parsing 256 KiB per agent every tick. `None` means "read fine, nothing
    /// parseable", which differs from a FAILED read; that is why [`read_detail`] returns a nested
    /// `Option`.
    last: Option<Activity>,
    /// The newest context reading in this source. Unlike `last` it is KEPT across a read whose
    /// tail holds no usage record — a turn of big tool output can push the last response out
    /// of the tail, and that says nothing about the context having emptied. Only a source
    /// change or a compaction marker clears it.
    ctx: Option<Context>,
    failed_at: Option<Instant>,
    /// When a source that costs a FORK to resolve was last re-resolved, for [`SOURCE_RECHECK`];
    /// `None` = never, so the first resolve is immediate.
    checked_at: Option<Instant>,
}

/// What the CHEAP re-check can say about the source we hold. Only Claude has one: its session file
/// names the conversation the pid is in NOW, and the transcript's file stem is that same id.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    /// No usable signal — KEEP what we have. The session file is rewritten in place constantly
    /// (`updatedAt`), so a read landing mid-write must not blow away a working source.
    Unknown,
    Same,
    Changed,
}

fn forks(tool: &str) -> bool {
    matches!(tool, "codex" | "agy")
}

pub fn spawn() -> Shared {
    let shared = idle();
    let out = shared.clone();
    let _ = std::thread::Builder::new()
        .name("agent-poll".into())
        .spawn(move || {
            // Read `HOME` once and pass it down: `std::env::set_var` is `unsafe` in edition 2024
            // and races other test threads, so the resolver takes it as a parameter. An absent
            // `HOME` yields an empty path whose reads fail, reported as "no detail".
            let home = std::env::var_os("HOME").map_or_else(PathBuf::new, PathBuf::from);
            let mut cache: HashMap<u32, Entry> = HashMap::new();
            let mut cursor = 0usize;
            loop {
                // Release the lock before any I/O: the render loop takes it to publish `wanted` and
                // read results and must never wait on a file read.
                let wanted: Vec<(u32, String)> = match out.lock() {
                    Ok(g) => g.wanted.iter().take(MAX_TRACKED).cloned().collect(),
                    Err(_) => Vec::new(),
                };
                if !wanted.is_empty() {
                    let seen = sweep(&wanted, &mut cache, &home, &mut cursor);
                    let sources = cache
                        .iter()
                        .filter_map(|(pid, e)| Some((*pid, e.path.clone()?)))
                        .collect();
                    let contexts = cache
                        .iter()
                        .filter_map(|(pid, e)| Some((*pid, e.ctx?)))
                        .collect();
                    if let Ok(mut g) = out.lock() {
                        g.seen = seen;
                        g.sources = sources;
                        g.contexts = contexts;
                    }
                } else if !cache.is_empty() {
                    cache.clear();
                    if let Ok(mut g) = out.lock() {
                        g.seen.clear();
                        g.sources.clear();
                        g.contexts.clear();
                    }
                }
                std::thread::sleep(TICK);
            }
        });
    shared
}

/// One pass over the tracked pids. `cursor` rotates the visit order each tick so that, with more
/// unresolvable entries than [`RESOLVE_BUDGET`], the ones at the front cannot eat every tick's
/// budget and starve those behind them.
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
            ctx: None,
            failed_at: None,
            checked_at: None,
        });
        // The file we were reading was DELETED. Not `Path::exists()`: it maps every `stat` error to
        // `false`, so a permission or I/O hiccup would discard a working source. Only a confirmed
        // `NotFound` counts.
        let vanished = e.path.as_ref().is_some_and(|p| {
            matches!(std::fs::metadata(p), Err(err) if err.kind() == std::io::ErrorKind::NotFound)
        });
        if vanished {
            e.path = None;
            e.stamp = None;
            e.last = None;
            e.ctx = None;
        }
        // The cheap oracle, for the tool that has one; codex's timer is in `want`.
        let changed = !forks(tool)
            && match cheap_source(home, *pid, tool, e.path.as_deref()) {
                Source::Unknown | Source::Same => false,
                Source::Changed => {
                    // A positively different conversation invalidates the reading AT ONCE, even if
                    // the new transcript is not resolvable yet: Claude writes the session-file
                    // entry before the first transcript record exists, and republishing the old
                    // conversation across that window is the bug.
                    e.path = None;
                    e.stamp = None;
                    e.last = None;
                    e.ctx = None;
                    true
                }
            };
        let due = e.failed_at.is_none_or(|t| t.elapsed() >= RESOLVE_RETRY);
        // For a forking tool "we have no path" must not make a re-resolve wanted: an observed
        // absence clears the path, so that would fork `lsof` every tick for a codex pane with no
        // rollout. The timer is the only gate; `checked_at == None` means the first resolve is
        // immediate.
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
                        e.path = found;
                        e.stamp = None;
                        e.last = None;
                        e.ctx = None;
                    }
                }
            }
        }
        let Some(path) = e.path.clone() else { continue };
        let now = std::fs::metadata(&path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        if now.is_some() && now == e.stamp {
            // Unchanged: republish the last reading so an idle agent does not flicker.
            if let Some(a) = e.last.clone() {
                out.insert(*pid, a);
            }
            continue;
        }
        // A read that SUCCEEDED commits both the stamp and the reading, even if the tail held
        // nothing parseable (an inner `None` legitimately blanks the row). A FAILED read leaves
        // `stamp` uncommitted so the next tick retries, and what we had is republished instead of
        // blanking the row; committing it would suppress the pid until mtime or length changed.
        if let Some((detail, mark)) = read_log(&path, tool) {
            e.stamp = now;
            e.last = detail;
            match mark {
                Some(Mark::Used(c)) => e.ctx = Some(c),
                Some(Mark::Reset(Some(used))) => {
                    let window = e.ctx.and_then(|c| c.window);
                    e.ctx = Some(Context { used, window });
                }
                Some(Mark::Reset(None)) => e.ctx = None,
                None => {}
            }
        }
        if let Some(a) = e.last.clone() {
            out.insert(*pid, a);
        }
    }
    out
}

/// Locate the log for an agent pid. The nested `Option` must keep two negatives apart:
///
/// * `None`: could not OBSERVE (session file unreadable, `lsof`/`/proc` enumeration failed, or no
///   known log). The caller keeps what it holds.
/// * `Some(None)`: observed, and there is no log. The caller DROPS what it holds.
///
/// Codex closes its old rollout before opening the new one, so a re-check landing in between sees
/// no rollout; reading that as "could not observe" would pin the previous conversation forever.
fn resolve_log(home: &Path, pid: u32, tool: &str) -> Option<Option<PathBuf>> {
    match tool {
        "claude" => claude_transcript(home, pid),
        "codex" => codex_rollout(pid),
        "grok" => grok_transcript(pid),
        "agy" => agy_transcript(home, pid),
        _ => None,
    }
}

/// The rollout a codex TUI holds open, else the daemon-held one [`crate::codexdaemon`] correlates
/// to it (a daemon-attached TUI holds none), as session restore does.
fn codex_rollout(pid: u32) -> Option<Option<PathBuf>> {
    match crate::agentstate::codex_rollout_path(pid)? {
        Some(own) => Some(Some(own)),
        None => Some(crate::codexdaemon::rollout_path(pid)),
    }
}

/// [`claude_source`] for whichever fork-free tool `tool` is: Claude's session file, or grok's
/// session registry. Either names the conversation the pid is in NOW (Claude `/clear`, grok
/// `/new` both mint a new id in place), compared against the id the held path belongs to.
fn cheap_source(home: &Path, pid: u32, tool: &str, path: Option<&Path>) -> Source {
    match tool {
        "claude" => claude_source(home, pid, path),
        "grok" => match crate::agentstate::grok_session_id(pid) {
            None => Source::Unknown,
            Some(id) if path.and_then(transcript_session_id).as_deref() == Some(id.as_str()) => {
                Source::Same
            }
            Some(_) => Source::Changed,
        },
        _ => Source::Unknown,
    }
}

/// grok's transcript for `pid`. `None` = the registry has no live row for it (cannot observe);
/// `Some(None)` = it names a session whose transcript is not on disk.
fn grok_transcript(pid: u32) -> Option<Option<PathBuf>> {
    let id = crate::agentstate::grok_session_id(pid)?;
    grok_transcript_in(&crate::agentstate::grok_home_of(pid)?.join("sessions"), &id)
}

/// `<sessions>/*/<id>/chat_history.jsonl`. The directory level is grok's encoding of the cwd, so it
/// is enumerated; an entry we could not read makes the answer "cannot observe", as in
/// [`claude_transcript`].
///
/// `NotADirectory` counts as absence: grok keeps `session_search.sqlite` beside the per-cwd
/// folders, and `<that file>/<id>/…` fails with ENOTDIR, which as "cannot observe" blanked every
/// grok pane's detail.
fn grok_transcript_in(sessions: &Path, id: &str) -> Option<Option<PathBuf>> {
    for entry in std::fs::read_dir(sessions).ok()? {
        let candidate = entry.ok()?.path().join(id).join("chat_history.jsonl");
        match std::fs::metadata(&candidate) {
            Ok(m) if m.is_file() => return Some(Some(candidate)),
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(_) => return None,
        }
    }
    Some(None)
}

/// agy's transcript for `pid`, under the conversation [`crate::agentstate::agy_session_id`]
/// resolves (a fork on macOS — the caller budgets it like codex).
fn agy_transcript(home: &Path, pid: u32) -> Option<Option<PathBuf>> {
    let id = crate::agentstate::agy_session_id(pid)?;
    let path = agy_transcript_path(home, &id);
    match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => Some(Some(path)),
        Ok(_) => Some(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(None),
        Err(_) => None,
    }
}

pub(crate) fn agy_transcript_path(home: &Path, id: &str) -> PathBuf {
    home.join(".gemini/antigravity-cli/brain")
        .join(id)
        .join(".system_generated/logs/transcript.jsonl")
}

/// Whether the transcript we hold is still the conversation `pid` is in. The cached path's file
/// stem IS the `sessionId`, so this is a string compare against one small read, never the
/// `read_dir` scan.
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

/// Claude's transcript for `pid`: guess the project slug from the session file's `cwd`, else scan
/// `~/.claude/projects/*` for `<sessionId>.jsonl`. The slug is another program's private encoding,
/// so a guess is used only when the file exists (a missing file looks the same as an agent that ran
/// no tools).
///
/// `Some(None)` is an observed absence (Claude creates the session entry before the first record):
/// the caller must drop the previous transcript.
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
    // The guess was wrong: scan. Not via `flatten()`/`is_file()`: both turn an error into "not this
    // one", so one unreadable entry would make an incomplete listing look like a confirmed absence,
    // and `Some(None)` tells the caller to drop its source.
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

/// Claude's project-directory encoding for a cwd, as observed (separators become `-`). Only a
/// guess, verified by [`claude_transcript`].
fn project_slug(cwd: &str) -> String {
    cwd.replace('/', "-")
}

/// Read the newest activity out of `path`'s tail. `None` = could not read (the caller leaves its
/// stamp uncommitted so the next tick retries); `Some(None)` = read fine but nothing parseable, a
/// cacheable "no activity".
#[cfg(test)]
fn read_detail(path: &Path, tool: &str) -> Option<Option<Activity>> {
    read_log(path, tool).map(|(a, _)| a)
}

/// One read of the log's tail: the activity (as [`read_detail`]) and the NEWEST context
/// mark in it, `None` when the tail holds none.
fn read_log(path: &Path, tool: &str) -> Option<(Option<Activity>, Option<Mark>)> {
    let (budget, parse): (u64, fn(&str) -> Option<String>) = match tool {
        "claude" => (CLAUDE_TAIL, claude_line_detail),
        "codex" => (CODEX_TAIL, codex_line_detail),
        "grok" => (GROK_TAIL, grok_line_detail),
        "agy" => (AGY_TAIL, agy_line_detail),
        _ => return None,
    };
    let mark: fn(&str) -> Option<Mark> = match tool {
        "claude" => claude_line_context,
        "codex" => codex_line_context,
        _ => |_| None,
    };
    let text = tail(path, budget)?;
    let detail = text.lines().rev().find_map(parse).map(|detail| Activity {
        detail: truncate(&detail, MAX_DETAIL),
    });
    Some((detail, text.lines().rev().find_map(mark)))
}

/// One Claude transcript line → its context mark: an assistant response's input-side usage,
/// or a `compact_boundary`. Subagent (sidechain) responses run in their own context and
/// synthetic messages carry zeroed usage, so neither counts.
pub(crate) fn claude_line_context(line: &str) -> Option<Mark> {
    if line.contains("\"compact_boundary\"") {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        if v.get("subtype").and_then(|t| t.as_str()) == Some("compact_boundary") {
            return Some(Mark::Reset(
                v.pointer("/compactMetadata/postTokens")
                    .and_then(|n| n.as_u64()),
            ));
        }
        return None;
    }
    if !line.contains("\"usage\"") || !line.contains("\"assistant\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant")
        || v.get("isSidechain").and_then(|b| b.as_bool()) == Some(true)
        || v.pointer("/message/model").and_then(|m| m.as_str()) == Some("<synthetic>")
    {
        return None;
    }
    let u = v.pointer("/message/usage")?;
    let n = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let used = n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
    (used > 0).then_some(Mark::Used(Context { used, window: None }))
}

/// One codex rollout line → its context mark, from a `token_count` event: the last
/// response's input tokens (cached ones are a subset of them) against the model's window.
fn codex_line_context(line: &str) -> Option<Mark> {
    if !line.contains("\"token_count\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.pointer("/payload/type").and_then(|t| t.as_str()) != Some("token_count") {
        return None;
    }
    let info = v.pointer("/payload/info")?;
    let used = info.pointer("/last_token_usage/input_tokens")?.as_u64()?;
    let window = info
        .get("model_context_window")
        .and_then(|w| w.as_u64())
        .filter(|w| *w > 0);
    Some(Mark::Used(Context { used, window }))
}

/// The last `budget` bytes of `path` as UTF-8, with a leading PARTIAL line discarded (a JSON record
/// cut mid-way can be a valid prefix that parses to something else). `None` means the file could
/// not be READ, nothing else.
fn tail(path: &Path, budget: u64) -> Option<String> {
    tail_from(path, budget).map(|(text, _)| text)
}

/// [`tail`], plus whether the read started past the beginning of the file — i.e. whether
/// anything older was left unread.
pub(crate) fn tail_from(path: &Path, budget: u64) -> Option<(String, bool)> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let from = len.saturating_sub(budget);
    f.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::with_capacity(budget.min(len) as usize);
    f.take(budget).read_to_end(&mut buf).ok()?;
    if from > 0 {
        // Not at the start, so the first line is a fragment. NO newline at all means the budget
        // landed inside one record: nothing complete to parse, but the read SUCCEEDED, and
        // reporting failure would re-read an oversized record every tick forever. Cut in BYTES
        // before decoding: the cut usually lands mid-character, which sent the whole buffer down
        // the lossy copying path.
        let keep = buf
            .iter()
            .position(|&b| b == b'\n')
            .map_or(buf.len(), |i| i + 1);
        buf.drain(..keep);
    }
    let text = String::from_utf8(buf)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    Some((text, from > 0))
}

/// One Claude transcript line → `"Bash: commit the fix"`, or `None` when it names no tool use.
fn claude_line_detail(line: &str) -> Option<String> {
    // Pre-filter before paying for JSON: a line runs to 30 KB.
    if !line.contains("\"tool_use\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let content = v.pointer("/message/content")?.as_array()?;
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

/// One grok `chat_history.jsonl` record → `"run_terminal_command: Create empty probe file"`.
fn grok_line_detail(line: &str) -> Option<String> {
    if !line.contains("\"tool_calls\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let call = v.get("tool_calls")?.as_array()?.last()?;
    let name = call.get("name").and_then(|n| n.as_str())?;
    // `arguments` is a JSON STRING, as in the OpenAI wire format grok mirrors.
    let args = call
        .get("arguments")
        .and_then(|a| a.as_str())
        .and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok());
    let hint = args.as_ref().and_then(|a| {
        [
            "description",
            "command",
            "target_file",
            "file_path",
            "pattern",
            "query",
        ]
        .iter()
        .find_map(|k| a.get(*k).and_then(|x| x.as_str()))
        .map(clean)
        .filter(|h| !h.is_empty())
    });
    Some(match hint {
        Some(h) => format!("{name}: {h}"),
        None => name.to_string(),
    })
}

/// One agy `transcript.jsonl` step → `"run_command: Touch approval-probe.txt"`.
fn agy_line_detail(line: &str) -> Option<String> {
    if !line.contains("\"tool_calls\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("PLANNER_RESPONSE") {
        return None;
    }
    let call = v.get("tool_calls")?.as_array()?.last()?;
    let name = call.get("name").and_then(|n| n.as_str())?;
    let hint = [
        "toolSummary",
        "CommandLine",
        "TargetFile",
        "AbsolutePath",
        "Query",
    ]
    .iter()
    .find_map(|k| call.pointer(&format!("/args/{k}")).and_then(agy_arg))
    .map(|h| clean(&h))
    .filter(|h| !h.is_empty());
    Some(match hint {
        Some(h) => format!("{name}: {h}"),
        None => name.to_string(),
    })
}

/// One agy tool-call argument. agy stores each value JSON-ENCODED inside the string
/// (`"\"touch x\""`), so decode that layer when it is there and keep the raw text otherwise.
pub(crate) fn agy_arg(v: &serde_json::Value) -> Option<String> {
    let raw = v.as_str()?;
    Some(serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string()))
}

/// The human-readable command out of a codex `CommandExecution` item. `command` is an ARRAY
/// (`["/bin/zsh", "-lc", "git status --short"]`) whose LAST element is the script;
/// `parsed_cmd[].cmd` is cleaner and preferred.
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

    #[test]
    fn a_tail_cut_inside_a_character_keeps_only_whole_lines() {
        let path = std::env::temp_dir().join(format!("comux-tail-{}.jsonl", std::process::id()));
        let body = "가나다라마바사\n".repeat(50);
        std::fs::write(&path, &body).unwrap();
        for budget in [1u64, 5, 40, 41, 200] {
            let (text, partial) = tail_from(&path, budget).unwrap();
            assert!(partial);
            assert!(!text.contains('\u{fffd}'), "budget {budget}: {text:?}");
            assert!(
                text.lines().all(|l| l == "가나다라마바사"),
                "budget {budget}"
            );
            assert!(body.ends_with(&text));
        }
        assert_eq!(tail_from(&path, 1 << 20), Some((body.clone(), false)));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn grok_and_agy_conversations_are_named_by_their_session_directory() {
        let grok = Path::new(
            "/h/.grok/sessions/%2Fp/01a16e02-ad06-7380-97f5-03eb32ae7487/chat_history.jsonl",
        );
        let agy = Path::new(
            "/h/.gemini/antigravity-cli/brain/60cc988c-b6b0-4d35-a98f-b4c6e31e449b/.system_generated/logs/transcript.jsonl",
        );
        assert_eq!(
            transcript_session_id(grok).as_deref(),
            Some("01a16e02-ad06-7380-97f5-03eb32ae7487")
        );
        assert_eq!(
            transcript_session_id(agy).as_deref(),
            Some("60cc988c-b6b0-4d35-a98f-b4c6e31e449b")
        );
        assert_eq!(
            transcript_session_id(Path::new("/h/x/chat_history.jsonl")),
            None
        );
    }

    #[test]
    fn grok_tool_calls_read_as_a_detail() {
        // Verbatim grok 1.0.70 chat_history.jsonl records, trimmed of model metadata.
        let call = r#"{"type": "assistant", "content": "I'll create `grok-probe.txt` and confirm when it's done.", "tool_calls": [{"id": "call-cfbf2c5c-905f-4233-a859-78885565fd89-0", "name": "run_terminal_command", "arguments": "{\"command\":\"touch grok-probe.txt\",\"description\":\"Create empty grok-probe.txt file\"}"}], "model_id": "grok-4.7"}"#;
        assert_eq!(
            grok_line_detail(call).as_deref(),
            Some("run_terminal_command: Create empty grok-probe.txt file")
        );
        let read = r#"{"type": "assistant", "content": "", "tool_calls": [{"id": "c", "name": "read_file", "arguments": "{\"target_file\":\"/Users/u/SKILL.md\"}"}]}"#;
        assert_eq!(
            grok_line_detail(read).as_deref(),
            Some("read_file: /Users/u/SKILL.md")
        );
        let reply = r#"{"type": "assistant", "content": "Done.", "model_id": "grok-4.7"}"#;
        assert_eq!(grok_line_detail(reply), None);
        let result = r#"{"type": "tool_result", "tool_call_id": "c", "content": "exit: 0\n"}"#;
        assert_eq!(grok_line_detail(result), None);
    }

    #[test]
    fn agy_tool_calls_read_as_a_detail() {
        // Verbatim agy 1.4.7 transcript.jsonl step — argument values are JSON-encoded strings.
        let step = r#"{"step_index": 1, "source": "MODEL", "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-10-24T13:01:30Z", "input_tokens": 6897, "cache_read_tokens": 0, "output_tokens": 195, "tool_calls": [{"name": "run_command", "args": {"CommandLine": "\"touch approval-probe.txt\"", "WaitMsBeforeAsync": "5000", "toolSummary": "\"Touch approval-probe.txt\""}}]}"#;
        assert_eq!(
            agy_line_detail(step).as_deref(),
            Some("run_command: Touch approval-probe.txt")
        );
        let reply = r#"{"step_index": 3, "source": "MODEL", "type": "PLANNER_RESPONSE", "status": "DONE", "content": "done"}"#;
        assert_eq!(agy_line_detail(reply), None);
    }

    #[test]
    fn a_grok_transcript_is_found_under_any_cwd_directory() {
        let dir = std::env::temp_dir().join(format!("grok-tr-{}", std::process::id()));
        let id = "01a16e02-ad06-7380-97f5-03eb32ae7487";
        let want = dir.join("%2Fp%2Fq").join(id).join("chat_history.jsonl");
        std::fs::create_dir_all(want.parent().unwrap()).unwrap();
        std::fs::create_dir_all(dir.join("%2Fother")).unwrap();
        // grok's own search index sits beside the per-cwd directories.
        std::fs::write(dir.join("session_search.sqlite"), "").unwrap();
        std::fs::write(&want, "{}\n").unwrap();
        assert_eq!(grok_transcript_in(&dir, id), Some(Some(want)));
        assert_eq!(
            grok_transcript_in(&dir, "01a16e02-ad06-7380-97f5-000000000000"),
            Some(None),
            "a listed session with no transcript is an observed absence"
        );
        assert_eq!(grok_transcript_in(&dir.join("missing"), id), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_conversation_is_published_with_its_id_whether_or_not_it_has_a_detail() {
        let shared = idle();
        let claude =
            PathBuf::from("/h/.claude/projects/-x/4f1c2a9e-0000-4000-8000-000000000001.jsonl");
        let codex = PathBuf::from(
            "/h/.codex/sessions/2026/07/24/rollout-2026-07-24T09-59-48-36213526-fd15-4fc9-b146-842f71382088.jsonl",
        );
        {
            let mut g = shared.lock().unwrap();
            g.sources.insert(1, claude.clone());
            g.sources.insert(2, codex.clone());
        }
        assert_eq!(
            conversation(&shared, 1),
            Some(("4f1c2a9e-0000-4000-8000-000000000001".to_string(), claude))
        );
        assert_eq!(
            conversation(&shared, 2),
            Some(("36213526-fd15-4fc9-b146-842f71382088".to_string(), codex))
        );
        assert_eq!(conversation(&shared, 3), None);
        assert_eq!(
            activity(&shared, 1),
            None,
            "no detail is not no conversation"
        );
    }

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
            ctx: None,
            failed_at: None,
            checked_at: None,
        }
    }

    /// A home with no `sessions/`, so the cheap oracle returns [`Source::Unknown`] and a hand-built
    /// path stands.
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

    /// An unchanged transcript must not be re-tailed and re-parsed on every tick. The second pass
    /// runs against a file whose CONTENT is swapped while length and mtime are restored: anything
    /// that re-reads publishes the new content, only the cache publishes the old.
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

    /// A conversation can change inside the SAME pid (Claude `/clear` mints a new `sessionId`)
    /// while the previous transcript stays on disk, so re-resolving only when the cached path
    /// VANISHED would republish the old conversation forever.
    #[test]
    fn a_new_conversation_in_the_same_pid_stops_republishing_the_old_one() {
        let home = FakeHome::new("cleared");
        let old = home.transcript(A, "first");
        home.in_conversation(1, A);
        home.transcript(B, "second");

        let mut cache = HashMap::from([(1u32, entry_for(&old))]);
        assert_eq!(home.sweep(&mut cache)[&1].detail, "Bash: first");

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

    /// Claude writes the session record naming B before B's transcript exists; A's activity must
    /// not be published across that gap.
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
            ctx: None,
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

    /// `wanted` is walked in a fixed order, so the front entries get first claim on
    /// [`RESOLVE_BUDGET`]. Nothing reachable starves today, so this forces the pathological case
    /// (every entry wanting a resolve every tick) to exercise the rotation.
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
        let arr = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":["/bin/zsh","-lc","cargo  test\n--all"]}}}"#;
        assert_eq!(
            codex_line_detail(arr).as_deref(),
            Some("running: cargo test --all"),
            "a multi-line command collapses to one line"
        );
        let scalar = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","command":"ls"}}}"#;
        assert_eq!(codex_line_detail(scalar).as_deref(), Some("running: ls"));
        let think = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning"}}}"#;
        assert_eq!(codex_line_detail(think).as_deref(), Some("thinking"));
        let reply = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"AgentMessage"}}}"#;
        assert_eq!(codex_line_detail(reply).as_deref(), Some("replying"));
        let other = r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Extension"}}}"#;
        assert_eq!(codex_line_detail(other), None);
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
        // Budget lands mid-way through the first line, which must be discarded whole.
        assert_eq!(tail(&f, 12).as_deref(), Some("BBBB\nCCCC\n"));
        assert_eq!(tail(&f, 999).as_deref(), Some("AAAA\nBBBB\nCCCC\n"));

        // A single record LARGER than the budget leaves no complete line, but the read succeeded:
        // that is an empty tail, NOT the `None` that means "could not read" (which would re-read
        // the file every tick).
        let big = dir.join("big.jsonl");
        std::fs::write(&big, "x".repeat(4096)).unwrap();
        assert_eq!(tail(&big, 64).as_deref(), Some(""));

        assert_eq!(tail(&dir.join("absent.jsonl"), 64), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_slug_guess_is_only_ever_a_guess() {
        assert_eq!(project_slug("/Users/me/dev/copad"), "-Users-me-dev-copad");
        assert_eq!(project_slug("/private/tmp"), "-private-tmp");
    }

    #[test]
    fn claude_context_is_the_last_responses_input_side() {
        let used = |line: &str| match claude_line_context(line) {
            Some(Mark::Used(c)) => Some(c),
            _ => None,
        };
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-5-5","usage":{"input_tokens":2,"cache_creation_input_tokens":1038,"cache_read_input_tokens":130167,"output_tokens":662}}}"#;
        assert_eq!(
            used(line),
            Some(Context {
                used: 131207,
                window: None
            })
        );
        // A subagent's response runs in its own window.
        let side = line.replace(
            r#""type":"assistant","#,
            r#""type":"assistant","isSidechain":true,"#,
        );
        assert_eq!(used(&side), None);
        let synthetic = line.replace("claude-opus-5-5", "<synthetic>");
        assert_eq!(used(&synthetic), None);
        let boundary = r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"auto","preTokens":43977,"postTokens":9516}}"#;
        assert_eq!(claude_line_context(boundary), Some(Mark::Reset(Some(9516))));
        let bare = r#"{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":"manual"}}"#;
        assert_eq!(claude_line_context(bare), Some(Mark::Reset(None)));
    }

    #[test]
    fn codex_context_carries_its_window() {
        let line = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":233780},"last_token_usage":{"input_tokens":33649,"cached_input_tokens":33280,"output_tokens":57},"model_context_window":475000}}}"#;
        assert_eq!(
            codex_line_context(line),
            Some(Mark::Used(Context {
                used: 33649,
                window: Some(475000)
            }))
        );
        // Codex emits token_count with a null info before the first response.
        let empty = r#"{"type":"event_msg","payload":{"type":"token_count","info":null}}"#;
        assert_eq!(codex_line_context(empty), None);
    }

    #[test]
    fn a_tail_without_usage_keeps_the_last_reading_and_a_compaction_resets_it() {
        let dir = std::env::temp_dir().join(format!("agentpoll-ctx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.jsonl");
        let usage = |n: u64| {
            format!(
                "{{\"type\":\"assistant\",\"message\":{{\"model\":\"m\",\"usage\":{{\"input_tokens\":{n}}}}}}}\n"
            )
        };
        std::fs::write(&f, usage(5000)).unwrap();
        let mut cache = HashMap::from([(1, entry_for(&f))]);
        let wanted = vec![(1u32, "claude".to_string())];
        let mut cursor = 0;
        sweep(&wanted, &mut cache, no_home(), &mut cursor);
        assert_eq!(
            cache[&1].ctx,
            Some(Context {
                used: 5000,
                window: None
            })
        );
        let mut text = usage(5000);
        text.push_str("{\"type\":\"user\",\"message\":{\"content\":\"x\"}}\n");
        std::fs::write(&f, &text).unwrap();
        sweep(&wanted, &mut cache, no_home(), &mut cursor);
        assert_eq!(cache[&1].ctx.map(|c| c.used), Some(5000));
        text.push_str("{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"compactMetadata\":{\"postTokens\":900}}\n");
        std::fs::write(&f, &text).unwrap();
        sweep(&wanted, &mut cache, no_home(), &mut cursor);
        assert_eq!(cache[&1].ctx.map(|c| c.used), Some(900));
        let _ = std::fs::remove_dir_all(&dir);
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
