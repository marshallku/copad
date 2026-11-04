//! The work graph of one agent conversation (decisions #204): which files it read, edited,
//! wrote and searched, the commands it ran, the subagents it spawned, and the order it did it
//! in — built from the agent's own transcript, kept on disk, and sliced for the L0 `graph`
//! topic and the `graph` command.
//!
//! **Sources.** Claude: the transcript `agentpoll` resolved (`assistant` `tool_use` blocks,
//! settled by the matching `tool_result`), plus each subagent's own transcript under
//! `<session>/subagents/agent-*.jsonl`, found by its `.meta.json` (`toolUseId` names the
//! spawning call). Codex: the rollout's completed `CommandExecution` items (`parsed_cmd` says
//! which reads and searches a command was), the results of `apply_patch` tool calls (`Success. Updated the
//! following files:`), `UserMessage` and `task_complete`. Codex subagents are not seen.
//!
//! **Shell commands.** A Claude `Bash` call, and a Codex command whose `parsed_cmd` gives up or
//! that writes, is read by [`crate::shellcmd`]: one step per file it read, searched, edited,
//! wrote or deleted, plus a `Run` step when part of it was not understood. A part its exit
//! status does not vouch for settles `unknown` (drawn, never counted).
//!
//! **Only outcomes count.** A Claude call is a PENDING step until its result arrives; only an
//! ok result adds to the file's counters, a failed one marks the step failed. Codex records
//! are already complete. Patch INPUT text is never read as an edit.
//!
//! **Identity.** File and subagent ids are minted in discovery order and never reused or
//! evicted; once a graph holds [`MAX_FILES`] / [`MAX_SUBS`] new ones are not admitted (the
//! step is kept without them and counted). Steps carry a rising `seq`; past [`MAX_STEPS`] the
//! OLDEST are dropped. `gen` is minted whenever a graph starts from nothing, so a reader of an
//! earlier graph of the same conversation can tell it is looking at another one.
//!
//! **Reading.** Each source is read incrementally from its committed offset (the end of the
//! last complete line consumed) with a per-call byte budget; a trailing partial line waits in
//! memory for the rest, and a line longer than [`MAX_LINE`] is skipped rather than stalling
//! the reader. A source whose file was replaced (other inode) or shrank rebuilds the graph.
//!
//! **On disk.** `<state dir>/graphs/<tool>-<conversation>.json`, the whole graph together with
//! each source's committed offset and a hash of the 4 KiB before it, written atomically. A
//! saved graph is resumed only when EVERY source still matches; otherwise it is rebuilt.

use copad_mux_proto::graph::{
    AgentGraph, GraphCounts, GraphFile, GraphPage, GraphStep, GraphSub, Outcome, Settled, StepKind,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

/// Files (and search directories) one conversation admits.
pub const MAX_FILES: usize = 2000;
/// Subagents one conversation admits.
pub const MAX_SUBS: usize = 64;
/// Steps kept; older ones are dropped and counted.
pub const MAX_STEPS: usize = 4000;
/// Calls waiting for their result. Past this the oldest is settled `unknown`.
const MAX_PENDING: usize = 256;
/// Results (and subagent ends) that arrived before what they settle, kept to be matched.
const MAX_UNMATCHED: usize = 128;
/// Settlements the overview carries.
const SETTLED_KEPT: usize = 64;
/// A transcript line longer than this is skipped.
pub const MAX_LINE: usize = 8 << 20;
/// Longest label (char-boundary cut).
const MAX_LABEL: usize = 80;
/// Longest path kept; a longer one is cut (and still a distinct identity by its prefix).
const MAX_PATH: usize = 1024;
/// How much of the file before a committed offset its hash covers.
const HASH_SPAN: u64 = 4096;
/// The on-disk format.
const FORMAT: u32 = 2;
/// File effects one shell command contributes; the rest are counted as omitted.
const MAX_SHELL_EFFECTS: usize = 16;

/// One transcript a graph reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Source {
    pub path: PathBuf,
    /// Whose transcript it is: 0 = the main agent, else a subagent id.
    pub agent: u32,
    pub dev: u64,
    pub ino: u64,
    /// The end of the last complete line consumed.
    pub offset: u64,
    /// FNV-1a of the [`HASH_SPAN`] bytes before `offset`, recorded at save time.
    #[serde(default)]
    pub hash: u64,
    /// Bytes read past `offset` that do not end in a newline yet.
    #[serde(skip)]
    carry: Vec<u8>,
    /// Discarding an oversized line up to its newline.
    #[serde(skip)]
    skipping: bool,
}

/// What reading a source found.
#[derive(Debug, PartialEq)]
pub enum Read_ {
    /// Complete lines (possibly none), and how many bytes were read to get them.
    Lines(Vec<String>, usize),
    /// The file is gone, shrank or was replaced: the graph must start over.
    Rebuild,
}

impl Source {
    pub fn new(path: PathBuf, agent: u32) -> Self {
        Source {
            path,
            agent,
            ..Default::default()
        }
    }

    /// Read up to `budget` new bytes and return the complete lines among them.
    pub fn read(&mut self, budget: usize) -> std::io::Result<Read_> {
        let mut f = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.offset == 0 => {
                return Ok(Read_::Lines(Vec::new(), 0));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Read_::Rebuild),
            Err(e) => return Err(e),
        };
        let meta = f.metadata()?;
        let (dev, ino) = ident(&meta);
        if self.offset == 0 && self.carry.is_empty() && !self.skipping {
            (self.dev, self.ino) = (dev, ino);
        } else if (dev, ino) != (self.dev, self.ino) {
            return Ok(Read_::Rebuild);
        }
        let pos = self.offset + self.carry.len() as u64;
        if meta.len() < pos {
            return Ok(Read_::Rebuild);
        }
        if meta.len() == pos {
            return Ok(Read_::Lines(Vec::new(), 0));
        }
        f.seek(SeekFrom::Start(pos))?;
        let mut buf = Vec::with_capacity(budget.min((meta.len() - pos) as usize));
        f.take(budget as u64).read_to_end(&mut buf)?;
        let read = buf.len();
        Ok(Read_::Lines(self.feed(&buf), read))
    }

    /// Fold freshly read bytes in; returns the lines they completed.
    fn feed(&mut self, mut bytes: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        if self.skipping {
            match bytes.iter().position(|b| *b == b'\n') {
                Some(i) => {
                    self.offset += i as u64 + 1;
                    bytes = &bytes[i + 1..];
                    self.skipping = false;
                }
                None => {
                    self.offset += bytes.len() as u64;
                    return lines;
                }
            }
        }
        let mut start = 0;
        for (i, b) in bytes.iter().enumerate() {
            if *b != b'\n' {
                continue;
            }
            let line = if self.carry.is_empty() {
                &bytes[start..i]
            } else {
                self.carry.extend_from_slice(&bytes[start..i]);
                &self.carry[..]
            };
            let n = line.len() as u64 + 1;
            // Over the cap even though it ended within this read: skipped like the rest.
            if !line.is_empty() && line.len() <= MAX_LINE {
                lines.push(String::from_utf8_lossy(line).into_owned());
            }
            self.offset += n;
            self.carry.clear();
            start = i + 1;
        }
        let rest = &bytes[start..];
        if self.carry.len() + rest.len() > MAX_LINE {
            // An oversized line: drop what we hold of it and skip to its end.
            self.offset += (self.carry.len() + rest.len()) as u64;
            self.carry.clear();
            self.skipping = true;
        } else {
            self.carry.extend_from_slice(rest);
        }
        lines
    }

    /// Record the hash of the bytes before `offset`, for resuming.
    fn stamp(&mut self) {
        self.hash = span_hash(&self.path, self.offset).unwrap_or(0);
    }

    /// Whether a saved source still describes its file.
    fn still_valid(&self) -> bool {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return self.offset == 0;
        };
        ident(&meta) == (self.dev, self.ino)
            && meta.len() >= self.offset
            && span_hash(&self.path, self.offset) == Some(self.hash)
    }
}

#[cfg(unix)]
fn ident(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(not(unix))]
fn ident(_meta: &std::fs::Metadata) -> (u64, u64) {
    (0, 0)
}

fn span_hash(path: &Path, offset: u64) -> Option<u64> {
    let start = offset.saturating_sub(HASH_SPAN);
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((offset - start) as usize);
    f.take(offset - start).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 == offset - start).then(|| fnv(&buf))
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A call waiting for its result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PendingCall {
    /// Every step the call made (a shell command can make several), with what settling it
    /// needs kept here rather than read back from the step, which may be gone by then.
    effects: Vec<PendingEffect>,
    spawned: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct PendingEffect {
    seq: u64,
    kind: StepKind,
    file: Option<u32>,
    /// The step's outcome when the call succeeds: `Unknown` for a part of a shell command its
    /// exit status does not vouch for ([`crate::shellcmd`]).
    on_ok: Outcome,
}

/// A result that arrived before what it settles.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Unmatched {
    id: String,
    outcome: Outcome,
    at_ms: i64,
    /// A background launch (the subagent goes on running).
    launched: bool,
    /// A subagent's end (`<task-notification>`), not a call's result.
    ended: Option<String>,
}

/// One conversation's graph and the state of reading it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConvGraph {
    format: u32,
    pub tool: String,
    pub conversation: String,
    #[serde(rename = "gen")]
    pub generation: String,
    /// The agent's working directory, for relative paths and `display`.
    pub cwd: String,
    files: Vec<GraphFile>,
    /// Index `i` is subagent id `i + 1`.
    subs: Vec<GraphSub>,
    /// Spawning call id → subagent id.
    sub_keys: HashMap<String, u32>,
    steps: VecDeque<GraphStep>,
    next_seq: u64,
    dropped_before: u64,
    pending: HashMap<String, PendingCall>,
    pending_order: VecDeque<String>,
    unmatched: VecDeque<Unmatched>,
    settled: VecDeque<Settled>,
    counts: GraphCounts,
    omitted_files: u32,
    omitted_subs: u32,
    started_ms: i64,
    updated_ms: i64,
    pub sources: Vec<Source>,
    /// The cwd of the newest Codex turn (its paths are relative to it).
    #[serde(default)]
    turn_cwd: String,
    /// Codex `apply_patch` calls, by call id: only THEIR output is read as edits.
    #[serde(default)]
    patch_calls: VecDeque<String>,
    #[serde(skip)]
    file_index: HashMap<String, u32>,
    /// Bumped on every change; what the poller compares to decide to publish or save.
    #[serde(skip)]
    pub rev: u64,
    #[serde(skip)]
    home: String,
}

/// Mint a generation: unique per process and across restarts.
fn mint_gen() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    format!(
        "{ms}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

impl ConvGraph {
    pub fn new(tool: &str, conversation: &str, cwd: &str, transcript: PathBuf) -> Self {
        ConvGraph {
            format: FORMAT,
            tool: tool.to_string(),
            conversation: conversation.to_string(),
            generation: mint_gen(),
            cwd: cwd.to_string(),
            files: Vec::new(),
            subs: Vec::new(),
            sub_keys: HashMap::new(),
            steps: VecDeque::new(),
            next_seq: 1,
            dropped_before: 0,
            pending: HashMap::new(),
            pending_order: VecDeque::new(),
            unmatched: VecDeque::new(),
            settled: VecDeque::new(),
            counts: GraphCounts::default(),
            omitted_files: 0,
            omitted_subs: 0,
            started_ms: 0,
            updated_ms: 0,
            sources: vec![Source::new(transcript, 0)],
            turn_cwd: String::new(),
            patch_calls: VecDeque::new(),
            file_index: HashMap::new(),
            rev: 1,
            home: home_dir(),
        }
    }

    /// The transcript the graph was built from.
    pub fn main_path(&self) -> &Path {
        &self.sources[0].path
    }

    /// Fold one transcript line in, from the source of `agent`.
    pub fn ingest(&mut self, line: &str, agent: u32) {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let before = self.rev;
        match self.tool.as_str() {
            "codex" => self.ingest_codex(&v),
            _ => self.ingest_claude(&v, agent),
        }
        if self.rev != before {
            self.counts.files = self.files.len() as u32;
            self.counts.subagents = self.subs.len() as u32;
        }
    }

    // ---- Claude ---------------------------------------------------------------------------

    fn ingest_claude(&mut self, v: &Value, agent: u32) {
        // The main file can carry a subagent's records too; its own file is where they count.
        if agent == 0 && v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts_ms);
        let at = at.unwrap_or(self.updated_ms);
        if agent == 0
            && self.cwd.is_empty()
            && let Some(c) = v.get("cwd").and_then(Value::as_str)
        {
            self.cwd = c.to_string();
        }
        let content = v.get("message").and_then(|m| m.get("content"));
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                for b in content.and_then(Value::as_array).into_iter().flatten() {
                    if b.get("type").and_then(Value::as_str) != Some("tool_use") {
                        continue;
                    }
                    let id = b.get("id").and_then(Value::as_str).unwrap_or_default();
                    let name = b.get("name").and_then(Value::as_str).unwrap_or_default();
                    let input = b.get("input").unwrap_or(&Value::Null);
                    if name == "Bash" {
                        let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or_default();
                        self.claude_bash(id, input, cwd, at, agent);
                    } else {
                        self.claude_call(id, name, input, at, agent);
                    }
                }
            }
            Some("user") => {
                if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    return;
                }
                let launched = v
                    .get("toolUseResult")
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_str)
                    == Some("async_launched");
                match content {
                    Some(Value::String(s)) => self.claude_text(s, at, agent),
                    Some(Value::Array(blocks)) => {
                        for b in blocks {
                            match b.get("type").and_then(Value::as_str) {
                                Some("tool_result") => {
                                    let id = b
                                        .get("tool_use_id")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default();
                                    let failed =
                                        b.get("is_error").and_then(Value::as_bool) == Some(true);
                                    let outcome =
                                        if failed { Outcome::Failed } else { Outcome::Ok };
                                    self.settle(id, outcome, at, launched);
                                }
                                Some("text") => {
                                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                                        self.claude_text(t, at, agent);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn claude_call(&mut self, id: &str, name: &str, input: &Value, at: i64, agent: u32) {
        let s = |k: &str| input.get(k).and_then(Value::as_str);
        let (kind, path, label) = match name {
            "Read" => (StepKind::Read, s("file_path"), None),
            "Edit" | "MultiEdit" => (StepKind::Edit, s("file_path"), None),
            "NotebookEdit" => (StepKind::Edit, s("notebook_path"), None),
            "Write" => (StepKind::Write, s("file_path"), None),
            "Grep" | "Glob" => (StepKind::Search, s("path"), s("pattern")),
            "WebFetch" => (StepKind::Web, None, s("url")),
            "WebSearch" => (StepKind::Web, None, s("query")),
            "Agent" | "Task" => (StepKind::Spawn, None, s("description")),
            _ => return,
        };
        let file = path.and_then(|p| self.file_for(p, at));
        let spawned = (kind == StepKind::Spawn).then(|| {
            let sub_kind = s("subagent_type").unwrap_or("general-purpose");
            self.sub_for(id, sub_kind, label.unwrap_or_default(), agent, at)
        });
        let seq = self.push_step(GraphStep {
            seq: 0,
            at_ms: at,
            kind,
            agent,
            file,
            label: cut(label.unwrap_or_default()),
            outcome: Outcome::Pending,
            spawned: spawned.flatten(),
        });
        if kind == StepKind::Spawn && spawned.flatten().is_none() {
            self.omitted_subs += 1;
        }
        if path.is_some() && file.is_none() {
            self.omitted_files += 1;
        }
        let call = PendingCall {
            effects: vec![PendingEffect {
                seq,
                kind,
                file,
                on_ok: Outcome::Ok,
            }],
            spawned: spawned.flatten(),
        };
        self.track(id, call);
    }

    /// A `Bash` call: a step per file effect its command has ([`crate::shellcmd`]), plus a
    /// `Run` step when part of it is not understood (or it touches no file).
    fn claude_bash(&mut self, id: &str, input: &Value, cwd: &str, at: i64, agent: u32) {
        let s = |k: &str| input.get(k).and_then(Value::as_str);
        let command = s("command").unwrap_or_default();
        let background = input.get("run_in_background").and_then(Value::as_bool) == Some(true);
        let cwd = if cwd.is_empty() {
            self.cwd.clone()
        } else {
            cwd.to_string()
        };
        let found = crate::shellcmd::analyze(command, &cwd, &self.home, background);
        let mut effects = Vec::new();
        if found.opaque || found.effects.is_empty() {
            let label = cut(s("description").unwrap_or(command));
            let seq = self.push_step(GraphStep {
                at_ms: at,
                kind: StepKind::Run,
                agent,
                label,
                outcome: Outcome::Pending,
                ..Default::default()
            });
            effects.push(PendingEffect {
                seq,
                kind: StepKind::Run,
                file: None,
                on_ok: Outcome::Ok,
            });
        }
        for (i, e) in found.effects.iter().enumerate() {
            if i >= MAX_SHELL_EFFECTS {
                self.omitted_files += 1;
                continue;
            }
            let file = self.file_for(&e.path, at);
            if file.is_none() {
                self.omitted_files += 1;
            }
            let seq = self.push_step(GraphStep {
                at_ms: at,
                kind: e.kind,
                agent,
                file,
                label: cut(e.label.as_deref().unwrap_or_default()),
                outcome: Outcome::Pending,
                ..Default::default()
            });
            effects.push(PendingEffect {
                seq,
                kind: e.kind,
                file,
                on_ok: e.on_ok,
            });
        }
        self.track(
            id,
            PendingCall {
                effects,
                spawned: None,
            },
        );
    }

    /// Wait for the result of call `id`.
    fn track(&mut self, id: &str, call: PendingCall) {
        if id.is_empty() {
            return;
        }
        self.pending.insert(id.to_string(), call);
        self.pending_order.push_back(id.to_string());
        // Settled ids stay in the queue until they reach its front; drop them in bulk so a
        // long conversation of one-at-a-time calls does not grow it without bound.
        if self.pending_order.len() > 2 * MAX_PENDING {
            let pending = &self.pending;
            self.pending_order.retain(|id| pending.contains_key(id));
        }
        if let Some(i) = self
            .unmatched
            .iter()
            .position(|u| u.id == id && u.ended.is_none())
        {
            let u = self.unmatched.remove(i).expect("indexed");
            self.settle(&u.id, u.outcome, u.at_ms, u.launched);
        }
        while self.pending.len() > MAX_PENDING {
            let Some(old) = self.pending_order.pop_front() else {
                break;
            };
            if let Some(p) = self.pending.remove(&old) {
                for e in &p.effects {
                    self.set_outcome(e.seq, Outcome::Unknown);
                }
            }
        }
    }

    /// A user record's text: a prompt, a slash command, a subagent's end, or furniture.
    fn claude_text(&mut self, text: &str, at: i64, agent: u32) {
        let t = text.trim_start();
        if let Some(rest) = t.strip_prefix("<task-notification>") {
            let id = between(rest, "<tool-use-id>", "</tool-use-id>");
            let status = between(rest, "<status>", "</status>").unwrap_or("completed");
            if let Some(id) = id {
                self.sub_ended(id, status, at);
            }
            return;
        }
        if agent != 0 {
            // A subagent's first message is its task, already the spawn's label.
            return;
        }
        let label = if let Some(name) = between(t, "<command-name>", "</command-name>") {
            let args = between(t, "<command-args>", "</command-args>").unwrap_or_default();
            format!("{name} {args}").trim().to_string()
        } else if t.starts_with('<')
            || t.starts_with("[Request interrupted")
            || t.starts_with("Caveat:")
        {
            return;
        } else {
            t.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        if label.is_empty() {
            return;
        }
        self.counts.prompts += 1;
        self.push_step(GraphStep {
            seq: 0,
            at_ms: at,
            kind: StepKind::Prompt,
            agent: 0,
            file: None,
            label: cut(&label),
            outcome: Outcome::Ok,
            spawned: None,
        });
    }

    /// A subagent's transcript was found (its `.meta.json`): create the subagent if its spawn
    /// has not been read yet, and start reading its file.
    pub fn found_subagent(
        &mut self,
        call_id: &str,
        kind: &str,
        description: &str,
        transcript: PathBuf,
        at: i64,
    ) {
        if self.sources.iter().any(|s| s.path == transcript) {
            return;
        }
        let Some(id) = self.sub_for(call_id, kind, description, 0, at) else {
            return;
        };
        self.sources.push(Source::new(transcript, id));
        self.rev += 1;
    }

    /// The subagent spawned by `call_id`, creating it (admission permitting).
    fn sub_for(
        &mut self,
        call_id: &str,
        kind: &str,
        description: &str,
        parent: u32,
        at: i64,
    ) -> Option<u32> {
        if let Some(id) = self.sub_keys.get(call_id).copied() {
            let sub = &mut self.subs[id as usize - 1];
            // Found by its file first: the spawn now says who started it and when.
            if parent != 0 && sub.parent == 0 && parent != id {
                sub.parent = parent;
            }
            if sub.started_ms == 0 || (at != 0 && at < sub.started_ms) {
                sub.started_ms = at;
            }
            return Some(id);
        }
        if self.subs.len() >= MAX_SUBS {
            return None;
        }
        let id = self.subs.len() as u32 + 1;
        self.subs.push(GraphSub {
            id,
            kind: cut(kind),
            description: cut(description),
            parent,
            started_ms: at,
            ended_ms: None,
            status: "running".into(),
        });
        if !call_id.is_empty() {
            self.sub_keys.insert(call_id.to_string(), id);
        }
        self.rev += 1;
        if let Some(i) = self
            .unmatched
            .iter()
            .position(|u| u.id == call_id && u.ended.is_some())
        {
            let u = self.unmatched.remove(i).expect("indexed");
            self.sub_ended(&u.id, u.ended.as_deref().unwrap_or("completed"), u.at_ms);
        }
        Some(id)
    }

    fn sub_ended(&mut self, call_id: &str, status: &str, at: i64) {
        let Some(id) = self.sub_keys.get(call_id).copied() else {
            self.remember(Unmatched {
                id: call_id.to_string(),
                outcome: Outcome::Ok,
                at_ms: at,
                launched: false,
                ended: Some(status.to_string()),
            });
            return;
        };
        let sub = &mut self.subs[id as usize - 1];
        sub.ended_ms = Some(at);
        sub.status = match status {
            "completed" => "completed",
            "failed" => "failed",
            _ => "killed",
        }
        .into();
        self.updated_ms = self.updated_ms.max(at);
        self.rev += 1;
    }

    fn remember(&mut self, u: Unmatched) {
        self.unmatched.push_back(u);
        while self.unmatched.len() > MAX_UNMATCHED {
            self.unmatched.pop_front();
        }
    }

    /// A call's result: settle its step and apply what it did.
    fn settle(&mut self, call_id: &str, outcome: Outcome, at: i64, launched: bool) {
        let Some(p) = self.pending.remove(call_id) else {
            if !call_id.is_empty() {
                self.remember(Unmatched {
                    id: call_id.to_string(),
                    outcome,
                    at_ms: at,
                    launched,
                    ended: None,
                });
            }
            return;
        };
        for e in &p.effects {
            let outcome = if outcome == Outcome::Ok {
                e.on_ok
            } else {
                outcome
            };
            self.set_outcome(e.seq, outcome);
            if outcome == Outcome::Ok {
                self.apply(e.kind, e.file, at);
            }
        }
        if let Some(sub) = p.spawned
            && !launched
        {
            // A spawn that waited for its subagent: the result is its end.
            let status = if outcome == Outcome::Ok {
                "completed"
            } else {
                "failed"
            };
            if let Some(key) = self
                .sub_keys
                .iter()
                .find(|(_, v)| **v == sub)
                .map(|(k, _)| k.clone())
            {
                self.sub_ended(&key, status, at);
            }
        }
    }

    fn set_outcome(&mut self, seq: u64, outcome: Outcome) {
        if outcome == Outcome::Failed {
            self.counts.failed += 1;
        }
        if let Ok(i) = self.steps.binary_search_by_key(&seq, |s| s.seq) {
            self.steps[i].outcome = outcome;
        }
        self.settled.push_back(Settled { seq, outcome });
        while self.settled.len() > SETTLED_KEPT {
            self.settled.pop_front();
        }
        self.rev += 1;
    }

    /// Add what an ok step did to the counters.
    fn apply(&mut self, kind: StepKind, file: Option<u32>, at: i64) {
        let c = &mut self.counts;
        match kind {
            StepKind::Read => c.reads += 1,
            StepKind::Edit | StepKind::Delete => c.edits += 1,
            StepKind::Write => c.writes += 1,
            StepKind::Search => c.searches += 1,
            StepKind::Run => c.runs += 1,
            _ => {}
        }
        if let Some(f) = file.and_then(|i| self.files.get_mut(i as usize)) {
            match kind {
                StepKind::Read => f.reads += 1,
                StepKind::Edit | StepKind::Delete => f.edits += 1,
                StepKind::Write => f.writes += 1,
                StepKind::Search => f.searches += 1,
                _ => {}
            }
            f.last_ms = f.last_ms.max(at);
        }
        self.rev += 1;
    }

    // ---- Codex ----------------------------------------------------------------------------

    fn ingest_codex(&mut self, v: &Value) {
        let at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts_ms);
        let at = at.unwrap_or(self.updated_ms);
        let Some(p) = v.get("payload") else {
            return;
        };
        let ptype = p.get("type").and_then(Value::as_str);
        match (v.get("type").and_then(Value::as_str), ptype) {
            (Some("turn_context"), _) => {
                if let Some(c) = p.get("cwd").and_then(Value::as_str) {
                    self.turn_cwd = c.to_string();
                    if self.cwd.is_empty() {
                        self.cwd = c.to_string();
                    }
                }
            }
            (Some("session_meta"), _) => {
                if self.cwd.is_empty()
                    && let Some(c) = p.get("cwd").and_then(Value::as_str)
                {
                    self.cwd = c.to_string();
                }
            }
            (Some("event_msg"), Some("item_completed")) => {
                let Some(item) = p.get("item") else {
                    return;
                };
                match item.get("type").and_then(Value::as_str) {
                    Some("CommandExecution") => self.codex_command(item, at),
                    Some("UserMessage") => {
                        let text = item
                            .get("content")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .find_map(|c| c.get("text").and_then(Value::as_str));
                        if let Some(line) =
                            text.and_then(|t| t.lines().find(|l| !l.trim().is_empty()))
                        {
                            self.counts.prompts += 1;
                            self.push_step(GraphStep {
                                at_ms: at,
                                kind: StepKind::Prompt,
                                label: cut(line.trim()),
                                ..Default::default()
                            });
                        }
                    }
                    _ => {}
                }
            }
            (Some("event_msg"), Some("task_complete")) => {
                self.push_step(GraphStep {
                    at_ms: at,
                    kind: StepKind::Done,
                    ..Default::default()
                });
            }
            (Some("response_item"), Some("custom_tool_call" | "function_call")) => {
                // The patch TOOL itself, by name. A shell or script call whose text merely
                // contains a patch (or prints one) is not evidence that anything was applied.
                let patch_tool = p.get("name").and_then(Value::as_str) == Some("apply_patch");
                if patch_tool && let Some(id) = p.get("call_id").and_then(Value::as_str) {
                    self.patch_calls.push_back(id.to_string());
                    while self.patch_calls.len() > MAX_PENDING {
                        self.patch_calls.pop_front();
                    }
                }
            }
            (Some("response_item"), Some("custom_tool_call_output" | "function_call_output")) => {
                // Only a patch call's output: any other tool's output may merely QUOTE the
                // success line (a log, a doc, this very file).
                let call = p.get("call_id").and_then(Value::as_str).unwrap_or_default();
                let Some(i) = self.patch_calls.iter().position(|c| c == call) else {
                    return;
                };
                self.patch_calls.remove(i);
                let text = match p.get("output") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Array(parts)) => parts
                        .iter()
                        .filter_map(|x| x.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => return,
                };
                self.codex_patch_result(&text, at);
            }
            _ => {}
        }
    }

    fn codex_command(&mut self, item: &Value, at: i64) {
        let failed = item
            .get("exit_code")
            .and_then(Value::as_i64)
            .is_some_and(|c| c != 0)
            || item.get("status").and_then(Value::as_str) == Some("failed");
        let outcome = if failed { Outcome::Failed } else { Outcome::Ok };
        let base = item
            .get("cwd")
            .and_then(Value::as_str)
            .map(|c| c.strip_prefix("file://").unwrap_or(c).to_string())
            .unwrap_or_else(|| self.turn_cwd.clone());
        let parsed = item
            .get("parsed_cmd")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // `parsed_cmd` knows reads and searches only; where it gives up, or the command also
        // writes, the command's own analysis is the one source for it.
        let script = item
            .get("command")
            .and_then(Value::as_array)
            .filter(|a| a.len() == 3 && matches!(a[1].as_str(), Some("-lc" | "-c")))
            .and_then(|a| a[2].as_str());
        if let Some(script) = script {
            let found = crate::shellcmd::analyze(script, &base, &self.home, false);
            let gave_up = parsed.is_empty()
                || parsed
                    .iter()
                    .any(|c| c.get("type").and_then(Value::as_str) == Some("unknown"));
            let writes = found
                .effects
                .iter()
                .any(|e| matches!(e.kind, StepKind::Edit | StepKind::Write | StepKind::Delete));
            if gave_up || writes {
                if found.opaque || found.effects.is_empty() {
                    self.push_settled(StepKind::Run, None, script, outcome, at);
                }
                for (i, e) in found.effects.iter().enumerate() {
                    if i >= MAX_SHELL_EFFECTS {
                        self.omitted_files += 1;
                        continue;
                    }
                    let file = self.file_for(&e.path, at);
                    if file.is_none() {
                        self.omitted_files += 1;
                    }
                    let outcome = if outcome == Outcome::Ok {
                        e.on_ok
                    } else {
                        outcome
                    };
                    let label = e.label.as_deref().unwrap_or_default();
                    self.push_settled(e.kind, file, label, outcome, at);
                }
                return;
            }
        }
        let mut any = false;
        for c in &parsed {
            let s = |k: &str| c.get(k).and_then(Value::as_str);
            let kind = match s("type") {
                Some("read") => StepKind::Read,
                Some("search" | "list_files") => StepKind::Search,
                _ => continue,
            };
            any = true;
            let path = s("path").map(|p| join(&base, p));
            let file = path.as_deref().and_then(|p| self.file_for(p, at));
            if path.is_some() && file.is_none() {
                self.omitted_files += 1;
            }
            let label = if kind == StepKind::Search {
                s("query").or(s("cmd"))
            } else {
                None
            };
            self.push_settled(kind, file, label.unwrap_or_default(), outcome, at);
        }
        if !any {
            let cmd = parsed
                .first()
                .and_then(|c| c.get("cmd").and_then(Value::as_str))
                .map(str::to_string)
                .or_else(|| {
                    item.get("command")
                        .and_then(Value::as_array)
                        .and_then(|a| a.last())
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            self.push_settled(StepKind::Run, None, &cmd, outcome, at);
        }
    }

    /// `Success. Updated the following files:` then `A|M|D <path>` lines.
    fn codex_patch_result(&mut self, text: &str, at: i64) {
        let Some(i) = text.find("Success. Updated the following files:") else {
            return;
        };
        let base = self.turn_cwd.clone();
        for line in text[i..].lines().skip(1) {
            let (kind, rest) = match line.split_once(' ') {
                Some(("A", r)) => (StepKind::Write, r),
                Some(("M", r)) => (StepKind::Edit, r),
                Some(("D", r)) => (StepKind::Delete, r),
                _ => break,
            };
            let abs = join(&base, rest.trim());
            let file = self.file_for(&abs, at);
            if file.is_none() {
                self.omitted_files += 1;
            }
            self.push_settled(kind, file, "", Outcome::Ok, at);
        }
    }

    fn push_settled(
        &mut self,
        kind: StepKind,
        file: Option<u32>,
        label: &str,
        outcome: Outcome,
        at: i64,
    ) {
        self.push_step(GraphStep {
            at_ms: at,
            kind,
            file,
            label: cut(label),
            outcome,
            ..Default::default()
        });
        if outcome == Outcome::Ok {
            self.apply(kind, file, at);
        } else if outcome == Outcome::Failed {
            self.counts.failed += 1;
        }
    }

    // ---- shared ---------------------------------------------------------------------------

    fn push_step(&mut self, mut step: GraphStep) -> u64 {
        step.seq = self.next_seq;
        self.next_seq += 1;
        self.counts.steps = step.seq;
        if self.started_ms == 0 || (step.at_ms != 0 && step.at_ms < self.started_ms) {
            self.started_ms = step.at_ms;
        }
        self.updated_ms = self.updated_ms.max(step.at_ms);
        let seq = step.seq;
        self.steps.push_back(step);
        while self.steps.len() > MAX_STEPS {
            self.steps.pop_front();
            self.dropped_before += 1;
        }
        self.rev += 1;
        seq
    }

    /// The file at `path` (absolute, or relative to the cwd), admitting it if there is room.
    fn file_for(&mut self, path: &str, at: i64) -> Option<u32> {
        let abs = normalize(&self.cwd, path);
        if let Some(id) = self.file_index.get(&abs) {
            return Some(*id);
        }
        if self.files.len() >= MAX_FILES {
            return None;
        }
        let id = self.files.len() as u32;
        let display = display(&abs, &self.cwd, &self.home);
        self.files.push(GraphFile {
            id,
            abs: abs.clone(),
            display,
            first_ms: at,
            last_ms: at,
            ..Default::default()
        });
        self.file_index.insert(abs, id);
        self.rev += 1;
        Some(id)
    }

    // ---- slices ---------------------------------------------------------------------------

    /// The overview with at most `files` files, `steps` steps and `subs` subagents (plus every
    /// record the chosen steps reference).
    pub fn overview(&self, files: usize, steps: usize, subs: usize) -> AgentGraph {
        let chosen: Vec<GraphStep> = self.steps.iter().rev().take(steps).rev().cloned().collect();
        let (files, subs) = self.referenced(&chosen, files, subs);
        let mut pending: Vec<u64> = self
            .pending
            .values()
            .flat_map(|p| p.effects.iter().map(|e| e.seq))
            .collect();
        pending.sort_unstable();
        let pending = pending.split_off(pending.len().saturating_sub(32));
        AgentGraph {
            tool: self.tool.clone(),
            conversation: self.conversation.clone(),
            generation: self.generation.clone(),
            cwd: self.cwd.clone(),
            started_ms: self.started_ms,
            updated_ms: self.updated_ms,
            counts: self.counts,
            files,
            subagents: subs,
            steps: chosen,
            pending,
            settled: self.settled.iter().copied().collect(),
            oldest_seq: self.oldest_seq(),
            dropped_before: self.dropped_before,
            omitted_files: self.omitted_files,
            omitted_subagents: self.omitted_subs,
            ..Default::default()
        }
    }

    fn oldest_seq(&self) -> u64 {
        self.steps.front().map_or(0, |s| s.seq)
    }

    /// The newest `files` files and `subs` subagents, plus those `steps` reference.
    fn referenced(
        &self,
        steps: &[GraphStep],
        files: usize,
        subs: usize,
    ) -> (Vec<GraphFile>, Vec<GraphSub>) {
        // The steps' own files first, then the most recent others up to `files` in all.
        let mut fids: Vec<u32> = steps.iter().filter_map(|s| s.file).collect();
        fids.sort_unstable();
        fids.dedup();
        if fids.len() < files {
            let mut by_recent: Vec<&GraphFile> = self.files.iter().collect();
            by_recent.sort_by_key(|f| (std::cmp::Reverse(f.last_ms), std::cmp::Reverse(f.id)));
            let extra: Vec<u32> = by_recent
                .iter()
                .map(|f| f.id)
                .filter(|id| fids.binary_search(id).is_err())
                .take(files - fids.len())
                .collect();
            fids.extend(extra);
            fids.sort_unstable();
        }
        let mut sids: Vec<u32> = steps
            .iter()
            .flat_map(|s| [Some(s.agent), s.spawned])
            .flatten()
            .filter(|id| *id != 0)
            .collect();
        sids.extend(self.subs.iter().rev().take(subs).map(|s| s.id));
        // A subagent's parent chain, so its edge has somewhere to go.
        let mut i = 0;
        while i < sids.len() {
            let parent = self.subs.get(sids[i] as usize - 1).map_or(0, |s| s.parent);
            if parent != 0 && !sids.contains(&parent) {
                sids.push(parent);
            }
            i += 1;
        }
        sids.sort_unstable();
        sids.dedup();
        let files = fids
            .iter()
            .filter_map(|i| self.files.get(*i as usize))
            .cloned()
            .collect();
        let subs = sids
            .iter()
            .filter_map(|i| self.subs.get(*i as usize - 1))
            .cloned()
            .collect();
        (files, subs)
    }

    /// A page of history: up to `limit` steps before `before` (all when `None`), within
    /// `max_bytes` serialised (the oldest steps trimmed first).
    pub fn page(&self, before: Option<u64>, limit: usize, max_bytes: usize) -> GraphPage {
        let mut steps: Vec<GraphStep> = self
            .steps
            .iter()
            .filter(|s| before.is_none_or(|b| s.seq < b))
            .cloned()
            .collect();
        steps = steps.split_off(steps.len().saturating_sub(limit.max(1)));
        loop {
            let (files, subagents) = self.referenced(&steps, 0, 0);
            let page = GraphPage {
                tool: self.tool.clone(),
                conversation: self.conversation.clone(),
                generation: self.generation.clone(),
                cwd: self.cwd.clone(),
                counts: self.counts,
                files,
                subagents,
                oldest_seq: self.oldest_seq(),
                page_first_seq: steps.first().map_or(0, |s| s.seq),
                page_last_seq: steps.last().map_or(0, |s| s.seq),
                next_before: steps
                    .first()
                    .filter(|s| s.seq > self.oldest_seq())
                    .map(|s| s.seq),
                dropped_before: self.dropped_before,
                steps: steps.clone(),
                ..Default::default()
            };
            let size = serde_json::to_vec(&page).map_or(0, |v| v.len());
            // One step and its two records are bounded (labels and paths are cut), so a page
            // of one always fits a sane budget and the loop ends.
            if size <= max_bytes || steps.len() <= 1 {
                return page;
            }
            let cut_n = (steps.len() / 4).max(1);
            steps.drain(..cut_n);
        }
    }

    // ---- disk -----------------------------------------------------------------------------

    /// The graph's file in `dir`.
    pub fn file_name(tool: &str, conversation: &str) -> Option<String> {
        let safe = |s: &str| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        };
        (safe(tool) && safe(conversation)).then(|| format!("{tool}-{conversation}.json"))
    }

    /// Write the graph to `dir` atomically, 0600 in a 0700 directory.
    pub fn save(&mut self, dir: &Path) -> std::io::Result<()> {
        let Some(name) = Self::file_name(&self.tool, &self.conversation) else {
            return Ok(());
        };
        for s in &mut self.sources {
            s.stamp();
        }
        create_private_dir(dir)?;
        let data = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        let tmp = dir.join(format!(".{name}.tmp"));
        {
            use std::io::Write;
            let mut f = open_private(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, dir.join(&name))
    }

    /// The saved graph for this conversation, if it still matches every file it read.
    pub fn load(dir: &Path, tool: &str, conversation: &str, transcript: &Path) -> Option<Self> {
        let name = Self::file_name(tool, conversation)?;
        let data = std::fs::read(dir.join(name)).ok()?;
        let mut g: ConvGraph = serde_json::from_slice(&data).ok()?;
        if g.format != FORMAT
            || g.tool != tool
            || g.conversation != conversation
            || g.sources.first().map(|s| s.path.as_path()) != Some(transcript)
            || !g.sources.iter().all(Source::still_valid)
            || g.subs
                .iter()
                .enumerate()
                .any(|(i, s)| s.id as usize != i + 1)
            || g.files.iter().enumerate().any(|(i, f)| f.id as usize != i)
        {
            return None;
        }
        g.file_index = g.files.iter().map(|f| (f.abs.clone(), f.id)).collect();
        g.home = home_dir();
        g.rev = 1;
        Some(g)
    }
}

/// Delete saved graphs older than `days`, then the oldest beyond `keep`.
pub fn prune(dir: &Path, days: u32, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let max_age = std::time::Duration::from_secs(u64::from(days) * 86_400);
    let mut kept: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Ok(m) = e.metadata() else { continue };
        if !m.is_file() {
            continue;
        }
        let mtime = m.modified().unwrap_or(now);
        if now.duration_since(mtime).unwrap_or_default() > max_age {
            let _ = std::fs::remove_file(&p);
        } else {
            kept.push((mtime, p));
        }
    }
    if kept.len() > keep {
        kept.sort();
        for (_, p) in &kept[..kept.len() - keep] {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

fn open_private(p: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(p)
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_default()
}

/// `text` cut to [`MAX_LABEL`] chars, on a char boundary, one line.
fn cut(text: &str) -> String {
    let one = text.lines().next().unwrap_or_default().trim();
    match one.char_indices().nth(MAX_LABEL) {
        Some((i, _)) => format!("{}…", &one[..i]),
        None => one.to_string(),
    }
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let end = s[start..].find(close)? + start;
    Some(s[start..end].trim())
}

fn join(base: &str, p: &str) -> String {
    if Path::new(p).is_absolute() || base.is_empty() {
        p.to_string()
    } else {
        format!("{base}/{p}")
    }
}

/// `path` made absolute against `cwd` and lexically normalised (`.` and `..` collapsed; never
/// through the filesystem, so a symlink is kept as named). Cut to [`MAX_PATH`] bytes.
pub fn normalize(cwd: &str, path: &str) -> String {
    let joined = if Path::new(path).is_absolute() || cwd.is_empty() {
        PathBuf::from(path)
    } else {
        Path::new(cwd).join(path)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    let s = out.to_string_lossy().into_owned();
    if s.len() <= MAX_PATH {
        return s;
    }
    let mut end = MAX_PATH;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// `abs` relative to `cwd` when under it, else `~/…` under `home`, else as is.
pub fn display(abs: &str, cwd: &str, home: &str) -> String {
    let under = |root: &str| {
        let root = root.trim_end_matches('/');
        (!root.is_empty())
            .then(|| abs.strip_prefix(root))
            .flatten()
            .and_then(|r| r.strip_prefix('/'))
    };
    if let Some(r) = under(cwd) {
        return r.to_string();
    }
    if let Some(r) = under(home) {
        return format!("~/{r}");
    }
    abs.to_string()
}

/// RFC 3339 (`2026-11-03T16:40:43.083Z`, `…+09:00`) → UNIX milliseconds.
pub fn parse_ts_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &s[19..];
    let mut ms = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        let f = &frac[..digits];
        let padded = format!("{:0<3}", &f[..f.len().min(3)]);
        ms = padded.parse().ok()?;
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" | "" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let oh: i64 = o[1..3].parse().ok()?;
            let om: i64 = o[4..6].parse().ok()?;
            let v = (oh * 60 + om) * 60;
            if o.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec) - offset) * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("copad-workgraph-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn claude(cwd: &str) -> ConvGraph {
        let mut g = ConvGraph::new("claude", "c1", cwd, PathBuf::from("/nonexistent/c1.jsonl"));
        g.home = "/home/u".into();
        g
    }

    fn call(id: &str, name: &str, input: &str, ts: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}}}}"#
        )
    }

    fn result(id: &str, err: bool, ts: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"{ts}","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","is_error":{err}}}]}}}}"#
        )
    }

    const T0: &str = "2026-11-04T00:00:00.000Z";
    const T1: &str = "2026-11-04T00:00:01.500Z";

    #[test]
    fn timestamps_parse_to_unix_ms() {
        assert_eq!(parse_ts_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_ts_ms("2026-11-03T16:40:43.083Z"),
            Some(1_793_724_043_083)
        );
        assert_eq!(
            parse_ts_ms("2026-11-04T01:40:43.083+09:00"),
            Some(1_793_724_043_083)
        );
        assert_eq!(
            parse_ts_ms("2026-11-03T16:40:43.5Z"),
            Some(1_793_724_043_500)
        );
        assert_eq!(parse_ts_ms("nope"), None);
    }

    #[test]
    fn paths_normalise_and_display_relative() {
        assert_eq!(normalize("/r", "src/../a.rs"), "/r/a.rs");
        assert_eq!(normalize("/r", "/x/./y"), "/x/y");
        assert_eq!(display("/r/src/a.rs", "/r", "/home/u"), "src/a.rs");
        assert_eq!(display("/home/u/n.md", "/r", "/home/u"), "~/n.md");
        assert_eq!(display("/rx/a", "/r", "/home/u"), "/rx/a");
    }

    #[test]
    fn a_claude_call_counts_only_once_its_result_is_ok() {
        let mut g = claude("/r");
        g.ingest(&call("t1", "Read", r#"{"file_path":"/r/src/a.rs"}"#, T0), 0);
        let o = g.overview(32, 24, 8);
        assert_eq!(o.steps[0].outcome, Outcome::Pending);
        assert_eq!(o.pending, vec![1]);
        assert_eq!(o.files[0].reads, 0);
        assert_eq!(o.files[0].display, "src/a.rs");
        g.ingest(&result("t1", false, T1), 0);
        let o = g.overview(32, 24, 8);
        assert_eq!(o.steps[0].outcome, Outcome::Ok);
        assert_eq!(o.files[0].reads, 1);
        assert_eq!(o.counts.reads, 1);
        assert_eq!(
            o.settled,
            vec![Settled {
                seq: 1,
                outcome: Outcome::Ok
            }]
        );
        assert!(o.pending.is_empty());
    }

    #[test]
    fn a_failed_edit_is_marked_and_not_counted() {
        let mut g = claude("/r");
        g.ingest(&call("t1", "Edit", r#"{"file_path":"/r/a.rs"}"#, T0), 0);
        g.ingest(&result("t1", true, T1), 0);
        let o = g.overview(32, 24, 8);
        assert_eq!(o.steps[0].outcome, Outcome::Failed);
        assert_eq!(o.files[0].edits, 0);
        assert_eq!(o.counts.failed, 1);
    }

    #[test]
    fn each_tool_maps_to_its_kind() {
        let mut g = claude("/r");
        for (i, (name, input)) in [
            ("Write", r#"{"file_path":"/r/n.rs"}"#),
            ("Grep", r#"{"pattern":"fn main","path":"/r/src"}"#),
            (
                "Bash",
                r#"{"command":"cargo test","description":"Run tests"}"#,
            ),
            ("WebSearch", r#"{"query":"rust"}"#),
            ("TodoWrite", r#"{}"#),
        ]
        .iter()
        .enumerate()
        {
            g.ingest(&call(&format!("t{i}"), name, input, T0), 0);
            g.ingest(&result(&format!("t{i}"), false, T1), 0);
        }
        let o = g.overview(32, 24, 8);
        let kinds: Vec<_> = o.steps.iter().map(|s| (s.kind, s.label.as_str())).collect();
        assert_eq!(
            kinds,
            vec![
                (StepKind::Write, ""),
                (StepKind::Search, "fn main"),
                (StepKind::Run, "Run tests"),
                (StepKind::Web, "rust"),
            ]
        );
        assert_eq!(
            (o.counts.writes, o.counts.searches, o.counts.runs),
            (1, 1, 1)
        );
        assert_eq!(o.files[1].display, "src");
    }

    #[test]
    fn prompts_are_kept_and_furniture_is_not() {
        let mut g = claude("/r");
        let user = |text: &str| {
            format!(
                r#"{{"type":"user","timestamp":"{T0}","message":{{"content":{}}}}}"#,
                serde_json::to_string(text).unwrap()
            )
        };
        g.ingest(&user("Fix the build\nplease"), 0);
        g.ingest(&user("<system-reminder>x</system-reminder>"), 0);
        g.ingest(
            &user("<command-name>/goal</command-name>\n<command-args>ship it</command-args>"),
            0,
        );
        g.ingest(&user("[Request interrupted by user]"), 0);
        g.ingest(
            r#"{"type":"user","isMeta":true,"message":{"content":"meta"}}"#,
            0,
        );
        let labels: Vec<_> = g
            .overview(0, 24, 0)
            .steps
            .into_iter()
            .map(|s| s.label)
            .collect();
        assert_eq!(labels, vec!["Fix the build", "/goal ship it"]);
        assert_eq!(g.counts.prompts, 2);
    }

    #[test]
    fn sidechain_records_in_the_main_file_are_skipped() {
        let mut g = claude("/r");
        let line = call("t1", "Read", r#"{"file_path":"/r/a"}"#, T0).replacen(
            "{",
            r#"{"isSidechain":true,"#,
            1,
        );
        g.ingest(&line, 0);
        assert!(g.overview(32, 24, 8).steps.is_empty());
    }

    #[test]
    fn a_subagent_gets_its_steps_and_ends_by_notification_or_result() {
        let mut g = claude("/r");
        g.ingest(
            &call(
                "a1",
                "Agent",
                r#"{"description":"Map it","subagent_type":"Explore"}"#,
                T0,
            ),
            0,
        );
        // Launched in the background: the launch settles, the subagent goes on.
        g.ingest(
            &result("a1", false, T0).replacen(
                "{",
                r#"{"toolUseResult":{"status":"async_launched"},"#,
                1,
            ),
            0,
        );
        g.found_subagent(
            "a1",
            "Explore",
            "Map it",
            PathBuf::from("/x/agent-1.jsonl"),
            0,
        );
        assert_eq!(g.sources.len(), 2);
        g.ingest(&call("s1", "Read", r#"{"file_path":"/r/b.rs"}"#, T1), 1);
        g.ingest(&result("s1", false, T1), 1);
        let o = g.overview(32, 24, 8);
        assert_eq!(o.subagents[0].status, "running");
        assert_eq!(o.subagents[0].kind, "Explore");
        assert_eq!(o.steps[1].agent, 1);
        assert_eq!(o.steps[0].spawned, Some(1));
        let note = "<task-notification>\n<tool-use-id>a1</tool-use-id>\n<status>completed</status>";
        g.ingest(
            &format!(
                r#"{{"type":"user","timestamp":"{T1}","message":{{"content":{}}}}}"#,
                serde_json::to_string(note).unwrap()
            ),
            0,
        );
        let o = g.overview(32, 24, 8);
        assert_eq!(o.subagents[0].status, "completed");
        assert!(o.subagents[0].ended_ms.is_some());
        // A foreground spawn ends with its result.
        g.ingest(&call("a2", "Task", r#"{"description":"Quick"}"#, T0), 0);
        g.ingest(&result("a2", false, T1), 0);
        assert_eq!(g.overview(32, 24, 8).subagents[1].status, "completed");
    }

    #[test]
    fn a_subagent_found_before_its_spawn_is_reconciled() {
        let mut g = claude("/r");
        g.found_subagent(
            "a9",
            "Explore",
            "early",
            PathBuf::from("/x/agent-9.jsonl"),
            0,
        );
        g.ingest(
            &call(
                "a9",
                "Agent",
                r#"{"description":"early","subagent_type":"Explore"}"#,
                T0,
            ),
            0,
        );
        let o = g.overview(32, 24, 8);
        assert_eq!(o.subagents.len(), 1);
        assert_eq!(o.steps[0].spawned, Some(1));
        assert_eq!(o.subagents[0].started_ms, parse_ts_ms(T0).unwrap());
    }

    #[test]
    fn a_nested_spawn_names_its_parent() {
        let mut g = claude("/r");
        g.ingest(&call("a1", "Agent", r#"{"description":"outer"}"#, T0), 0);
        g.ingest(&call("a2", "Agent", r#"{"description":"inner"}"#, T1), 1);
        assert_eq!(g.overview(0, 24, 8).subagents[1].parent, 1);
    }

    #[test]
    fn a_result_before_its_call_is_matched_later() {
        let mut g = claude("/r");
        g.ingest(&result("t1", false, T1), 0);
        g.ingest(&call("t1", "Read", r#"{"file_path":"/r/a"}"#, T0), 0);
        assert_eq!(g.overview(32, 24, 8).steps[0].outcome, Outcome::Ok);
    }

    #[test]
    fn admission_stops_at_the_cap_and_counts_what_it_left_out() {
        let mut g = claude("/r");
        for i in 0..MAX_FILES + 3 {
            g.ingest(
                &call(
                    &format!("t{i}"),
                    "Read",
                    &format!(r#"{{"file_path":"/r/f{i}"}}"#),
                    T0,
                ),
                0,
            );
        }
        assert_eq!(g.files.len(), MAX_FILES);
        assert_eq!(g.omitted_files, 3);
        assert_eq!(g.steps.back().unwrap().file, None);
        // A known path still maps to its id.
        g.ingest(&call("again", "Read", r#"{"file_path":"/r/f0"}"#, T0), 0);
        assert_eq!(g.steps.back().unwrap().file, Some(0));
    }

    #[test]
    fn old_steps_drop_but_references_still_resolve() {
        let mut g = claude("/r");
        for i in 0..MAX_STEPS + 10 {
            g.ingest(
                &call(&format!("t{i}"), "Read", r#"{"file_path":"/r/a"}"#, T0),
                0,
            );
        }
        let o = g.overview(0, 24, 0);
        assert_eq!(o.dropped_before, 10);
        assert_eq!(o.oldest_seq, 11);
        assert_eq!(o.files.len(), 1);
        // Pending overflow settled the oldest calls as unknown.
        assert!(g.pending.len() <= MAX_PENDING);
        assert!(g.settled.iter().all(|s| s.outcome == Outcome::Unknown));
    }

    #[test]
    fn pages_walk_back_to_the_oldest_step() {
        let mut g = claude("/r");
        for i in 0..25 {
            g.ingest(
                &call(
                    &format!("t{i}"),
                    "Read",
                    &format!(r#"{{"file_path":"/r/f{i}"}}"#),
                    T0,
                ),
                0,
            );
        }
        let p = g.page(None, 10, 1 << 20);
        assert_eq!(
            (p.page_first_seq, p.page_last_seq, p.next_before),
            (16, 25, Some(16))
        );
        assert_eq!(p.files.len(), 10);
        let p = g.page(Some(16), 10, 1 << 20);
        assert_eq!((p.page_first_seq, p.page_last_seq), (6, 15));
        let p = g.page(Some(6), 10, 1 << 20);
        assert_eq!((p.page_first_seq, p.next_before), (1, None));
        // A small byte budget trims the oldest end and says where to continue.
        let p = g.page(None, 25, 2000);
        assert!(p.steps.len() < 25 && p.page_last_seq == 25);
        assert_eq!(p.next_before, Some(p.page_first_seq));
    }

    #[test]
    fn codex_reads_searches_runs_and_patch_results() {
        let mut g = ConvGraph::new("codex", "x", "", PathBuf::from("/nonexistent"));
        g.home = "/home/u".into();
        let lines = [
            r#"{"type":"turn_context","payload":{"cwd":"/r"}}"#,
            r#"{"type":"event_msg","timestamp":"2026-11-04T00:00:00Z","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"Review it"}]}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","cwd":"file:///r","exit_code":0,"parsed_cmd":[{"type":"read","cmd":"sed -n 1,9p a.rs","path":"src/a.rs"},{"type":"search","cmd":"rg foo","query":"foo","path":"src"}]}}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution","cwd":"file:///r","exit_code":2,"parsed_cmd":[{"type":"unknown","cmd":"cargo test"}]}}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"c1","name":"apply_patch","input":"*** Begin Patch\n*** Update File: src/zz.rs"}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c1","output":[{"type":"input_text","text":"Success. Updated the following files:\nM src/a.rs\nA src/new.rs\n"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"c2","name":"exec","input":"echo '*** Begin Patch'; cat notes.md"}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c2","output":"Success. Updated the following files:\nM src/quoted.rs\n"}}"#,
            r#"{"type":"event_msg","payload":{"type":"task_complete"}}"#,
        ];
        for l in lines {
            g.ingest(l, 0);
        }
        let o = g.overview(32, 24, 8);
        let kinds: Vec<_> = o.steps.iter().map(|s| (s.kind, s.outcome)).collect();
        assert_eq!(
            kinds,
            vec![
                (StepKind::Prompt, Outcome::Ok),
                (StepKind::Read, Outcome::Ok),
                (StepKind::Search, Outcome::Ok),
                (StepKind::Run, Outcome::Failed),
                (StepKind::Edit, Outcome::Ok),
                (StepKind::Write, Outcome::Ok),
                (StepKind::Done, Outcome::Ok),
            ]
        );
        let names: Vec<_> = o.files.iter().map(|f| f.display.as_str()).collect();
        assert_eq!(names, vec!["src/a.rs", "src", "src/new.rs"]);
        // The patch's input named zz.rs; only the result counts.
        assert!(!o.files.iter().any(|f| f.abs.ends_with("zz.rs")));
        // Another tool's output that merely quotes the success line is not an edit.
        assert!(!o.files.iter().any(|f| f.abs.ends_with("quoted.rs")));
        assert_eq!((o.files[0].reads, o.files[0].edits), (1, 1));
        assert_eq!(o.steps[3].label, "cargo test");
    }

    fn bash(id: &str, command: &str, cwd: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": T0,
            "cwd": cwd,
            "message": {"content": [{
                "type": "tool_use", "id": id, "name": "Bash",
                "input": {"command": command, "description": "look around"},
            }]},
        })
        .to_string()
    }

    fn kinds(g: &ConvGraph) -> Vec<(StepKind, Outcome, String)> {
        let o = g.overview(64, 64, 8);
        o.steps
            .iter()
            .map(|s| {
                let f = s.file.and_then(|i| o.files.iter().find(|f| f.id == i));
                (
                    s.kind,
                    s.outcome,
                    f.map(|f| f.display.clone()).unwrap_or_default(),
                )
            })
            .collect()
    }

    #[test]
    fn a_bash_call_is_a_step_per_file_it_touched() {
        let mut g = claude("/r");
        // The record's cwd is where Claude's shell was, not where the conversation began.
        g.ingest(
            &bash(
                "b1",
                "sed -n 1,60p src/a.rs && sed -i '' 's/x/y/' src/b.rs",
                "/r/sub",
            ),
            0,
        );
        assert_eq!(
            kinds(&g),
            vec![
                (StepKind::Read, Outcome::Pending, "sub/src/a.rs".into()),
                (StepKind::Edit, Outcome::Pending, "sub/src/b.rs".into()),
            ],
            "every part understood: no Run step"
        );
        assert_eq!(g.overview(64, 64, 8).pending, vec![1, 2]);
        g.ingest(&result("b1", false, T1), 0);
        assert_eq!(
            kinds(&g).iter().map(|k| k.1).collect::<Vec<_>>(),
            vec![Outcome::Ok, Outcome::Ok]
        );
        let o = g.overview(64, 64, 8);
        assert_eq!((o.counts.reads, o.counts.edits, o.counts.runs), (1, 1, 0));
        assert!(o.pending.is_empty());
    }

    #[test]
    fn a_bash_part_its_exit_does_not_vouch_for_is_unknown() {
        let mut g = claude("/r");
        g.ingest(&bash("b1", "cat a.rs | head -5; git status", "/r"), 0);
        g.ingest(&result("b1", false, T1), 0);
        assert_eq!(
            kinds(&g),
            vec![
                (StepKind::Run, Outcome::Ok, String::new()),
                (StepKind::Read, Outcome::Unknown, "a.rs".into()),
            ]
        );
        let o = g.overview(64, 64, 8);
        assert_eq!(o.steps[0].label, "look around");
        assert_eq!(
            (o.counts.reads, o.counts.runs),
            (0, 1),
            "shown, not counted"
        );
        // A failed call fails every part.
        g.ingest(&bash("b2", "cat b.rs && rm c.rs", "/r"), 0);
        g.ingest(&result("b2", true, T1), 0);
        let k = kinds(&g);
        assert_eq!(
            &k[2..],
            &[
                (StepKind::Read, Outcome::Failed, "b.rs".into()),
                (StepKind::Delete, Outcome::Failed, "c.rs".into()),
            ]
        );
        assert_eq!(g.overview(64, 64, 8).counts.edits, 0);
    }

    #[test]
    fn a_bash_call_settles_after_its_steps_left_the_history() {
        let mut g = claude("/r");
        g.ingest(&bash("slow", "sed -i 's/a/b/' f.rs", "/r"), 0);
        for i in 0..MAX_STEPS + 10 {
            g.ingest(
                &call(&format!("t{i}"), "Read", r#"{"file_path":"/r/x"}"#, T0),
                0,
            );
            g.ingest(&result(&format!("t{i}"), false, T0), 0);
        }
        assert!(
            g.steps.front().unwrap().seq > 1,
            "the edit step was dropped"
        );
        g.ingest(&result("slow", false, T1), 0);
        let o = g.overview(64, 1, 8);
        assert_eq!(o.counts.edits, 1, "it still counts");
        let f = g.files.iter().find(|f| f.abs == "/r/f.rs").unwrap();
        assert_eq!(f.edits, 1);
    }

    #[test]
    fn evicting_a_pending_bash_call_settles_every_part() {
        let mut g = claude("/r");
        g.ingest(&bash("old", "cat a && cat b", "/r"), 0);
        for i in 0..MAX_PENDING {
            g.ingest(
                &call(&format!("t{i}"), "Read", r#"{"file_path":"/r/x"}"#, T0),
                0,
            );
        }
        let first: Vec<_> = g.steps.iter().take(2).map(|s| (s.seq, s.outcome)).collect();
        assert_eq!(first, vec![(1, Outcome::Unknown), (2, Outcome::Unknown)]);
        assert!(!g.overview(64, 64, 8).pending.contains(&2));
    }

    #[test]
    fn a_pending_bash_call_survives_a_restart() {
        let dir = tmpdir("bash-resume");
        let t = dir.join("c1.jsonl");
        std::fs::write(&t, format!("{}\n", bash("b1", "cat a && cat b", "/r"))).unwrap();
        let mut g = ConvGraph::new("claude", "c1", "/r", t.clone());
        let Read_::Lines(lines, _) = g.sources[0].read(1 << 20).unwrap() else {
            panic!()
        };
        for l in &lines {
            g.ingest(l, 0);
        }
        let store = dir.join("graphs");
        g.save(&store).unwrap();
        let mut back = ConvGraph::load(&store, "claude", "c1", &t).expect("resumes");
        back.ingest(&result("b1", false, T1), 0);
        assert_eq!(back.overview(64, 64, 8).counts.reads, 2);
    }

    #[test]
    fn codex_shell_writes_and_what_parsed_cmd_gave_up_on() {
        let mut g = ConvGraph::new("codex", "x", "/r", PathBuf::from("/nonexistent"));
        let item = |cmd: &str, types: &[(&str, &str)]| {
            let parsed: Vec<Value> = types
                .iter()
                .map(|(t, p)| serde_json::json!({"type": t, "cmd": cmd, "path": p}))
                .collect();
            serde_json::json!({"type":"event_msg","payload":{"type":"item_completed","item":{
                "type":"CommandExecution","cwd":"file:///r","exit_code":0,
                "command":["/bin/zsh","-lc",cmd],"parsed_cmd":parsed}}})
            .to_string()
        };
        // Codex calls this a read; it also writes `b`.
        g.ingest(&item("cat a > b", &[("read", "a")]), 0);
        // Codex gave up on the pipeline.
        g.ingest(&item("sed -n 1,9p c | head -3", &[("unknown", "")]), 0);
        // Fully understood by parsed_cmd: used as before.
        g.ingest(&item("sed -n 1,9p d", &[("read", "d")]), 0);
        g.ingest(
            &item("sed -i '' 's/a/b/' e && cargo test", &[("unknown", "")]),
            0,
        );
        assert_eq!(
            kinds(&g),
            vec![
                (StepKind::Read, Outcome::Ok, "a".into()),
                (StepKind::Write, Outcome::Ok, "b".into()),
                (StepKind::Read, Outcome::Unknown, "c".into()),
                (StepKind::Read, Outcome::Ok, "d".into()),
                (StepKind::Run, Outcome::Ok, String::new()),
                (StepKind::Edit, Outcome::Ok, "e".into()),
            ]
        );
    }

    #[test]
    fn the_reader_holds_a_partial_line_and_skips_an_oversized_one() {
        let mut s = Source::new(PathBuf::new(), 0);
        assert_eq!(s.feed(b"{\"a\":1}\n{\"b\""), vec!["{\"a\":1}".to_string()]);
        assert_eq!(s.offset, 8);
        assert_eq!(s.feed(b":2}\n"), vec!["{\"b\":2}".to_string()]);
        assert_eq!(s.offset, 16);
        let big = vec![b'x'; MAX_LINE + 1];
        assert!(s.feed(&big).is_empty());
        assert!(s.skipping);
        assert_eq!(s.feed(b"xx\n{\"c\":3}\n"), vec!["{\"c\":3}".to_string()]);
        assert_eq!(s.offset, 16 + MAX_LINE as u64 + 1 + 3 + 8);
        // An oversized line that ENDS within the read that completes it is skipped too.
        let mut s = Source::new(PathBuf::new(), 0);
        assert!(s.feed(&vec![b'y'; MAX_LINE]).is_empty());
        assert!(!s.skipping);
        assert_eq!(s.feed(b"y\n{}\n"), vec!["{}".to_string()]);
        assert_eq!(s.offset, MAX_LINE as u64 + 2 + 3);
    }

    #[test]
    fn a_saved_graph_resumes_only_while_its_files_match() {
        let dir = tmpdir("resume");
        let t = dir.join("c1.jsonl");
        std::fs::write(
            &t,
            format!("{}\n", call("t1", "Read", r#"{"file_path":"/r/a"}"#, T0)),
        )
        .unwrap();
        let mut g = ConvGraph::new("claude", "c1", "/r", t.clone());
        let Read_::Lines(lines, _) = g.sources[0].read(1 << 20).unwrap() else {
            panic!()
        };
        for l in &lines {
            g.ingest(l, 0);
        }
        let store = dir.join("graphs");
        g.save(&store).unwrap();
        let back = ConvGraph::load(&store, "claude", "c1", &t).expect("resumes");
        assert_eq!(back.generation, g.generation);
        assert_eq!(back.sources[0].offset, g.sources[0].offset);
        assert_eq!(back.pending.len(), 1, "a pending call survives the restart");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.join("claude-c1.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Rewritten under the same name: the bytes before the offset differ.
        std::fs::write(
            &t,
            format!("{}\n", call("t2", "Read", r#"{"file_path":"/r/b"}"#, T0)),
        )
        .unwrap();
        assert!(ConvGraph::load(&store, "claude", "c1", &t).is_none());
    }

    #[test]
    fn a_replaced_transcript_asks_for_a_rebuild() {
        let dir = tmpdir("replace");
        let t = dir.join("c.jsonl");
        std::fs::write(&t, "{}\n{}\n").unwrap();
        let mut s = Source::new(t.clone(), 0);
        assert!(matches!(s.read(1 << 20).unwrap(), Read_::Lines(l, 6) if l.len() == 2));
        std::fs::write(&t, "{}\n").unwrap();
        assert_eq!(s.read(1 << 20).unwrap(), Read_::Rebuild);
    }

    #[test]
    fn file_names_refuse_odd_ids() {
        assert_eq!(
            ConvGraph::file_name("claude", "ab-12").as_deref(),
            Some("claude-ab-12.json")
        );
        assert!(ConvGraph::file_name("claude", "../x").is_none());
        assert!(ConvGraph::file_name("", "x").is_none());
    }

    #[test]
    fn prune_drops_the_oldest_beyond_keep() {
        let dir = tmpdir("prune");
        for i in 0..4 {
            std::fs::write(dir.join(format!("claude-{i}.json")), "{}").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        prune(&dir, 30, 2);
        let mut left: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        left.sort();
        assert_eq!(left, vec!["claude-2.json", "claude-3.json"]);
    }
}
