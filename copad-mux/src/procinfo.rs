//! Foreground-process + agent classification for pane labels.
//!
//! One sweep builds a pid→(ppid, pgid, comm) tree; from a pane's shell pid we
//! descend to the deepest descendant (the foreground-ish process) and classify
//! its command name as an AI agent, a shell, or something else. Cheap enough to
//! run on a throttled cadence (~2 Hz), never per frame.
//!
//! The sweep is IN-PROCESS (`/proc` on Linux, libproc on macOS) rather than a
//! `ps` fork: a mux server lives for weeks and would otherwise fork twice a
//! second forever, and every one of those forks was a chance to blank the whole
//! label map — see [`ProcTree::snapshot`].

use std::collections::HashMap;
use std::path::PathBuf;
// macOS shells out to `lsof` in `open_files`; the non-Linux/macOS fallback sweep
// shells out to `ps`. Linux reads /proc for both, so it needs no child processes.
#[cfg(not(target_os = "linux"))]
use std::process::Command;

/// The current working directory of a process (Linux `/proc/<pid>/cwd`, macOS
/// libproc `PROC_PIDVNODEPATHINFO`). Used to derive a session's git branch.
#[cfg(target_os = "linux")]
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(target_os = "macos")]
pub fn process_cwd(pid: u32) -> Option<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let sz = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: `info` is a zeroed, correctly-sized out-param for this pid.
    let r = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            sz,
        )
    };
    if r <= 0 {
        return None;
    }
    // `vip_path` is a fixed C char buffer (its Rust type varies across libc versions —
    // sometimes a nested array), so read it as a flat NUL-terminated byte buffer.
    let path = &info.pvi_cdir.vip_path;
    let len = std::mem::size_of_val(path);
    // SAFETY: `path` is a live, `len`-byte contiguous C char array.
    let bytes = unsafe { std::slice::from_raw_parts(path.as_ptr() as *const u8, len) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(len);
    if end == 0 {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(&bytes[..end])))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

/// The argv (structural, NOT space-joined) of `pid`, for restoring a whitelisted program
/// (agent) on session restore. Structural so argument boundaries + quoting survive
/// (`claude "a; b"` stays ONE arg, re-quoted on restore — never re-split into two shell
/// commands). Linux reads `/proc/<pid>/cmdline`; macOS uses `sysctl KERN_PROCARGS2`. `None`
/// if the process is gone or has no readable argv.
#[cfg(target_os = "linux")]
pub fn process_command(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let args: Vec<String> = raw
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    (!args.is_empty()).then_some(args)
}

#[cfg(target_os = "macos")]
pub fn process_command(pid: u32) -> Option<Vec<String>> {
    // KERN_PROCARGS2 buffer: [argc: i32][exec_path\0][padding \0…][argv0\0 argv1\0 …][env…].
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    // SAFETY: standard two-call sysctl — first sizes the buffer.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return None;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` holds `size` bytes for sysctl to fill.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < 4 {
        return None;
    }
    buf.truncate(size);
    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]).max(0) as usize;
    let mut p = 4;
    // Skip the exec_path string and any NUL padding after it.
    while p < buf.len() && buf[p] != 0 {
        p += 1;
    }
    while p < buf.len() && buf[p] == 0 {
        p += 1;
    }
    // Read exactly `argc` NUL-terminated args.
    let mut args = Vec::with_capacity(argc.min(256));
    for _ in 0..argc {
        if p >= buf.len() {
            break;
        }
        let start = p;
        while p < buf.len() && buf[p] != 0 {
            p += 1;
        }
        args.push(String::from_utf8_lossy(&buf[start..p]).into_owned());
        p += 1; // skip the NUL
    }
    (!args.is_empty()).then_some(args)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_command(_pid: u32) -> Option<Vec<String>> {
    None
}

/// The regular files a process holds open, used to resolve an agent's live session file
/// (e.g. the Codex rollout `…/rollout-<ts>-<uuid>.jsonl` an interactive TUI keeps open).
/// Linux reads the `/proc/<pid>/fd` symlinks; macOS shells out to `lsof -p <pid> -Fn`. Empty
/// on failure (the caller then falls back to a fresh restart, never a wrong session).
#[cfg(target_os = "linux")]
pub fn open_files(pid: u32) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
        for entry in rd.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path()) {
                out.push(target);
            }
        }
    }
    out
}

#[cfg(target_os = "macos")]
pub fn open_files(pid: u32) -> Vec<PathBuf> {
    // `lsof -Fn` emits one field per line prefixed by its type letter; `n` = the file name.
    let Ok(output) = Command::new("lsof")
        .args(["-p", &pid.to_string(), "-Fn"])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .map(PathBuf::from)
        .collect()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn open_files(_pid: u32) -> Vec<PathBuf> {
    Vec::new()
}

/// What a pane is running, for styling the sidebar/popup row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A known AI coding agent (claude, codex, …).
    Agent,
    /// An interactive shell (zsh, bash, …).
    Shell,
    /// Anything else (nvim, cargo, top, …).
    Other,
}

/// A pane's foreground command + its classification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub text: String,
    pub kind: Kind,
    /// The pid of the resolved foreground process (the agent process for an agent
    /// pane) — used to read its `~/.claude/sessions/<pid>.json` status.
    pub pid: u32,
}

/// Case-insensitive basenames treated as AI agents.
const AGENTS: &[&str] = &[
    "claude", "codex", "aider", "cursor", "gemini", "opencode", "droid", "copilot", "qwen", "crush",
];

/// The built-in AI-agent basenames — the default whitelist for restoring running
/// programs on session restore (config `restore_processes` overrides/extends it).
pub fn agent_basenames() -> &'static [&'static str] {
    AGENTS
}
/// Interactive shells (also matched with a leading `-` for login shells).
const SHELLS: &[&str] = &["zsh", "bash", "fish", "sh", "nu", "dash", "tcsh", "ksh"];

/// Normalize a `comm` to a basename without a leading `-` (login shells) or path.
fn basename(comm: &str) -> String {
    let b = comm.trim().rsplit('/').next().unwrap_or(comm).trim();
    b.strip_prefix('-').unwrap_or(b).to_string()
}

/// Classify a command basename.
pub fn classify(comm: &str) -> Kind {
    let c = basename(comm).to_ascii_lowercase();
    if AGENTS.iter().any(|a| c == *a) {
        Kind::Agent
    } else if SHELLS.iter().any(|s| c == *s) {
        Kind::Shell
    } else {
        Kind::Other
    }
}

/// One process row.
struct ProcRec {
    ppid: u32,
    pgid: u32,
    comm: String,
}

/// A snapshot of the process tree (`pid -> {ppid, pgid, comm-basename}`).
pub struct ProcTree {
    procs: HashMap<u32, ProcRec>,
}

/// Restore a resolved label's text to the process's own `argv[0]`.
///
/// macOS only, and purely to preserve what the label ALREADY said: `ps -o comm=`
/// printed argv[0], but the libproc sweep reports the EXEC name, so a command
/// reached through a shim symlink would silently relabel — with coreutils' gnubin
/// early on `$PATH`, `sleep` renders as `gsleep` and `tail` as `gtail`. Linux needs
/// none of this: its sweep reads the same `/proc` `comm` that `ps` did.
///
/// Costs one `KERN_PROCARGS2` sysctl per PANE (the single resolved foreground pid),
/// not per process, so it does not scale with the machine's process count. Leaves
/// the sweep's name in place when argv is unreadable (process already gone).
#[cfg(target_os = "macos")]
pub fn refine_label(label: &mut Label) {
    let Some(argv0) = process_command(label.pid).and_then(|a| a.into_iter().next()) else {
        return;
    };
    let name = basename(&argv0);
    if name.is_empty() {
        return;
    }
    // Reclassify: the old behaviour classified argv[0], so an agent launched through
    // a wrapper keeps being detected exactly as it was before the sweep changed.
    label.kind = classify(&name);
    label.text = name;
}

#[cfg(not(target_os = "macos"))]
pub fn refine_label(_label: &mut Label) {}

/// Read the whole process table. `None` only when the table could not be read at
/// all; a single process that exits mid-sweep is skipped, never fatal.
///
/// Zombies are dropped on the in-process paths: a pane's just-exited child would
/// otherwise win [`ProcTree::foreground`]'s highest-pid descent and label the pane
/// with a command that already finished.
#[cfg(target_os = "linux")]
fn sweep() -> Option<HashMap<u32, ProcRec>> {
    let mut procs = HashMap::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue; // /proc also holds non-pid entries (self, meminfo, …)
        };
        // A process can exit between readdir and read — skip its row, not the sweep.
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if let Some(rec) = parse_proc_stat(&stat) {
            procs.insert(pid, rec);
        }
    }
    Some(procs)
}

/// Parse `/proc/<pid>/stat`: `pid (comm) state ppid pgrp …`. `comm` is unquoted and
/// may itself contain spaces AND parens (`(sd-pam)`, a process renamed to `a (b)`),
/// so it is delimited by the FIRST `(` and the LAST `)` — splitting on whitespace
/// would mis-align every field after it. `None` for a zombie or an unparsable row.
#[cfg(target_os = "linux")]
fn parse_proc_stat(stat: &str) -> Option<ProcRec> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?;
    let mut fields = stat.get(close + 1..)?.split_whitespace();
    if fields.next()? == "Z" {
        return None;
    }
    let ppid = fields.next()?.parse().ok()?;
    let pgid = fields.next()?.parse().ok()?;
    Some(ProcRec {
        ppid,
        pgid,
        comm: basename(comm),
    })
}

/// macOS has no readable `/proc`, so sweep via libproc: one call for the pid list,
/// then one `PROC_PIDTBSDINFO` per pid. That is ~1600 cheap syscalls on a busy
/// machine — still far less work than forking `ps` and parsing its output, and
/// unlike a fork it cannot fail under process pressure.
#[cfg(target_os = "macos")]
fn sweep() -> Option<HashMap<u32, ProcRec>> {
    let pids = list_all_pids()?;
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let mut procs = HashMap::with_capacity(pids.len());
    for pid in pids {
        if pid <= 0 {
            continue;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a zeroed, correctly-sized out-param for this flavor.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        // A short read means the process exited mid-sweep or belongs to another
        // user (EPERM). Skip the row — our own pane shells are always readable.
        if read != size || info.pbi_status == libc::SZOMB {
            continue;
        }
        // `pbi_name` is the 32-char accounting name, `pbi_comm` the 16-char `comm`;
        // prefer the longer one so a long binary name survives. Both are the EXEC
        // name, where `ps -o comm=` printed argv[0] — so a process that rewrites its
        // own argv (`npm exec …`) or is reached through a symlink (`sh`→`bash`) now
        // labels as what actually runs. Agent detection is unaffected: `claude` and
        // `codex` are real executables, verified against a live `ps` sweep.
        let Some(comm) = c_str_field(&info.pbi_name).or_else(|| c_str_field(&info.pbi_comm)) else {
            continue;
        };
        procs.insert(
            pid as u32,
            ProcRec {
                ppid: info.pbi_ppid,
                pgid: info.pbi_pgid,
                comm: basename(&comm),
            },
        );
    }
    Some(procs)
}

/// Every pid on the system, via libproc.
///
/// `proc_listallpids` TRUNCATES to the buffer it is given and reports only how many
/// pids it wrote, so a completely full buffer is indistinguishable from an exact fit
/// — and a silently short table can omit a pane's own shell. So size it, over-allocate
/// a slack margin (the table can grow between the two calls), and treat a full buffer
/// as truncation worth retrying with double the room. If it is STILL full after the
/// last attempt, report failure: a `None` sweep retains the previous labels, which is
/// always better than a table that is quietly missing processes.
#[cfg(target_os = "macos")]
fn list_all_pids() -> Option<Vec<i32>> {
    // SAFETY: the null/0 form is libproc's documented "how big?" query.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return None;
    }
    let mut cap = count as usize + 128;
    for _ in 0..4 {
        let mut pids = vec![0i32; cap];
        let bytes = (cap * std::mem::size_of::<i32>()) as libc::c_int;
        // SAFETY: `pids` owns `bytes` writable bytes; libproc fills at most that many.
        let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if got <= 0 {
            return None;
        }
        let got = got as usize;
        if got < cap {
            pids.truncate(got);
            return Some(pids);
        }
        cap = cap.saturating_mul(2);
    }
    None
}

/// Read a fixed-size C char array as a `String` up to its NUL. `None` when empty.
#[cfg(target_os = "macos")]
fn c_str_field(buf: &[libc::c_char]) -> Option<String> {
    // SAFETY: `buf` is a live, contiguous C char array from libproc.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len()) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    (end > 0).then(|| String::from_utf8_lossy(&bytes[..end]).into_owned())
}

/// Fallback for unixes with neither `/proc` nor libproc: one `ps` fork.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sweep() -> Option<HashMap<u32, ProcRec>> {
    let out = Command::new("ps")
        .args(["-eo", "pid=,ppid=,pgid=,comm="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_ps(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `ps -eo pid=,ppid=,pgid=,comm=`. Columns are space-PADDED, so peel one
/// whitespace-delimited field at a time (splitting on every space would yield
/// empty fields and drop the row). `comm` (the tail) may hold a path.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn parse_ps(text: &str) -> HashMap<u32, ProcRec> {
    let mut procs = HashMap::new();
    for line in text.lines() {
        let Some((pid, rest)) = line.trim_start().split_once(char::is_whitespace) else {
            continue;
        };
        let Some((ppid, rest)) = rest.trim_start().split_once(char::is_whitespace) else {
            continue;
        };
        let Some((pgid, comm)) = rest.trim_start().split_once(char::is_whitespace) else {
            continue;
        };
        let (Ok(pid), Ok(ppid), Ok(pgid)) = (
            pid.parse::<u32>(),
            ppid.trim().parse::<u32>(),
            pgid.trim().parse::<u32>(),
        ) else {
            continue;
        };
        procs.insert(
            pid,
            ProcRec {
                ppid,
                pgid,
                comm: basename(comm),
            },
        );
    }
    procs
}

impl ProcTree {
    /// One sweep of the process table.
    ///
    /// `None` means the SWEEP ITSELF failed — never "nothing is running". The
    /// distinction is the whole point of the `Option`: a caller that reads a
    /// failed sweep as an empty machine unlabels every pane at once, which is
    /// exactly the bug where a mux's tab names and agent list blinked out
    /// together (see `docs/troubleshooting.md`). On `None` the caller must keep
    /// its last-known labels.
    ///
    /// An empty table is treated as failure too: the sweeping process is always
    /// in its own process table, so zero rows can only mean a truncated read.
    pub fn snapshot() -> Option<Self> {
        let procs = sweep()?;
        (!procs.is_empty()).then_some(Self { procs })
    }

    /// The classified label of the terminal's foreground PROCESS GROUP (from
    /// `tcgetpgrp`). The pgid is NOT necessarily a live pid — in a pipeline like
    /// `true | sleep 300` the group leader (`true`) can exit while `sleep` runs —
    /// so resolve a LIVE member of the group: the leader if it's alive, else the
    /// most-recently-started (highest-pid) member. `None` if the group is empty.
    pub fn command_of_pgroup(&self, pgid: u32) -> Option<Label> {
        let mut members: Vec<u32> = self
            .procs
            .iter()
            .filter(|(_, r)| r.pgid == pgid)
            .map(|(pid, _)| *pid)
            .collect();
        if members.is_empty() {
            return None;
        }
        let pick = if members.contains(&pgid) {
            pgid // the group leader is alive
        } else {
            members.sort_unstable();
            *members.last().unwrap() // last-started live member
        };
        self.procs.get(&pick).map(|r| Label {
            kind: classify(&r.comm),
            text: r.comm.clone(),
            pid: pick,
        })
    }

    /// Fallback foreground heuristic when the terminal PGID isn't resolvable:
    /// descend from `shell_pid` to the deepest descendant (highest-pid child at
    /// each level ≈ most recently spawned). `None` if the pid is unknown.
    pub fn foreground(&self, shell_pid: u32) -> Option<Label> {
        let mut cur = shell_pid;
        let mut comm = self.procs.get(&cur).map(|r| r.comm.clone())?;
        for _ in 0..64 {
            let child = self
                .procs
                .iter()
                .filter(|(_, r)| r.ppid == cur)
                .map(|(pid, _)| *pid)
                .max();
            match child {
                Some(c) => {
                    cur = c;
                    if let Some(r) = self.procs.get(&c) {
                        comm = r.comm.clone();
                    }
                }
                None => break,
            }
        }
        Some(Label {
            kind: classify(&comm),
            text: comm,
            pid: cur,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_agents_shells_and_others() {
        assert_eq!(classify("claude"), Kind::Agent);
        assert_eq!(classify("/opt/homebrew/bin/codex"), Kind::Agent);
        assert_eq!(classify("-zsh"), Kind::Shell);
        assert_eq!(classify("bash"), Kind::Shell);
        assert_eq!(classify("nvim"), Kind::Other);
        assert_eq!(classify("sleep"), Kind::Other);
    }

    fn rec(ppid: u32, pgid: u32, comm: &str) -> ProcRec {
        ProcRec {
            ppid,
            pgid,
            comm: comm.to_string(),
        }
    }

    #[test]
    fn foreground_descends_to_deepest_child() {
        let mut procs = HashMap::new();
        procs.insert(100, rec(1, 100, "zsh"));
        procs.insert(200, rec(100, 200, "claude"));
        procs.insert(300, rec(200, 200, "node"));
        let tree = ProcTree { procs };
        // 100 → 200 → 300 : deepest is node
        assert_eq!(tree.foreground(100).unwrap().text, "node");
        // unknown pid → None
        assert!(tree.foreground(999).is_none());
    }

    #[test]
    fn snapshot_reads_the_live_process_table() {
        // The sweep must SUCCEED and must contain the sweeping process itself —
        // the guarantee `refresh_labels` relies on to tell "sweep broke" (keep the
        // old labels) apart from "this pane really has nothing running".
        let tree = ProcTree::snapshot().expect("a live process sweep must succeed");
        let me = std::process::id();
        assert!(
            tree.procs.contains_key(&me),
            "sweep of {} processes is missing our own pid {me}",
            tree.procs.len()
        );
        assert!(!tree.foreground(me).unwrap().text.is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_stat_parses_comm_containing_spaces_and_parens() {
        // `comm` is unquoted: it can hold spaces and parens, so only the FIRST `(`
        // and LAST `)` delimit it. Anything else mis-aligns ppid/pgid.
        let rec = parse_proc_stat("42 (my (odd) name) S 7 9 0 0 -1 4194304 100").unwrap();
        assert_eq!(rec.comm, "my (odd) name");
        assert_eq!(rec.ppid, 7);
        assert_eq!(rec.pgid, 9);
        // A login-shell/path comm still reduces to a basename.
        assert_eq!(parse_proc_stat("1 (zsh) S 0 1 0").unwrap().comm, "zsh");
        // Zombies are dropped — they would win `foreground`'s highest-pid descent.
        assert!(parse_proc_stat("42 (ps) Z 7 9 0").is_none());
        assert!(parse_proc_stat("garbage").is_none());
    }

    #[test]
    fn pgroup_resolves_to_a_live_member_when_leader_is_dead() {
        // Pipeline `true | sleep 300`: the group leader (`true`, pid==pgid==500)
        // has exited; only `sleep` (pid 501, pgid 500) survives. The foreground
        // label must be `sleep`, not a fallback to the shell.
        let mut procs = HashMap::new();
        procs.insert(400, rec(1, 400, "zsh"));
        procs.insert(501, rec(400, 500, "sleep")); // leader 500 gone
        let tree = ProcTree { procs };
        let label = tree.command_of_pgroup(500).unwrap();
        assert_eq!(label.text, "sleep");
        assert_eq!(label.kind, Kind::Other);
        // With the leader alive, its own comm is used.
        let mut procs = HashMap::new();
        procs.insert(600, rec(400, 600, "claude"));
        procs.insert(601, rec(600, 600, "node"));
        let tree = ProcTree { procs };
        assert_eq!(tree.command_of_pgroup(600).unwrap().text, "claude");
        // Empty group → None.
        assert!(tree.command_of_pgroup(9999).is_none());
    }
}
