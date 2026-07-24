//! Foreground-process + agent classification for pane labels.
//!
//! One `ps` sweep builds a pid→(ppid, comm) tree; from a pane's shell pid we
//! descend to the deepest descendant (the foreground-ish process) and classify
//! its command name as an AI agent, a shell, or something else. Cheap enough to
//! run on a throttled cadence (~2 Hz), never per frame.

use std::collections::HashMap;
use std::path::PathBuf;
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

impl ProcTree {
    /// One `ps` sweep of all processes. Empty on failure (labels then fall back).
    pub fn snapshot() -> Self {
        let mut procs = HashMap::new();
        if let Ok(out) = Command::new("ps")
            .args(["-eo", "pid=,ppid=,pgid=,comm="])
            .output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                // `pid ppid pgid comm...` — columns are space-PADDED, so peel one
                // whitespace-delimited field at a time (splitting on every space
                // would yield empty fields and drop the row). comm (the tail) may
                // hold a path; `basename` handles it.
                let line = line.trim_start();
                let Some((pid, rest)) = line.split_once(char::is_whitespace) else {
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
        }
        Self { procs }
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
