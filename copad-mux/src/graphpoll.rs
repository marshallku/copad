//! Background reader for the work graph (decisions #204; the model is [`crate::workgraph`]).
//!
//! Same shape as `agentpoll`: `spawn() -> Shared`, the render loop publishes what it wants
//! tracked (`set_wanted`, at the label cadence) and reads published graphs, and the lock is
//! never held across I/O. It adds no forks: which transcript an agent is in comes from
//! `agentpoll::conversation`, resolved and re-checked there.
//!
//! Each tick reads at most [`CONV_BUDGET`] per conversation and [`TICK_BUDGET`] in all,
//! round-robin, so a fresh 30 MB transcript cannot starve the others.
//!
//! **Disk.** With `persist` on, a changed graph is saved at most every [`SAVE_EVERY`]; a
//! conversation nobody wants any more is saved once more and dropped from memory after
//! [`EVICT_AFTER`]. Saved graphs older than `retain_days` (and the oldest past [`KEEP_FILES`])
//! are pruned at start and hourly. With `persist` off nothing is written — and nothing already
//! on disk is touched.

use crate::workgraph::{ConvGraph, Read_};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const TICK: Duration = Duration::from_secs(1);
const CONV_BUDGET: usize = 2 << 20;
const TICK_BUDGET: usize = 8 << 20;
const SAVE_EVERY: Duration = Duration::from_secs(5);
const EVICT_AFTER: Duration = Duration::from_secs(600);
const PRUNE_EVERY: Duration = Duration::from_secs(3600);
const KEEP_FILES: usize = 500;
/// How often a Claude conversation's `subagents/` directory is looked at; a full listing
/// happens when its mtime moved, or every [`SUB_RESCAN`] regardless.
const SUB_CHECK: Duration = Duration::from_secs(2);
const SUB_RESCAN: Duration = Duration::from_secs(10);
const MAX_TRACKED: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Want {
    pub pid: u32,
    pub tool: String,
    pub conversation: String,
    pub transcript: PathBuf,
    pub cwd: String,
}

/// The `mux.toml` knobs, passed each time so a `comux reload` applies on the next tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub persist: bool,
    pub retain_days: u32,
    pub dir: PathBuf,
}

#[derive(Default)]
pub struct Inner {
    wanted: Vec<Want>,
    settings: Option<Settings>,
    graphs: HashMap<u32, Arc<ConvGraph>>,
    rev: u64,
}

pub type Shared = Arc<Mutex<Inner>>;

pub fn idle() -> Shared {
    Arc::new(Mutex::new(Inner::default()))
}

pub fn set_wanted(shared: &Shared, wanted: Vec<Want>, settings: Settings) {
    if let Ok(mut g) = shared.lock() {
        g.wanted = wanted;
        g.settings = Some(settings);
    }
}

/// Apply new settings now (a `comux reload`), without waiting for the next agent sweep.
pub fn set_settings(shared: &Shared, settings: Settings) {
    if let Ok(mut g) = shared.lock() {
        g.settings = Some(settings);
    }
}

pub fn snapshot(shared: &Shared) -> (u64, HashMap<u32, Arc<ConvGraph>>) {
    shared
        .lock()
        .map(|g| (g.rev, g.graphs.clone()))
        .unwrap_or_default()
}

pub fn tracks(tool: &str) -> bool {
    matches!(tool, "claude" | "codex")
}

struct Tracked {
    graph: ConvGraph,
    last_wanted: Instant,
    saved_rev: u64,
    last_save: Option<Instant>,
    published: Option<Arc<ConvGraph>>,
    sub_checked: Option<Instant>,
    sub_listed: Option<Instant>,
    sub_mtime: Option<SystemTime>,
    metas: HashSet<PathBuf>,
}

type Key = (String, String);

pub fn spawn() -> Shared {
    let shared = idle();
    let out = shared.clone();
    let _ = std::thread::Builder::new()
        .name("graph-poll".into())
        .spawn(move || {
            let mut tracked: HashMap<Key, Tracked> = HashMap::new();
            let mut cursor = 0usize;
            let mut pruned: Option<Instant> = None;
            loop {
                let (wanted, settings) = match out.lock() {
                    Ok(g) => (g.wanted.clone(), g.settings.clone()),
                    Err(_) => (Vec::new(), None),
                };
                let Some(settings) = settings else {
                    std::thread::sleep(TICK);
                    continue;
                };
                if !settings.enabled {
                    tracked.clear();
                    publish(&out, HashMap::new());
                    std::thread::sleep(TICK);
                    continue;
                }
                if settings.persist && pruned.is_none_or(|t| t.elapsed() >= PRUNE_EVERY) {
                    crate::workgraph::prune(&settings.dir, settings.retain_days, KEEP_FILES);
                    pruned = Some(Instant::now());
                }
                let graphs = tick(&wanted, &settings, &mut tracked, &mut cursor);
                publish(&out, graphs);
                std::thread::sleep(TICK);
            }
        });
    shared
}

fn publish(out: &Shared, graphs: HashMap<u32, Arc<ConvGraph>>) {
    if let Ok(mut g) = out.lock() {
        let same = g.graphs.len() == graphs.len()
            && graphs
                .iter()
                .all(|(pid, a)| g.graphs.get(pid).is_some_and(|b| Arc::ptr_eq(a, b)));
        if !same {
            g.graphs = graphs;
            g.rev += 1;
        }
    }
}

fn tick(
    wanted: &[Want],
    settings: &Settings,
    tracked: &mut HashMap<Key, Tracked>,
    cursor: &mut usize,
) -> HashMap<u32, Arc<ConvGraph>> {
    let now = Instant::now();
    let wanted: Vec<&Want> = wanted
        .iter()
        .filter(|w| tracks(&w.tool) && ConvGraph::file_name(&w.tool, &w.conversation).is_some())
        .take(MAX_TRACKED)
        .collect();
    let mut total = TICK_BUDGET;
    let n = wanted.len();
    let start = *cursor % n.max(1);
    *cursor = cursor.wrapping_add(1);
    let mut out = HashMap::new();
    for w in wanted.iter().cycle().skip(start).take(n) {
        let key = (w.tool.clone(), w.conversation.clone());
        let t = tracked.entry(key).or_insert_with(|| Tracked {
            graph: open(w, settings),
            last_wanted: now,
            saved_rev: 0,
            last_save: None,
            published: None,
            sub_checked: None,
            sub_listed: None,
            sub_mtime: None,
            metas: HashSet::new(),
        });
        t.last_wanted = now;
        if t.graph.main_path() != w.transcript {
            // The same conversation id under another file: start over.
            *t = fresh(w);
        }
        if t.graph.cwd.is_empty() {
            t.graph.cwd = w.cwd.clone();
        }
        if w.tool == "claude" {
            scan_subagents(t, &w.transcript, now);
        }
        let mut budget = CONV_BUDGET.min(total);
        let mut i = 0;
        while i < t.graph.sources.len() && budget > 0 {
            let agent = t.graph.sources[i].agent;
            match t.graph.sources[i].read(budget) {
                Ok(Read_::Lines(lines, read)) => {
                    budget = budget.saturating_sub(read.max(1));
                    total = total.saturating_sub(read);
                    for l in &lines {
                        t.graph.ingest(l, agent);
                    }
                    i += 1;
                }
                Ok(Read_::Rebuild) => {
                    t.graph =
                        ConvGraph::new(&w.tool, &w.conversation, &w.cwd, w.transcript.clone());
                    t.metas.clear();
                    t.sub_listed = None;
                    i = 0;
                }
                Err(_) => i += 1,
            }
        }
        if settings.persist
            && t.graph.rev != t.saved_rev
            && t.last_save.is_none_or(|s| s.elapsed() >= SAVE_EVERY)
            && t.graph.save(&settings.dir).is_ok()
        {
            t.saved_rev = t.graph.rev;
            t.last_save = Some(now);
        }
        let stale = t
            .published
            .as_ref()
            .is_none_or(|p| p.rev != t.graph.rev || p.generation != t.graph.generation);
        if stale {
            t.published = Some(Arc::new(t.graph.clone()));
        }
        if let Some(p) = &t.published {
            out.insert(w.pid, p.clone());
        }
    }
    tracked.retain(|_, t| {
        let keep = t.last_wanted.elapsed() < EVICT_AFTER;
        if !keep && settings.persist && t.graph.rev != t.saved_rev {
            let _ = t.graph.save(&settings.dir);
        }
        keep
    });
    out
}

fn open(w: &Want, settings: &Settings) -> ConvGraph {
    settings
        .persist
        .then(|| ConvGraph::load(&settings.dir, &w.tool, &w.conversation, &w.transcript))
        .flatten()
        .unwrap_or_else(|| ConvGraph::new(&w.tool, &w.conversation, &w.cwd, w.transcript.clone()))
}

fn fresh(w: &Want) -> Tracked {
    Tracked {
        graph: ConvGraph::new(&w.tool, &w.conversation, &w.cwd, w.transcript.clone()),
        last_wanted: Instant::now(),
        saved_rev: 0,
        last_save: None,
        published: None,
        sub_checked: None,
        sub_listed: None,
        sub_mtime: None,
        metas: HashSet::new(),
    }
}

fn scan_subagents(t: &mut Tracked, transcript: &Path, now: Instant) {
    if t.sub_checked
        .is_some_and(|c| now.duration_since(c) < SUB_CHECK)
    {
        return;
    }
    t.sub_checked = Some(now);
    let dir = transcript.with_extension("").join("subagents");
    let Ok(meta) = std::fs::metadata(&dir) else {
        return;
    };
    let mtime = meta.modified().ok();
    let due = t
        .sub_listed
        .is_none_or(|l| now.duration_since(l) >= SUB_RESCAN);
    if mtime == t.sub_mtime && !due {
        return;
    }
    t.sub_mtime = mtime;
    t.sub_listed = Some(now);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut found: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("agent-") && n.ends_with(".meta.json"))
        })
        .filter(|p| !t.metas.contains(p))
        .collect();
    found.sort();
    for p in found {
        let Some(m) = std::fs::read(&p)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        else {
            continue;
        };
        let s = |k: &str| {
            m.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let jsonl = p.with_file_name(name.trim_end_matches(".meta.json").to_string() + ".jsonl");
        let at = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_millis() as i64);
        t.graph.found_subagent(
            &s("toolUseId"),
            &s("agentType"),
            &s("description"),
            jsonl,
            at,
        );
        t.metas.insert(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("copad-graphpoll-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const CID: &str = "cb586121-d8cd-4c86-8011-132f3917967f";

    fn line(id: &str, name: &str, input: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-11-04T00:00:00Z","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{input}}}]}}}}"#
        ) + "\n"
    }

    fn setup(tag: &str, persist: bool) -> (PathBuf, Want, Settings) {
        let dir = tmpdir(tag);
        let t = dir.join(format!("{CID}.jsonl"));
        std::fs::write(&t, line("t1", "Read", r#"{"file_path":"/r/a.rs"}"#)).unwrap();
        let want = Want {
            pid: 7,
            tool: "claude".into(),
            conversation: CID.into(),
            transcript: t,
            cwd: "/r".into(),
        };
        let settings = Settings {
            enabled: true,
            persist,
            retain_days: 30,
            dir: dir.join("graphs"),
        };
        (dir, want, settings)
    }

    #[test]
    fn a_tick_reads_saves_and_picks_up_a_subagent() {
        let (dir, want, settings) = setup("tick", true);
        let mut tracked = HashMap::new();
        let mut cursor = 0;
        let out = tick(
            std::slice::from_ref(&want),
            &settings,
            &mut tracked,
            &mut cursor,
        );
        let g = out.get(&7).expect("published");
        assert_eq!(g.overview(32, 24, 8).files[0].display, "a.rs");
        assert!(settings.dir.join(format!("claude-{CID}.json")).exists());
        let subs = want.transcript.with_extension("").join("subagents");
        std::fs::create_dir_all(&subs).unwrap();
        std::fs::write(
            subs.join("agent-x1.meta.json"),
            r#"{"agentType":"Explore","description":"look","toolUseId":"a1"}"#,
        )
        .unwrap();
        std::fs::write(
            subs.join("agent-x1.jsonl"),
            line("s1", "Grep", r#"{"pattern":"p","path":"/r/src"}"#),
        )
        .unwrap();
        for t in tracked.values_mut() {
            t.sub_checked = None;
        }
        let out = tick(
            std::slice::from_ref(&want),
            &settings,
            &mut tracked,
            &mut cursor,
        );
        let o = out.get(&7).unwrap().overview(32, 24, 8);
        assert_eq!(o.subagents[0].kind, "Explore");
        assert!(o.steps.iter().any(|s| s.agent == 1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn persist_off_writes_nothing() {
        let (dir, want, settings) = setup("nopersist", false);
        let mut tracked = HashMap::new();
        tick(std::slice::from_ref(&want), &settings, &mut tracked, &mut 0);
        assert!(!settings.dir.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_unchanged_graph_is_not_cloned_again() {
        let (dir, want, settings) = setup("same", false);
        let mut tracked = HashMap::new();
        let a = tick(std::slice::from_ref(&want), &settings, &mut tracked, &mut 0);
        let b = tick(std::slice::from_ref(&want), &settings, &mut tracked, &mut 0);
        assert!(Arc::ptr_eq(&a[&7], &b[&7]));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_restart_resumes_from_the_saved_offset() {
        let (dir, want, settings) = setup("resume", true);
        let mut tracked = HashMap::new();
        tick(std::slice::from_ref(&want), &settings, &mut tracked, &mut 0);
        let gen1 = tracked.values().next().unwrap().graph.generation.clone();
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&want.transcript)
            .unwrap();
        f.write_all(line("t2", "Edit", r#"{"file_path":"/r/b.rs"}"#).as_bytes())
            .unwrap();
        let mut tracked = HashMap::new();
        let out = tick(std::slice::from_ref(&want), &settings, &mut tracked, &mut 0);
        let g = &out[&7];
        assert_eq!(g.generation, gen1, "resumed, not rebuilt");
        assert_eq!(
            g.overview(32, 24, 8).counts.steps,
            2,
            "the first step was not read twice"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
