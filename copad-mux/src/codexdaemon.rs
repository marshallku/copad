//! Codex conversations held by the shared app-server daemon.
//!
//! Since codex-cli 0.160 the interactive TUI attaches to a managed app-server daemon
//! (`$CODEX_HOME/app-server-daemon/daemon.pid`) and the DAEMON owns the rollout file, so the
//! pane's `codex` pid holds nothing that names its conversation and restore fell back to a
//! fresh `codex`. Nothing Codex exposes maps a client pid to a thread (the rollout's
//! `session_meta`, `state_5.sqlite` and the app-server's `thread/loaded/list` carry no client
//! pid), so this correlates instead:
//!
//! - the rollout is open in the daemon (the thread is loaded),
//! - its `session_meta.cwd` is the TUI's cwd and its originator is interactive,
//! - it was written after the TUI started (rules out a thread whose TUI exited before this one
//!   was launched — the daemon keeps a thread loaded after its client goes away).
//!
//! The answer is the ONE interactive thread loaded for that cwd, if it was also written since
//! launch — and only while this is the sole live Codex TUI there. Several loaded threads (this
//! pane may idle on a resumed one while a sibling's — live or exited — is fresh), a live sibling,
//! or a thread seen loaded beside a sibling earlier all yield `None` (a fresh start), because resuming another pane's conversation is worse
//! than resuming none — unless this process was resolved uniquely earlier, which is remembered
//! per `(pid, start time)` so a second `codex` opened later in the same repo doesn't erase an
//! answer the autosave already had.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::agentsessions::CODEX_HEADLESS;
use crate::procinfo;

/// Bytes read looking for line 1. `session_meta` embeds the base instructions (~22 KB seen).
const META_SCAN: u64 = 256 * 1024;

/// A daemon-held rollout, reduced to what the correlation needs.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Candidate {
    id: String,
    cwd: PathBuf,
    headless: bool,
    mtime: SystemTime,
}

/// What earlier lookups established, kept for the server's lifetime.
#[derive(Default)]
struct Memory {
    /// `(tui pid, its start time)` → the conversation it was last uniquely resolved to.
    resolved: HashMap<(u32, SystemTime), String>,
    /// Threads seen loaded in a cwd while two TUIs ran there. Any of them may belong to a TUI
    /// that has since EXITED (the daemon keeps it loaded), so being the lone fresh thread later
    /// still doesn't make one ours.
    tainted: HashSet<String>,
}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// What a lookup may fall back on, from [`Memory`].
struct Recall<'a> {
    known: Option<&'a str>,
    tainted: &'a HashSet<String>,
}

/// The conversation a daemon-attached Codex TUI (`pid`) is in, or `None` when it can't be
/// told apart from another loaded thread.
pub fn session_id(pid: u32) -> Option<String> {
    let start = procinfo::process_start_time(pid)?;
    let cwd = canonical(&procinfo::process_cwd(pid)?);
    // All or nothing: dropping an unreadable rollout could leave a sibling's as the "only" one,
    // and would let the taint pruning below forget a thread that is still loaded.
    let candidates: Vec<Candidate> = daemon_rollouts()?
        .iter()
        .map(|p| candidate(p))
        .collect::<Option<_>>()?;
    let sibling = shares_cwd_with_another_tui(pid, &cwd)?;
    let key = (pid, start);
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    let mem = guard.get_or_insert_with(Memory::default);
    // Forget threads the daemon has unloaded, so the set stays bounded.
    mem.tainted
        .retain(|id| candidates.iter().any(|c| &c.id == id));
    if sibling {
        mem.tainted
            .extend(loaded_here(&candidates, &cwd).map(|c| c.id.clone()));
    }
    let recall = Recall {
        known: mem.resolved.get(&key).map(String::as_str),
        tainted: &mem.tainted,
    };
    let id = if sibling {
        remembered(&candidates, &cwd, recall.known)
    } else {
        pick(&candidates, &cwd, start, &recall)
    }?;
    mem.resolved.insert(key, id.clone());
    Some(id)
}

/// Interactive threads the daemon holds for `cwd`.
fn loaded_here<'a>(
    candidates: &'a [Candidate],
    cwd: &'a Path,
) -> impl Iterator<Item = &'a Candidate> {
    candidates
        .iter()
        .filter(move |c| !c.headless && canonical(&c.cwd) == cwd)
}

/// The decision, split from the IO so it is testable.
fn pick(
    candidates: &[Candidate],
    cwd: &Path,
    start: SystemTime,
    recall: &Recall,
) -> Option<String> {
    // The ONE thread loaded for this cwd — not the one fresh thread among several: a pane idling
    // on a resumed thread the launch filter drops would otherwise inherit an exited sibling's
    // thread by elimination. Its own thread being loaded is what makes that case ambiguous.
    let here: Vec<&Candidate> = loaded_here(candidates, cwd).collect();
    if let [only] = here[..]
        && only.mtime >= start
        && !recall.tainted.contains(&only.id)
    {
        return Some(only.id.clone());
    }
    // Ambiguous (or nothing written since launch): a previous unique answer still stands while
    // its thread is loaded here — an idle conversation is still this pane's.
    remembered(candidates, cwd, recall.known)
}

/// `known`, while its thread is still loaded for `cwd`.
fn remembered(candidates: &[Candidate], cwd: &Path, known: Option<&str>) -> Option<String> {
    known
        .filter(|k| loaded_here(candidates, cwd).any(|c| c.id == *k))
        .map(str::to_string)
}

/// Whether another live Codex TUI runs in `cwd`. Then "the only thread written since launch"
/// proves nothing — it may be the OTHER pane's, while this one idles on a resumed thread the
/// launch-time filter excluded — so only a remembered answer is used. `None` = couldn't look.
/// Over-counting (a stray `codex` that isn't a TUI) only costs a fresh start.
fn shares_cwd_with_another_tui(pid: u32, cwd: &Path) -> Option<bool> {
    let tree = procinfo::ProcTree::snapshot()?;
    let peers = tree.pids_where(|comm| comm.starts_with("codex"));
    Some(peers.into_iter().filter(|&p| p != pid).any(|p| {
        let argv = procinfo::process_command(p).unwrap_or_default();
        may_hold_daemon_thread(&argv)
            && procinfo::process_cwd(p).is_some_and(|c| canonical(&c) == cwd)
    }))
}

/// Everything except what provably can't own a daemon thread: the daemon itself, an `exec` run
/// (only when `exec` is argv[1] — a later `exec` may be an option's value, `--profile exec`),
/// and a `--no-daemon` process, which keeps its rollout on its own pid.
fn may_hold_daemon_thread(argv: &[String]) -> bool {
    let first = argv.get(1).map(String::as_str);
    // Past `--` everything is a positional prompt (`codex -- --no-daemon`).
    let mut options = argv.iter().skip(1).take_while(|a| *a != "--");
    !(matches!(first, Some("app-server" | "exec"))
        || options.any(|a| a == "--no-daemon" || a == "--managed-daemon"))
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn codex_home() -> Option<PathBuf> {
    match std::env::var_os("CODEX_HOME") {
        Some(h) if !h.is_empty() => Some(PathBuf::from(h)),
        _ => Some(PathBuf::from(std::env::var_os("HOME")?).join(".codex")),
    }
}

/// Rollouts the live daemon holds open. The pid file can outlive its daemon, so the pid is
/// only trusted while it is still a `codex app-server` process.
fn daemon_rollouts() -> Option<Vec<PathBuf>> {
    let pidfile = codex_home()?.join("app-server-daemon/daemon.pid");
    let pid = daemon_pid(&std::fs::read_to_string(pidfile).ok()?)?;
    let argv = procinfo::process_command(pid)?;
    let is_codex = argv
        .first()
        .is_some_and(|a| a.rsplit('/').next() == Some("codex"));
    if !is_codex || !argv.iter().any(|a| a == "app-server") {
        return None;
    }
    let open = procinfo::open_files(pid)?;
    Some(
        open.into_iter()
            .filter(|p| crate::agentstate::is_rollout_path(p))
            .collect(),
    )
}

fn daemon_pid(json: &str) -> Option<u32> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    u32::try_from(v.get("pid")?.as_u64()?)
        .ok()
        .filter(|&p| p > 0)
}

fn candidate(path: &Path) -> Option<Candidate> {
    use std::io::BufRead;
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let md = f.metadata().ok()?;
    if !md.is_file() {
        return None;
    }
    let mtime = md.modified().ok()?;
    let mut line = String::new();
    std::io::BufReader::new(std::io::Read::take(f, META_SCAN))
        .read_line(&mut line)
        .ok()?;
    parse_meta(&line, mtime)
}

fn parse_meta(line: &str, mtime: SystemTime) -> Option<Candidate> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    let id = v.pointer("/payload/id")?.as_str()?;
    if !crate::agentstate::is_session_uuid(id) {
        return None;
    }
    let cwd = v
        .pointer("/payload/cwd")?
        .as_str()
        .filter(|c| !c.is_empty())?;
    // Same denylist as the resume picker: an originator we don't know counts as interactive.
    let headless = v
        .pointer("/payload/originator")
        .and_then(|o| o.as_str())
        .is_some_and(|o| CODEX_HEADLESS.contains(&o));
    Some(Candidate {
        id: id.to_string(),
        cwd: PathBuf::from(cwd),
        headless,
        mtime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const A: &str = "01a11142-410f-71b2-94c7-d1a0def9a980";
    const B: &str = "01a11140-fc21-74e2-b051-a2ed50ae6c63";

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    fn recall<'a>(known: Option<&'a str>, tainted: &'a HashSet<String>) -> Recall<'a> {
        Recall { known, tainted }
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn cand(id: &str, cwd: &str, mtime: u64) -> Candidate {
        Candidate {
            id: id.into(),
            cwd: cwd.into(),
            headless: false,
            mtime: at(mtime),
        }
    }

    // Paths that don't exist canonicalize to themselves, so these stay hermetic.
    const REPO: &str = "/nonexistent/repo";

    #[test]
    fn the_one_thread_loaded_for_this_cwd_wins_when_written_since_launch() {
        let cs = [
            cand(A, REPO, 200),
            cand(
                "01a11140-22ec-7540-9208-38098abd6d97",
                "/nonexistent/other",
                300,
            ),
        ];
        assert_eq!(
            pick(&cs, Path::new(REPO), at(100), &recall(None, &none())).as_deref(),
            Some(A)
        );
        // A lone thread untouched since before our launch belongs to a TUI that exited first.
        assert_eq!(
            pick(&cs, Path::new(REPO), at(250), &recall(None, &none())),
            None
        );
    }

    #[test]
    fn an_idle_resumed_thread_blocks_claiming_a_siblings_by_elimination() {
        // This pane idles on a resumed thread (A, untouched since before launch); an exited
        // sibling's thread (B) is the lone FRESH one — but two are loaded, so neither is claimed.
        let cs = [cand(A, REPO, 50), cand(B, REPO, 300)];
        assert_eq!(
            pick(&cs, Path::new(REPO), at(100), &recall(None, &none())),
            None
        );
    }

    #[test]
    fn two_live_threads_in_one_cwd_are_ambiguous() {
        let cs = [cand(A, REPO, 200), cand(B, REPO, 300)];
        assert_eq!(
            pick(&cs, Path::new(REPO), at(100), &recall(None, &none())),
            None
        );
    }

    #[test]
    fn an_earlier_unique_answer_survives_a_second_tui_and_going_idle() {
        let cs = [cand(A, REPO, 200), cand(B, REPO, 300)];
        assert_eq!(
            pick(&cs, Path::new(REPO), at(100), &recall(Some(A), &none())).as_deref(),
            Some(A)
        );
        // A unique fresh thread wins over the memory, and an unloaded one is forgotten.
        assert_eq!(
            pick(
                &cs[1..],
                Path::new(REPO),
                at(100),
                &recall(Some(A), &none())
            )
            .as_deref(),
            Some(B)
        );
    }

    #[test]
    fn a_thread_seen_beside_a_sibling_is_never_claimed_by_elimination() {
        // B was loaded while a sibling ran: even as the only thread left, it isn't ours.
        let cs = [cand(B, REPO, 300)];
        let tainted: HashSet<String> = [B.to_string()].into();
        assert_eq!(
            pick(&cs, Path::new(REPO), at(100), &recall(None, &tainted)),
            None
        );
    }

    #[test]
    fn only_provable_non_tuis_are_ignored_as_siblings() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(may_hold_daemon_thread(&v(&["codex"])));
        assert!(may_hold_daemon_thread(&v(&["codex", "--profile", "exec"])));
        assert!(!may_hold_daemon_thread(&v(&["codex", "exec", "x"])));
        assert!(may_hold_daemon_thread(&v(&["codex", "--", "--no-daemon"])));
        assert!(!may_hold_daemon_thread(&v(&[
            "codex",
            "--no-daemon",
            "exec"
        ])));
        assert!(!may_hold_daemon_thread(&v(&[
            "codex",
            "app-server",
            "--managed-daemon"
        ])));
    }

    #[test]
    fn headless_threads_never_match() {
        let mut c = cand(A, REPO, 200);
        c.headless = true;
        assert_eq!(
            pick(&[c], Path::new(REPO), at(100), &recall(None, &none())),
            None
        );
    }

    #[test]
    fn session_meta_and_daemon_pid_parse() {
        let line = format!(
            r#"{{"type":"session_meta","payload":{{"id":"{A}","cwd":"/r","originator":"codex_exec"}}}}"#
        );
        let c = parse_meta(&line, at(1)).unwrap();
        assert_eq!(c.cwd, PathBuf::from("/r"));
        assert!(c.headless);
        let tui = line.replace("codex_exec", "codex-tui");
        assert!(!parse_meta(&tui, at(1)).unwrap().headless);
        assert!(parse_meta(&line.replace("session_meta", "turn_context"), at(1)).is_none());

        assert_eq!(
            daemon_pid(r#"{"pid":49356,"processStartTime":"x"}"#),
            Some(49356)
        );
        assert_eq!(daemon_pid(r#"{"pid":0}"#), None);
        assert_eq!(daemon_pid("garbage"), None);
    }
}
