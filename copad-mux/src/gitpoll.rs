//! Per-repo working-tree state for the sidebar subtitle (decisions #144): dirty, ahead/behind,
//! and the net line change against `HEAD`.
//!
//! Unlike the branch name (`gitinfo`, a fork-free `.git/HEAD` read), these need `git` itself,
//! so they run on a dedicated thread and never between frames. The render loop says which
//! repos it wants ([`set_wanted`]); the poller reads them round-robin under a time budget and
//! publishes [`GitStat`]s the loop renders.
//!
//! A background `git status` in somebody's repo is a guest. So every run:
//! - takes no optional locks (`--no-optional-locks`), which git documents for exactly this case
//!   — an index refresh would otherwise make the user's own concurrent git command fail;
//! - runs no repository-configured helper it can avoid (`core.fsmonitor=false`,
//!   `--no-ext-diff`, `--no-textconv`);
//! - inherits NO `GIT_*` variable from the server: a `GIT_DIR`/`GIT_WORK_TREE` in the
//!   daemon's environment would otherwise attribute one repo's state to every session;
//! - is killed, with its whole process group, at a deadline, so a filter or hook it spawned
//!   cannot hold the output pipe open past it.
//!
//! A failed or timed-out read DROPS the previous reading instead of showing it as current, and
//! every reading carries the branch it was taken on and when, so a checkout never shows the
//! previous branch's numbers (`fresh_for`).

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One repo's working-tree state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitStat {
    /// `branch.head` from `git status` (the branch, or `(detached)`).
    pub head: String,
    /// `branch.oid`: the commit checked out (`(initial)` on an unborn branch).
    pub oid: String,
    /// Tracked entries changed in the index or the worktree, including conflicts.
    pub changed: u32,
    /// Untracked entries (a new directory counts once, `--untracked-files=normal`).
    pub untracked: u32,
    pub ahead: u32,
    pub behind: u32,
    /// Net lines added / removed against `HEAD`, staged and unstaged together; untracked
    /// files and binary files contribute nothing.
    pub added: u32,
    pub removed: u32,
}

impl GitStat {
    pub fn dirty(&self) -> bool {
        self.changed > 0 || self.untracked > 0
    }
}

/// A published reading and when it was taken.
#[derive(Debug, Clone)]
pub struct Reading {
    pub stat: GitStat,
    pub at: Instant,
}

impl Reading {
    /// Whether this reading may be shown for a repo currently on `branch`: taken on that
    /// branch, and recently enough that it is not a frozen picture of a repo the poller has
    /// since been unable to read.
    /// `branch` is what `gitinfo::branch` displays: the branch name, or for a detached HEAD
    /// the commit's first 8 hex digits — matched against `branch.oid`.
    pub fn fresh_for(&self, branch: &str) -> bool {
        let same = if self.stat.head == "(detached)" {
            !branch.is_empty() && self.stat.oid.starts_with(branch)
        } else {
            self.stat.head == branch
        };
        same && self.at.elapsed() < STALE_AFTER
    }
}

/// The poller's shared state (opaque outside this module).
#[derive(Default)]
pub struct Inner {
    wanted: Vec<PathBuf>,
    seen: HashMap<PathBuf, Reading>,
}

pub type Shared = Arc<Mutex<Inner>>;

/// How often the poller wakes.
const TICK: Duration = Duration::from_secs(1);
/// The least time between two reads of one repo.
const MIN_INTERVAL: Duration = Duration::from_secs(5);
/// A repo whose read took longer than this is read less often ([`MAX_INTERVAL`] at most).
const SLOW_READ: Duration = Duration::from_millis(800);
const MAX_INTERVAL: Duration = Duration::from_secs(60);
/// Wall time one tick may spend running git, across all repos.
const TICK_BUDGET: Duration = Duration::from_secs(2);
/// Per-command deadline.
const DEADLINE: Duration = Duration::from_secs(3);
/// A reading older than this is not shown (the repo has failed to read since).
const STALE_AFTER: Duration = Duration::from_secs(90);

/// A handle that never updates — tests and any path with no server.
pub fn idle() -> Shared {
    Arc::new(Mutex::new(Inner::default()))
}

/// Tell the poller which repo roots to read. Roots no longer wanted are dropped.
pub fn set_wanted(shared: &Shared, mut wanted: Vec<PathBuf>) {
    wanted.sort();
    wanted.dedup();
    if let Ok(mut g) = shared.lock() {
        g.seen.retain(|root, _| wanted.contains(root));
        g.wanted = wanted;
    }
}

/// The latest reading for `root`, if any.
pub fn reading(shared: &Shared, root: &Path) -> Option<Reading> {
    shared.lock().ok()?.seen.get(root).cloned()
}

/// Publish a reading by hand — for other modules' tests, which never start the poller.
#[cfg(test)]
pub fn publish(shared: &Shared, root: PathBuf, stat: GitStat) {
    if let Ok(mut g) = shared.lock() {
        g.seen.insert(
            root,
            Reading {
                stat,
                at: Instant::now(),
            },
        );
    }
}

/// Start the poller thread.
pub fn spawn() -> Shared {
    let shared = idle();
    let out = shared.clone();
    let _ = std::thread::Builder::new()
        .name("git-poll".into())
        .spawn(move || {
            // Per root: when it is next due, and its current interval.
            let mut due: HashMap<PathBuf, (Instant, Duration)> = HashMap::new();
            loop {
                std::thread::sleep(TICK);
                let wanted = match out.lock() {
                    Ok(g) => g.wanted.clone(),
                    Err(_) => return,
                };
                due.retain(|r, _| wanted.contains(r));
                let now = Instant::now();
                // Longest-overdue first, so a slow repo cannot starve the others.
                let mut ready: Vec<(Instant, PathBuf)> = wanted
                    .iter()
                    .filter_map(|r| {
                        let at = due.get(r).map(|d| d.0).unwrap_or(now);
                        (at <= now).then(|| (at, r.clone()))
                    })
                    .collect();
                ready.sort();
                let tick_start = Instant::now();
                for (_, root) in ready {
                    if tick_start.elapsed() >= TICK_BUDGET {
                        break;
                    }
                    let started = Instant::now();
                    let stat = read_stat(&root);
                    let took = started.elapsed();
                    let prev = due.get(&root).map(|d| d.1).unwrap_or(MIN_INTERVAL);
                    let interval = if took > SLOW_READ {
                        (prev * 4).min(MAX_INTERVAL)
                    } else {
                        MIN_INTERVAL
                    };
                    due.insert(root.clone(), (Instant::now() + interval, interval));
                    if let Ok(mut g) = out.lock() {
                        if !g.wanted.contains(&root) {
                            continue;
                        }
                        match stat {
                            Some(stat) => {
                                g.seen.insert(
                                    root,
                                    Reading {
                                        stat,
                                        at: Instant::now(),
                                    },
                                );
                            }
                            // Never show a reading we could not refresh as if it were current.
                            None => {
                                g.seen.remove(&root);
                            }
                        }
                    }
                }
            }
        });
    shared
}

/// Read `root`'s state with two git runs. `None` when either fails or times out.
pub fn read_stat(root: &Path) -> Option<GitStat> {
    let status = run_git(
        root,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
            "--ignore-submodules=dirty",
        ],
    )?;
    let mut stat = parse_status(&status);
    // An unborn branch has no HEAD to diff against: nothing is "changed relative to HEAD".
    if !status.lines().any(|l| l == "# branch.oid (initial)") {
        let diff = run_git(
            root,
            &[
                "diff",
                "--shortstat",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=dirty",
                "HEAD",
                "--",
            ],
        )?;
        let (a, r) = parse_shortstat(&diff);
        stat.added = a;
        stat.removed = r;
    }
    Some(stat)
}

/// Run `git -C root <args>` as a well-behaved guest (see the module docs).
fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(&k);
        }
    }
    cmd.arg("-C")
        .arg(root)
        .args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotePath=false",
            "-c",
            "color.ui=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Its own process group, so a deadline can take down whatever it spawned too.
        .process_group(0);
    run_guarded(cmd, DEADLINE)
}

/// Run `cmd` (already in its own process group) and collect its stdout, with ONE deadline
/// covering both the process and the output.
///
/// The leader is detected as exited WITHOUT being reaped (`WNOWAIT`) and only reaped at the very
/// end. A zombie leader keeps its pid — and so the group id — reserved, which is what makes it
/// safe to SIGKILL the whole group at the deadline at any point here: a helper git spawned that
/// still holds the pipe after git itself exited is taken down with it, instead of surviving with
/// a reader thread blocked on it (one leaked pair per poll would add up over a server's life).
fn run_guarded(mut cmd: Command, deadline_in: Duration) -> Option<String> {
    let mut child = cmd.spawn().ok()?;
    let pid = child.id();
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    let deadline = Instant::now() + deadline_in;
    let kill_group = || {
        // SAFETY: a signal to the group we created; its leader is not reaped yet (see above).
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    };
    let mut timed_out = false;
    while !exited_unreaped(pid) {
        if Instant::now() >= deadline {
            kill_group();
            timed_out = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(out) => Some(out),
        Err(_) => {
            // Something in the group still holds the pipe: end it, then the reader sees EOF.
            kill_group();
            timed_out = true;
            rx.recv_timeout(Duration::from_secs(1)).ok()
        }
    };
    let status = child.wait().ok()?; // reap last: the group id stays ours until here
    (!timed_out && status.success()).then_some(out?)
}

/// Whether `pid` (our child) has exited, WITHOUT reaping it.
fn exited_unreaped(pid: u32) -> bool {
    // SAFETY: `waitid` writes only into `info`; WNOWAIT leaves the child waitable.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if r != 0 {
        return true; // not our child any more — nothing left to wait for
    }
    #[cfg(target_os = "linux")]
    let got = unsafe { info.si_pid() };
    #[cfg(not(target_os = "linux"))]
    let got = info.si_pid;
    got != 0
}

/// Parse `git status --porcelain=v2 --branch` (not `-z`: one record per line).
pub fn parse_status(s: &str) -> GitStat {
    let mut st = GitStat::default();
    for line in s.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            st.head = head.to_string();
        } else if let Some(oid) = line.strip_prefix("# branch.oid ") {
            st.oid = oid.to_string();
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    st.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    st.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ") {
            st.changed += 1;
        } else if line.starts_with("? ") {
            st.untracked += 1;
        }
    }
    st
}

/// Parse `git diff --shortstat` (`N files changed, A insertions(+), R deletions(-)`, either
/// count omitted when zero; empty when nothing changed). `LC_ALL=C` keeps it in English.
pub fn parse_shortstat(s: &str) -> (u32, u32) {
    let (mut added, mut removed) = (0, 0);
    for part in s.trim().split(", ") {
        let mut it = part.split_whitespace();
        let (Some(n), Some(what)) = (it.next(), it.next()) else {
            continue;
        };
        let n: u32 = n.parse().unwrap_or(0);
        if what.starts_with("insertion") {
            added = n;
        } else if what.starts_with("deletion") {
            removed = n;
        }
    }
    (added, removed)
}

/// The sidebar subtitle for a repo on `branch`: `[⑂ ]branch[*][ ↑A][ ↓B][ +a −r]`. `⑂` marks a
/// linked worktree; the rest only appears with a fresh reading for this branch.
pub fn subtitle(branch: &str, linked_worktree: bool, reading: Option<&Reading>) -> String {
    let mut s = String::new();
    if linked_worktree {
        s.push_str("⑂ ");
    }
    s.push_str(branch);
    let Some(r) = reading.filter(|r| r.fresh_for(branch)) else {
        return s;
    };
    let st = &r.stat;
    if st.dirty() {
        s.push('*');
    }
    if st.ahead > 0 {
        s.push_str(&format!(" ↑{}", st.ahead));
    }
    if st.behind > 0 {
        s.push_str(&format!(" ↓{}", st.behind));
    }
    if st.added > 0 || st.removed > 0 {
        s.push_str(&format!(" +{} −{}", st.added, st.removed));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_v2_counts_and_tracking() {
        let s = "# branch.oid 0123\n# branch.head main\n# branch.upstream origin/main\n\
                 # branch.ab +2 -1\n1 .M N... 100644 100644 100644 a b src/x.rs\n\
                 2 R. N... 100644 100644 100644 a b R100 new.rs\told.rs\n\
                 u UU N... 1 2 3 4 a b c conflict.rs\n? notes.txt\n? newdir/\n";
        let st = parse_status(s);
        assert_eq!(st.head, "main");
        assert_eq!((st.ahead, st.behind), (2, 1));
        assert_eq!(st.changed, 3, "modified + renamed + conflicted");
        assert_eq!(st.untracked, 2);
    }

    #[test]
    fn no_upstream_and_detached() {
        let st = parse_status("# branch.oid abc\n# branch.head (detached)\n");
        assert_eq!(st.head, "(detached)");
        assert_eq!((st.ahead, st.behind, st.changed), (0, 0, 0));
        assert!(!st.dirty());
    }

    #[test]
    fn shortstat_variants() {
        assert_eq!(
            parse_shortstat(" 3 files changed, 12 insertions(+), 3 deletions(-)\n"),
            (12, 3)
        );
        assert_eq!(parse_shortstat(" 1 file changed, 1 insertion(+)\n"), (1, 0));
        assert_eq!(parse_shortstat(" 1 file changed, 4 deletions(-)\n"), (0, 4));
        assert_eq!(parse_shortstat(""), (0, 0));
    }

    fn reading(head: &str) -> Reading {
        Reading {
            stat: GitStat {
                head: head.into(),
                oid: String::new(),
                changed: 1,
                untracked: 0,
                ahead: 2,
                behind: 0,
                added: 12,
                removed: 3,
            },
            at: Instant::now(),
        }
    }

    #[test]
    fn subtitle_decorates_only_a_fresh_reading_for_this_branch() {
        let r = reading("feat-x");
        assert_eq!(subtitle("feat-x", true, Some(&r)), "⑂ feat-x* ↑2 +12 −3");
        assert_eq!(
            subtitle("main", false, Some(&r)),
            "main",
            "another branch's numbers"
        );
        assert_eq!(subtitle("feat-x", false, None), "feat-x");
        let mut old = reading("feat-x");
        old.at = Instant::now() - STALE_AFTER - Duration::from_secs(1);
        assert_eq!(subtitle("feat-x", false, Some(&old)), "feat-x");
    }

    #[test]
    fn a_detached_reading_matches_its_short_sha() {
        let mut r = reading("(detached)");
        r.stat.oid = "a1b2c3d4e5f60718".into();
        assert!(r.fresh_for("a1b2c3d4"));
        assert!(!r.fresh_for("ffffffff"));
        assert!(!r.fresh_for(""));
    }

    #[test]
    fn a_descendant_holding_the_pipe_is_killed_at_the_deadline() {
        // `sleep` inherits stdout and outlives the shell: exactly a helper that keeps the pipe.
        // A unique duration names the process portably (no `exec -a` in dash).
        let marker = format!("sleep 31.{}", std::process::id() % 100_000);
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &format!("echo hi; {marker} & exit 0")])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let started = Instant::now();
        let out = run_guarded(cmd, Duration::from_millis(500));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the deadline held"
        );
        assert!(
            out.is_none(),
            "a run that had to be killed is not a reading"
        );
        std::thread::sleep(Duration::from_millis(200));
        let ps = Command::new("ps").args(["-Ao", "args="]).output().unwrap();
        assert!(
            !String::from_utf8_lossy(&ps.stdout).contains(&marker),
            "the pipe-holding descendant was killed with the group"
        );
    }

    #[test]
    fn a_quick_command_is_read_whole() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo one; echo two"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        assert_eq!(
            run_guarded(cmd, Duration::from_secs(3)).as_deref(),
            Some("one\ntwo\n")
        );
    }

    /// A real repository, when `git` is installed: the end-to-end contract of `read_stat`.
    #[test]
    fn reads_a_real_repo() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("comux-gitpoll-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
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
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        // Unborn: no HEAD to diff against, and still a reading.
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let st = read_stat(&dir).expect("unborn repo reads");
        assert_eq!(st.untracked, 1);
        assert_eq!((st.added, st.removed), (0, 0));
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "init"]);
        std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        let st = read_stat(&dir).expect("reads");
        assert_eq!(st.head, "main");
        assert_eq!(st.changed, 1);
        assert_eq!((st.added, st.removed), (2, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
