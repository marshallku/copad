//! Past agent conversations on disk — the data source behind the `Ctrl-b R` resume
//! picker (decision #99).
//!
//! [`agentstate`](crate::agentstate) answers "what is the agent in THIS pane doing"; this
//! module answers the opposite question: "which conversations exist that no pane is running
//! any more?" Both CLIs keep every transcript, so closing a pane never loses the session —
//! it only loses the id, and neither `claude --resume` (per-cwd) nor `codex resume` is
//! reachable from the mux. We enumerate the transcripts ourselves.
//!
//! - Claude: `~/.claude/projects/<cwd-slug>/<uuid>.jsonl`. Records carry `cwd`, `timestamp`
//!   and `entrypoint`; the first `type: "user"` record whose `message.content` is a plain
//!   string is the prompt that started the conversation (its title). Subdirectories hold
//!   subagent transcripts and are skipped.
//! - Codex: `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<uuid>.jsonl`. Line 1 is a
//!   `session_meta` record with `session_id` / `cwd` / `originator`; the first user prompt is
//!   much deeper (a multi-KB developer preamble precedes it).
//!
//! `~/.claude/history.jsonl` looks like a cheaper index and is NOT used: on the owner's
//! machine only 59 of its 223 session ids still have a transcript, and 638 transcripts are
//! absent from it entirely — it is a prompt log with its own retention, not a session index.
//!
//! Everything here is bounded: transcripts are unbounded append-only logs written by another
//! program, so every read is capped per file AND across the scan, only regular files are
//! opened (a symlinked FIFO would otherwise block the scanner thread forever), and the
//! scan itself always runs off the render loop (see [`spawn_scan`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Which CLI wrote a transcript — decides the resume command and the row's badge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Claude,
    Codex,
}

impl Tool {
    /// The binary name, which is also the label shown in the picker.
    pub fn bin(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
        }
    }
}

/// One resumable conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub tool: Tool,
    /// The conversation id passed to `--resume` / `resume`. Always a validated UUID.
    pub id: String,
    /// Where the conversation ran. `None` when the transcript never recorded one.
    pub cwd: Option<PathBuf>,
    /// The first user prompt, collapsed to one line. Empty when none was found within the
    /// read budget (the row then shows the path alone rather than a wrong title).
    pub title: String,
    /// Last write to the transcript — the recency the picker sorts on.
    pub mtime: SystemTime,
    /// A non-interactive run (`claude -p` / SDK, `codex exec`, an agent driving another
    /// agent). Hidden by default; `Ctrl-a` in the picker reveals them.
    pub headless: bool,
}

/// A completed scan. `req` matches the request that asked for it, so a slow scan that
/// lands after a newer one can be dropped instead of overwriting it (`gen` is a reserved
/// keyword in edition 2024).
#[derive(Debug, Clone)]
pub struct Scan {
    pub req: u64,
    pub entries: Vec<Entry>,
}

/// Newest transcripts kept. Applied AFTER the mtime sort, so the cap drops the oldest
/// conversations rather than an arbitrary directory-order slice.
const MAX_ENTRIES: usize = 4000;
/// Bytes read per transcript while looking for the title. Generous because the prompt sits
/// near the top of a Claude transcript but up to several hundred KB into a Codex one (the
/// developer preamble precedes it), and because a Codex `session_meta` line alone embeds the
/// model's base instructions (~22 KB observed).
const MAX_FILE_SCAN: u64 = 1024 * 1024;
/// Bytes read across the whole scan while hunting for TITLES. Past this, remaining rows fall
/// back to the metadata budget below and lose only their title.
const MAX_TOTAL_SCAN: u64 = 256 * 1024 * 1024;
/// Bytes read per transcript, and across the scan, once the title budget is spent. Metadata
/// (`cwd` + interactive-vs-headless) has its OWN budget because it decides whether a row is
/// even LISTED by default and which space it opens in — degrading it would quietly show
/// automation as interactive and resume it in the wrong directory, which looks like a correct
/// list. Both signals sit in the first records of either format, so this cap is generous.
const META_FILE_SCAN: u64 = 128 * 1024;
const MAX_TOTAL_META_SCAN: u64 = 128 * 1024 * 1024;
/// A user record big enough that it is almost certainly a tool result, not a prompt. Such a
/// line is only JSON-parsed when it carries `promptSource` (which tool results do not), so a
/// multi-MB tool result never costs a full deserialize.
const BIG_LINE: usize = 64 * 1024;
/// Chars kept from a prompt for the row label.
const MAX_TITLE: usize = 200;
/// Directory-walk budget for `~/.codex/sessions` (`YYYY/MM/DD` is 3 levels).
const MAX_DEPTH: usize = 4;
/// Directories visited by the Codex walk, and transcripts kept per tool.
const MAX_DIRS: usize = 4000;
const MAX_FILES: usize = 20_000;
/// Directory entries LOOKED AT per tool, whether or not they turn out to be transcripts.
/// `MAX_FILES` alone bounds only what is accepted, so a directory stuffed with junk would
/// still be walked in full — and this runs on a thread the mux never waits for.
const MAX_VISITS: usize = 200_000;

/// Non-interactive Claude `entrypoint` values. A DENYLIST on purpose (decision #98's
/// precedent): an entrypoint we have never seen counts as interactive, so a rename upstream
/// makes sessions appear in the default view rather than silently vanish from it.
const CLAUDE_HEADLESS: &[&str] = &["sdk-cli", "sdk-ts", "sdk-py", "print"];
/// Non-interactive Codex `originator` values: `codex_exec` is `codex exec`, and
/// `Claude Code` is Codex driven by a Claude Code integration.
pub(crate) const CODEX_HEADLESS: &[&str] = &["codex_exec", "Claude Code"];

/// The handle the picker reads. `None` = no scan has completed yet ("scanning…").
pub type Shared = Arc<Mutex<Option<Scan>>>;

/// An empty handle with no scanner behind it (the default before the first open, and in
/// tests that build an `App` without one).
pub fn idle() -> Shared {
    Arc::new(Mutex::new(None))
}

/// Run one scan on a detached thread and publish it into `out` under `req`. The scan walks
/// two directory trees and reads up to [`MAX_TOTAL_SCAN`] bytes, so it must never run on the
/// server's render loop. A result is dropped if a newer generation already landed.
pub fn spawn_scan(out: Shared, req: u64, home: PathBuf, deep: bool) {
    let _ = std::thread::Builder::new()
        .name("agent-session-scan".into())
        .spawn(move || {
            let entries = scan(&home, deep);
            if let Ok(mut g) = out.lock() {
                if g.as_ref().is_some_and(|prev| prev.req > req) {
                    return; // a newer scan already published — this one is stale
                }
                *g = Some(Scan { req, entries });
            }
        });
}

/// Every resumable conversation under `home`, newest first.
///
/// `deep` also reads a TITLE for headless transcripts. It is off for the picker's default
/// view because those rows are hidden there, and they are the overwhelming majority (1534 of
/// 1595 on the owner's machine) — their prompts sit hundreds of KB into the file, so skipping
/// them is the difference between ~0.3 s and ~2.6 s of background I/O. Every row is still
/// listed either way; only the title of a hidden row waits for the `Ctrl-a` rescan.
pub fn scan(home: &Path, deep: bool) -> Vec<Entry> {
    let mut found: Vec<(Tool, PathBuf, String, SystemTime)> = claude_files(home);
    found.extend(codex_files(home));
    // Sort BEFORE truncating so the cap drops the oldest, and parse in the same order so the
    // read budget is spent on the transcripts the user is most likely looking for.
    found.sort_by_key(|f| std::cmp::Reverse(f.3));
    found.truncate(MAX_ENTRIES);

    let mut budget = Budget {
        titles: MAX_TOTAL_SCAN,
        meta: MAX_TOTAL_META_SCAN,
    };
    found
        .into_iter()
        .map(|(tool, path, id, mtime)| {
            let meta = match tool {
                Tool::Claude => parse_claude(&path, &mut budget, deep),
                Tool::Codex => parse_codex(&path, &mut budget, deep),
            };
            Entry {
                tool,
                id,
                cwd: meta.cwd,
                title: meta.title,
                mtime,
                headless: meta.headless,
            }
        })
        .collect()
}

/// The scan's two read allowances. They are separate so that exhausting the title budget on a
/// pathological directory costs titles only — never a row's classification or directory.
struct Budget {
    titles: u64,
    meta: u64,
}

impl Budget {
    /// Whether the title allowance still has enough left to be worth using. The threshold is a
    /// WHOLE metadata read, not zero: a file handed the last few KB of the title budget could
    /// run out before reaching its `entrypoint`/`session_meta` record and come back
    /// unclassified, which is exactly the degradation the reserve exists to prevent.
    fn on_titles(&self) -> bool {
        self.titles >= META_FILE_SCAN
    }

    /// What this transcript may read. The choice is made ONCE per file and carried in the
    /// grant, so a long title read that drains `titles` mid-file cannot start eating the
    /// metadata reserve — the reserve has to survive for the transcripts that come after.
    fn allow(&self) -> Grant {
        if self.on_titles() {
            Grant {
                cap: MAX_FILE_SCAN.min(self.titles),
                want_title: true,
                from_titles: true,
            }
        } else {
            Grant {
                cap: META_FILE_SCAN.min(self.meta),
                want_title: false,
                from_titles: false,
            }
        }
    }

    /// Charge `n` bytes actually read to the allowance `allow` handed out.
    fn charge(&mut self, from_titles: bool, n: u64) {
        if from_titles {
            self.titles = self.titles.saturating_sub(n);
        } else {
            self.meta = self.meta.saturating_sub(n);
        }
    }
}

/// One transcript's read allowance: how many bytes, whether a title is worth looking for, and
/// which budget pays for it.
struct Grant {
    cap: u64,
    want_title: bool,
    from_titles: bool,
}

/// A reader that charges the scan budget for every byte it hands over.
///
/// Charging per decoded LINE instead would leak: `lines()` stops at the first non-UTF-8 record
/// and the bytes consumed getting there are never billed, so a directory of binary junk could
/// read its per-file cap thousands of times over inside a "bounded" scan.
struct Metered<'a, R> {
    inner: R,
    budget: &'a mut Budget,
    from_titles: bool,
}

impl<R: std::io::Read> std::io::Read for Metered<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.budget.charge(self.from_titles, n as u64);
        Ok(n)
    }
}

/// What a transcript's contents tell us beyond its path.
#[derive(Default)]
struct Meta {
    cwd: Option<PathBuf>,
    title: String,
    headless: bool,
}

/// `~/.claude/projects/<slug>/<uuid>.jsonl`. One level only — a subdirectory under a project
/// holds subagent transcripts, which are not separately resumable.
fn claude_files(home: &Path) -> Vec<(Tool, PathBuf, String, SystemTime)> {
    let mut out = Vec::new();
    let mut visits = 0usize;
    let Ok(projects) = std::fs::read_dir(home.join(".claude").join("projects")) else {
        return out;
    };
    // Count every entry the iterator yields, INCLUDING errors: a concurrently mutated or
    // hostile tree can produce unbounded `ReadDir` errors, and `.flatten()` would drop them
    // before they were ever metered.
    for project in projects {
        visits += 1;
        if out.len() >= MAX_FILES || visits >= MAX_VISITS {
            break;
        }
        let Ok(project) = project else { continue };
        // Only descend into a REAL directory: a symlink under `projects/` would otherwise
        // redirect the walk out of the transcript tree (onto a network mount, say), and this
        // runs on a thread the mux never waits for. Same rule as the Codex walk.
        let path = project.path();
        if !std::fs::symlink_metadata(&path).is_ok_and(|md| md.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&path) else {
            continue;
        };
        for f in files {
            visits += 1;
            if out.len() >= MAX_FILES || visits >= MAX_VISITS {
                break;
            }
            let Ok(f) = f else { continue };
            let path = f.path();
            let Some(id) = transcript_id(&path, "") else {
                continue;
            };
            let Some(mtime) = regular_file_mtime(&path) else {
                continue;
            };
            out.push((Tool::Claude, path, id, mtime));
        }
    }
    out
}

/// `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<ts>-<uuid>.jsonl`, walked depth-first with an
/// explicit depth/dir budget (an unbounded walk of a user-writable tree is a hang waiting to
/// happen).
fn codex_files(home: &Path) -> Vec<(Tool, PathBuf, String, SystemTime)> {
    let mut out = Vec::new();
    let mut stack = vec![(home.join(".codex").join("sessions"), 0usize)];
    let mut dirs = 0usize;
    let mut visits = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_DEPTH || dirs >= MAX_DIRS || out.len() >= MAX_FILES || visits >= MAX_VISITS {
            continue;
        }
        dirs += 1;
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for f in rd {
            visits += 1;
            if out.len() >= MAX_FILES || visits >= MAX_VISITS {
                break;
            }
            let Ok(f) = f else { continue };
            let path = f.path();
            // `symlink_metadata`: never descend through a symlink, and never open one.
            let Ok(md) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if md.is_dir() {
                // Bound the QUEUE as well: pushing every directory first would let a wide tree
                // grow the stack without limit before the `MAX_DIRS` check ever refuses one.
                if dirs + stack.len() < MAX_DIRS {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !md.is_file() {
                continue;
            }
            let Some(id) = transcript_id(&path, "rollout-") else {
                continue;
            };
            let Ok(mtime) = md.modified() else { continue };
            out.push((Tool::Codex, path, id, mtime));
        }
    }
    out
}

/// The conversation id from a transcript filename: the stem with `prefix` stripped, and for
/// Codex the trailing UUID of `rollout-<timestamp>-<uuid>`. `None` unless the result is a
/// real UUID — an id lands on a command line, so it is validated, never guessed.
fn transcript_id(path: &Path, prefix: &str) -> Option<String> {
    if path.extension()? != "jsonl" {
        return None;
    }
    let stem = path.file_stem()?.to_str()?.strip_prefix(prefix)?;
    // `rollout-2026-09-06T23-07-50-<uuid>` → the last 5 dash-groups.
    let parts: Vec<&str> = stem.split('-').collect();
    let id = if parts.len() >= 5 {
        parts[parts.len() - 5..].join("-")
    } else {
        stem.to_string()
    };
    crate::agentstate::is_session_uuid(&id).then_some(id)
}

/// `mtime` of `path` when it is a REGULAR file — never a symlink, FIFO or device. This is the
/// enumeration filter; the read itself re-checks the type on its own descriptor
/// (see [`bounded_lines`]), since a path check can be raced.
fn regular_file_mtime(path: &Path) -> Option<SystemTime> {
    let md = std::fs::symlink_metadata(path).ok()?;
    md.is_file().then(|| md.modified().ok()).flatten()
}

/// A bounded line reader over a transcript: at most `cap` bytes from a REGULAR file.
///
/// `budget` is charged by the bytes ACTUALLY read (see [`Metered`]), not by the per-file cap —
/// charging the cap up front exhausts the allowance after a few hundred files and silently
/// degrades every later row (each transcript is parsed lazily and most stop short of its cap).
///
/// The open is `O_NOFOLLOW | O_NONBLOCK` and the file type is re-checked on the DESCRIPTOR, not
/// on the path: a `symlink_metadata` check followed by a plain open is a race a hostile tree
/// can win by swapping the file for a FIFO in between, and opening a FIFO blocks forever on a
/// thread nobody joins.
fn bounded_lines<'a>(
    path: &Path,
    grant: &Grant,
    budget: &'a mut Budget,
) -> Option<impl Iterator<Item = String> + 'a> {
    use std::io::BufRead;
    use std::os::unix::fs::OpenOptionsExt;
    if grant.cap == 0 {
        return None;
    }
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let metered = Metered {
        inner: std::io::Read::take(f, grant.cap),
        budget,
        from_titles: grant.from_titles,
    };
    Some(
        std::io::BufReader::new(metered)
            .lines()
            .map_while(Result::ok),
    )
}

/// Claude transcript → cwd + title + interactivity. Bails as soon as both the `entrypoint`
/// (which decides `headless`) and a title are known.
fn parse_claude(path: &Path, budget: &mut Budget, deep: bool) -> Meta {
    let mut meta = Meta::default();
    let mut origin_known = false;
    let grant = budget.allow();
    let want_title = grant.want_title;
    let Some(lines) = bounded_lines(path, &grant, budget) else {
        return meta;
    };
    for line in lines {
        // Every record that carries `cwd` carries `entrypoint` too, so one parse settles
        // both; after that only user records can still add anything.
        if !origin_known
            && line.contains("\"entrypoint\"")
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
        {
            if let Some(e) = v.get("entrypoint").and_then(|e| e.as_str()) {
                meta.headless = CLAUDE_HEADLESS.contains(&e);
                origin_known = true;
            }
            if meta.cwd.is_none() {
                meta.cwd = v
                    .get("cwd")
                    .and_then(|c| c.as_str())
                    .filter(|c| !c.is_empty())
                    .map(PathBuf::from);
            }
        }
        if want_title
            && meta.title.is_empty()
            && line.contains("\"type\":\"user\"")
            // A prompt record carries `promptSource`; a tool-result record does not. Skipping
            // big lines without it keeps a multi-MB tool result from being deserialized.
            && (line.len() < BIG_LINE || line.contains("\"promptSource\""))
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
            && v.get("type").and_then(|t| t.as_str()) == Some("user")
            && v.get("isSidechain").and_then(|s| s.as_bool()) != Some(true)
            // Only a plain-string content is a typed prompt; an array is a tool result.
            && let Some(text) = v.pointer("/message/content").and_then(|c| c.as_str())
            && let Some(title) = prompt_title(text)
        {
            meta.title = title;
        }
        // Everything known — or the title isn't wanted at all (the transcript turned out to be
        // headless and `deep` is off, or the title budget is spent), so classification and cwd
        // are all that is left to find.
        if origin_known && (!meta.title.is_empty() || !want_title || (meta.headless && !deep)) {
            break;
        }
    }
    meta
}

/// Codex rollout → cwd + title + interactivity. Line 1 (`session_meta`) has the metadata;
/// the first real user prompt follows the developer preamble further down.
fn parse_codex(path: &Path, budget: &mut Budget, deep: bool) -> Meta {
    let mut meta = Meta::default();
    let grant = budget.allow();
    let want_title = grant.want_title;
    let Some(mut lines) = bounded_lines(path, &grant, budget) else {
        return meta;
    };
    if let Some(first) = lines.next()
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&first)
    {
        meta.cwd = v
            .pointer("/payload/cwd")
            .and_then(|c| c.as_str())
            .filter(|c| !c.is_empty())
            .map(PathBuf::from);
        // An originator we don't know is treated as interactive (see CODEX_HEADLESS).
        meta.headless = v
            .pointer("/payload/originator")
            .and_then(|o| o.as_str())
            .is_some_and(|o| CODEX_HEADLESS.contains(&o));
    }
    if !want_title || (meta.headless && !deep) {
        return meta; // hidden row — don't pay for a prompt buried under the preamble
    }
    for line in lines {
        if !line.contains("\"role\":\"user\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("response_item")
            || v.pointer("/payload/role").and_then(|r| r.as_str()) != Some("user")
        {
            continue;
        }
        let text: String = v
            .pointer("/payload/content")
            .and_then(|c| c.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        // Codex injects context as tag blocks (`<environment_context>…`) in the user role.
        if let Some(title) = prompt_title(&text) {
            meta.title = title;
            break;
        }
    }
    meta
}

/// A user record's text as a row title, or `None` when it is not the user's own words and
/// the caller should keep looking.
///
/// Both CLIs put machine-generated context in the user role, wrapped in a tag block
/// (`<environment_context>`, `<local-command-caveat>`, …). Those are skipped — except a slash
/// command, whose `<command-name>` IS what the user typed (`/catchup`), so it becomes the
/// title instead of the caveat that follows it.
fn prompt_title(text: &str) -> Option<String> {
    if is_tag_block(text) {
        let cmd = tag_value(text, "command-name")?;
        return Some(clean_title(&cmd)).filter(|t| !t.is_empty());
    }
    Some(clean_title(text)).filter(|t| !t.is_empty())
}

/// The contents of the first `<name>…</name>` element in `text`, if present.
fn tag_value(text: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].to_string())
}

/// Whether `text` is an injected tag block rather than a prompt: it opens with `<` and its
/// first line closes that tag.
fn is_tag_block(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with('<')
        && t.lines()
            .next()
            .is_some_and(|l| l.trim_end().ends_with('>'))
}

/// Collapse a prompt to one bounded line: whitespace runs (including newlines) become single
/// spaces, and the result is cut to [`MAX_TITLE`] chars.
fn clean_title(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        if out.chars().count() >= MAX_TITLE {
            break;
        }
    }
    out.chars().take(MAX_TITLE).collect()
}

/// Age as a compact, stable label (`now` / `12m` / `3h` / `2d` / `5w`). Buckets coarsely on
/// purpose: the picker's frame must not recompose every second.
pub fn fmt_age(secs: u64) -> String {
    match secs {
        0..60 => "now".into(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        86_400..604_800 => format!("{}d", secs / 86_400),
        _ => format!("{}w", secs / 604_800),
    }
}

/// The argv that resumes conversation `id` in a fresh shell. The per-tool command shape
/// (`claude --resume <id>` vs `codex resume <id>`) lives in [`crate::agentstate`], shared
/// with the persisted-restore path.
pub fn resume_argv(tool: Tool, id: &str) -> Vec<String> {
    crate::agentstate::resume_argv(tool.bin(), &[tool.bin().to_string()], id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CID: &str = "cb586121-d8cd-4c86-8011-132f3917967f";

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("copad-agentsessions-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn transcript_id_validates_and_strips() {
        let claude = PathBuf::from(format!("/p/{CID}.jsonl"));
        assert_eq!(transcript_id(&claude, "").as_deref(), Some(CID));
        let codex = PathBuf::from(format!("/s/rollout-2026-09-06T23-07-50-{CID}.jsonl"));
        assert_eq!(transcript_id(&codex, "rollout-").as_deref(), Some(CID));
        // Not a UUID / wrong extension / wrong prefix → rejected rather than guessed.
        assert_eq!(transcript_id(Path::new("/p/notes.jsonl"), ""), None);
        assert_eq!(transcript_id(&claude, "rollout-"), None);
        assert_eq!(
            transcript_id(Path::new(&format!("/p/{CID}.json")), ""),
            None
        );
    }

    #[test]
    fn claude_transcript_yields_cwd_title_and_interactivity() {
        let home = tmpdir("claude");
        let f = home
            .join(".claude/projects/-Users-me-dev-copad")
            .join(format!("{CID}.jsonl"));
        write(
            &f,
            &format!(
                concat!(
                    "{{\"type\":\"mode\",\"sessionId\":\"{id}\"}}\n",
                    "{{\"type\":\"attachment\",\"cwd\":\"/Users/me/dev/copad\",\"entrypoint\":\"cli\"}}\n",
                    "{{\"type\":\"user\",\"promptSource\":\"typed\",\"message\":{{\"role\":\"user\",\"content\":\"세션  목록\\n만들어줘\"}}}}\n",
                    "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"later prompt\"}}}}\n",
                ),
                id = CID
            ),
        );
        let e = scan(&home, true);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].tool, Tool::Claude);
        assert_eq!(e[0].id, CID);
        assert_eq!(e[0].cwd.as_deref(), Some(Path::new("/Users/me/dev/copad")));
        // Newlines collapsed, FIRST prompt wins (it is the conversation's identity).
        assert_eq!(e[0].title, "세션 목록 만들어줘");
        assert!(!e[0].headless);
    }

    #[test]
    fn claude_sdk_entrypoint_is_headless_and_tool_results_are_not_titles() {
        let home = tmpdir("claude-sdk");
        let f = home
            .join(".claude/projects/-Users-me-bots")
            .join(format!("{CID}.jsonl"));
        write(
            &f,
            concat!(
                "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"content\":\"ls output\"}]}}\n",
                "{\"type\":\"attachment\",\"cwd\":\"/Users/me/bots\",\"entrypoint\":\"sdk-cli\"}\n",
                "{\"type\":\"user\",\"isSidechain\":true,\"message\":{\"role\":\"user\",\"content\":\"subagent prompt\"}}\n",
                "{\"type\":\"user\",\"promptSource\":\"sdk\",\"message\":{\"role\":\"user\",\"content\":\"run the loop\"}}\n",
            ),
        );
        let e = scan(&home, true);
        assert_eq!(e.len(), 1);
        assert!(e[0].headless);
        // An array content (tool result) and a sidechain record are both skipped.
        assert_eq!(e[0].title, "run the loop");
    }

    #[test]
    fn codex_rollout_skips_tag_blocks_and_reads_meta() {
        let home = tmpdir("codex");
        let f = home
            .join(".codex/sessions/2026/09/06")
            .join(format!("rollout-2026-09-06T23-07-50-{CID}.jsonl"));
        write(
            &f,
            &format!(
                concat!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{id}\",\"cwd\":\"/Users/me/dev/infra\",\"originator\":\"codex-tui\"}}}}\n",
                    "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"developer\",\"content\":[{{\"text\":\"you are codex\"}}]}}}}\n",
                    "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"text\":\"<environment_context>\\ncwd=/x\\n</environment_context>\"}}]}}}}\n",
                    "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"text\":\"fix the deploy\"}}]}}}}\n",
                ),
                id = CID
            ),
        );
        let e = scan(&home, true);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].tool, Tool::Codex);
        assert_eq!(e[0].id, CID);
        assert_eq!(e[0].cwd.as_deref(), Some(Path::new("/Users/me/dev/infra")));
        assert_eq!(e[0].title, "fix the deploy");
        assert!(!e[0].headless);
        assert_eq!(
            resume_argv(e[0].tool, &e[0].id),
            vec!["codex".to_string(), "resume".into(), CID.into()]
        );
    }

    #[test]
    fn codex_exec_and_unknown_originators() {
        let home = tmpdir("codex-origin");
        let mk = |id: &str, originator: &str| {
            let f = home
                .join(".codex/sessions/2026/09/06")
                .join(format!("rollout-2026-09-06T23-07-50-{id}.jsonl"));
            write(
                &f,
                &format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"session_id\":\"{id}\",\"originator\":\"{originator}\"}}}}\n"
                ),
            );
        };
        mk(CID, "codex_exec");
        let other = "36213526-fd15-4fc9-b146-842f71382088";
        mk(other, "some-future-tui");
        let e = scan(&home, true);
        assert_eq!(e.len(), 2);
        let by_id = |id: &str| e.iter().find(|x| x.id == id).unwrap();
        assert!(by_id(CID).headless);
        // Unknown originator → treated as interactive, so it can never silently disappear.
        assert!(!by_id(other).headless);
    }

    #[test]
    fn an_exhausted_title_budget_still_classifies_and_locates() {
        let home = tmpdir("budget");
        let f = home
            .join(".claude/projects/-p")
            .join(format!("{CID}.jsonl"));
        write(
            &f,
            concat!(
                "{\"type\":\"attachment\",\"cwd\":\"/Users/me/bots\",\"entrypoint\":\"sdk-cli\"}\n",
                "{\"type\":\"user\",\"promptSource\":\"sdk\",\"message\":{\"role\":\"user\",\"content\":\"run it\"}}\n",
            ),
        );
        // Titles spent, metadata reserve intact: the row must still be classified headless and
        // keep its directory — those decide whether it is listed at all and where Enter opens
        // it, so degrading them would look like a correct list while behaving wrongly.
        let mut spent = Budget {
            titles: 0,
            meta: MAX_TOTAL_META_SCAN,
        };
        let meta = parse_claude(&f, &mut spent, true);
        assert!(meta.headless);
        assert_eq!(meta.cwd.as_deref(), Some(Path::new("/Users/me/bots")));
        assert_eq!(meta.title, "");
        // The SAME degradation must apply at the boundary, not only at exactly zero: a file
        // handed the last few KB of the title budget could run out before its `entrypoint`
        // record and come back unclassified.
        let mut nearly = Budget {
            titles: 1024,
            meta: MAX_TOTAL_META_SCAN,
        };
        let meta = parse_claude(&f, &mut nearly, true);
        assert!(meta.headless);
        assert_eq!(meta.cwd.as_deref(), Some(Path::new("/Users/me/bots")));
        assert_eq!(
            nearly.titles, 1024,
            "the reserve paid, not the title budget"
        );

        // And the choice is sticky per file: a title read that drains `titles` mid-file must
        // not spill into the reserve that LATER transcripts depend on.
        let mut draining = Budget {
            titles: META_FILE_SCAN + 8,
            meta: MAX_TOTAL_META_SCAN,
        };
        parse_claude(&f, &mut draining, true);
        assert_eq!(draining.meta, MAX_TOTAL_META_SCAN);

        // With both budgets alive the title comes back.
        let mut full = Budget {
            titles: MAX_TOTAL_SCAN,
            meta: MAX_TOTAL_META_SCAN,
        };
        assert_eq!(parse_claude(&f, &mut full, true).title, "run it");
    }

    #[test]
    fn a_symlinked_project_directory_is_not_followed() {
        let home = tmpdir("symlink");
        let real = home.join("elsewhere");
        std::fs::create_dir_all(&real).unwrap();
        write(
            &real.join(format!("{CID}.jsonl")),
            "{\"type\":\"attachment\",\"cwd\":\"/p\",\"entrypoint\":\"cli\"}\n",
        );
        let projects = home.join(".claude/projects");
        std::fs::create_dir_all(&projects).unwrap();
        std::os::unix::fs::symlink(&real, projects.join("-p")).unwrap();
        // Following it would let a hostile tree redirect the walk anywhere (a network mount
        // that blocks, for instance) on a thread nobody joins.
        assert!(scan(&home, true).is_empty());
    }

    #[test]
    fn a_fifo_is_refused_instead_of_blocking_the_scanner() {
        let home = tmpdir("fifo");
        let dir = home.join(".claude/projects/-p");
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join(format!("{CID}.jsonl"));
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a plain libc call on a path inside this test's own temp dir.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        let mut budget = Budget {
            titles: MAX_TOTAL_SCAN,
            meta: MAX_TOTAL_META_SCAN,
        };
        // Opening a FIFO for reading blocks until a writer appears; there is none, so a scan
        // that reached this file would hang forever on a thread nobody joins.
        let grant = budget.allow();
        assert!(bounded_lines(&fifo, &grant, &mut budget).is_none());
        // And it never reaches the reader anyway — enumeration drops non-regular files.
        assert!(scan(&home, true).is_empty());
    }

    #[test]
    fn scan_sorts_newest_first_across_tools() {
        let home = tmpdir("order");
        let a = "36213526-fd15-4fc9-b146-842f71382088";
        write(
            &home.join(format!(".claude/projects/-p/{a}.jsonl")),
            "{\"type\":\"attachment\",\"cwd\":\"/p\",\"entrypoint\":\"cli\"}\n",
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(
            &home.join(format!(
                ".codex/sessions/2026/09/06/rollout-2026-09-06T23-07-50-{CID}.jsonl"
            )),
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/q\"}}\n",
        );
        let e = scan(&home, true);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].id, CID, "newest transcript first");
        assert_eq!(e[1].id, a);
    }

    #[test]
    fn missing_dirs_and_junk_files_are_ignored() {
        let home = tmpdir("junk");
        assert!(scan(&home, true).is_empty());
        write(&home.join(".claude/projects/-p/README.md"), "hi\n");
        write(&home.join(".claude/projects/-p/not-a-uuid.jsonl"), "{}\n");
        // A subagent transcript under a session directory is not separately resumable.
        write(
            &home.join(format!(".claude/projects/-p/{CID}/{CID}.jsonl")),
            "{}\n",
        );
        assert!(scan(&home, true).is_empty());
    }

    #[test]
    fn title_is_collapsed_and_bounded() {
        assert_eq!(clean_title("  a\n\n b \t c "), "a b c");
        assert_eq!(clean_title("").len(), 0);
        let long = "가".repeat(500);
        assert_eq!(clean_title(&long).chars().count(), MAX_TITLE);
    }

    #[test]
    fn prompt_title_skips_wrappers_but_keeps_a_slash_command() {
        // An injected context block is not the user's words → keep looking.
        assert_eq!(
            prompt_title("<environment_context>\ncwd=/x\n</environment_context>"),
            None
        );
        assert_eq!(
            prompt_title(
                "<local-command-caveat>Caveat: the messages below…</local-command-caveat>"
            ),
            None
        );
        // A slash command's own name IS the prompt.
        assert_eq!(
            prompt_title(
                "<command-message>catchup</command-message>\n<command-name>/catchup</command-name>"
            )
            .as_deref(),
            Some("/catchup")
        );
        assert_eq!(
            prompt_title("fix the deploy").as_deref(),
            Some("fix the deploy")
        );
        assert_eq!(prompt_title("   \n  "), None);
    }

    #[test]
    fn tag_block_detection() {
        assert!(is_tag_block(
            "<environment_context>\nfoo\n</environment_context>"
        ));
        assert!(is_tag_block("  <user_instructions>\nx"));
        // A prompt that merely starts with `<` is a prompt.
        assert!(!is_tag_block("<- this arrow means something"));
        assert!(!is_tag_block("fix the deploy"));
    }

    #[test]
    fn age_labels_bucket() {
        assert_eq!(fmt_age(5), "now");
        assert_eq!(fmt_age(90), "1m");
        assert_eq!(fmt_age(7_200), "2h");
        assert_eq!(fmt_age(200_000), "2d");
        assert_eq!(fmt_age(2_000_000), "3w");
    }
}
