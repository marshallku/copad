//! Foreground-process + agent classification for pane labels.
//!
//! From a pane's shell pid we descend to the deepest descendant (the foreground-ish process)
//! and classify its command name as an AI agent, a shell, or something else. Runs on a
//! throttled cadence (~2 Hz), never per frame.
//!
//! The sweep is IN-PROCESS (`/proc` on Linux, libproc on macOS), not a `ps` fork: a mux server
//! lives for weeks, and each fork was a chance to blank the whole label map (see
//! [`ProcTree::snapshot`]).

use std::collections::HashMap;
use std::path::PathBuf;
// macOS shells out to `lsof` in `open_files`; the non-Linux/macOS fallback sweep
// shells out to `ps`. Linux reads /proc for both, so it needs no child processes.
#[cfg(not(target_os = "linux"))]
use std::process::Command;

/// The current working directory of a process (Linux `/proc/<pid>/cwd`, macOS libproc
/// `PROC_PIDVNODEPATHINFO`).
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

/// When `pid` started (macOS `PROC_PIDTBSDINFO`, Linux `/proc/<pid>/stat` field 22 + the
/// boot time). Also serves as a pid's incarnation stamp: a recycled pid has a different one.
#[cfg(target_os = "macos")]
pub fn process_start_time(pid: u32) -> Option<std::time::SystemTime> {
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a zeroed, correctly-sized out-param for this flavor.
    let read = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if read != size {
        return None;
    }
    Some(
        std::time::UNIX_EPOCH
            + std::time::Duration::new(info.pbi_start_tvsec, info.pbi_start_tvusec as u32 * 1000),
    )
}

#[cfg(target_os = "linux")]
pub fn process_start_time(pid: u32) -> Option<std::time::SystemTime> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `comm` (field 2) is parenthesised and may contain spaces, so count from the LAST `)`:
    // field 3 is the first one after it, which puts `starttime` (22) at index 19.
    let ticks: u64 = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    let boot: u64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    let since_boot = std::time::Duration::from_millis(ticks * 1000 / hz as u64);
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(boot) + since_boot)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_start_time(_pid: u32) -> Option<std::time::SystemTime> {
    None
}

/// The argv of `pid`, structural (NOT space-joined) so argument boundaries survive restore:
/// `claude "a; b"` stays ONE arg, never re-split into two shell commands. `None` if the process
/// is gone or has no readable argv.
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
    procargs(pid)
        .map(|(args, _)| args)
        .filter(|a| !a.is_empty())
}

/// One variable from the environment `pid` was STARTED with (Linux `/proc/<pid>/environ`,
/// macOS the tail of `KERN_PROCARGS2`). `None` when unset, empty or unreadable. An agent
/// launched as `GROK_HOME=/x grok` keeps its data where THAT says.
#[cfg(target_os = "linux")]
pub fn process_env(pid: u32, name: &str) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    env_lookup(
        raw.split(|&b| b == 0)
            .map(|e| String::from_utf8_lossy(e).into_owned()),
        name,
    )
}

#[cfg(target_os = "macos")]
pub fn process_env(pid: u32, name: &str) -> Option<String> {
    env_lookup(procargs(pid)?.1.into_iter(), name)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_env(_pid: u32, _name: &str) -> Option<String> {
    None
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn env_lookup(env: impl Iterator<Item = String>, name: &str) -> Option<String> {
    env.filter_map(|e| Some(e.strip_prefix(name)?.strip_prefix('=')?.to_string()))
        .next()
        .filter(|v| !v.is_empty())
}

#[cfg(target_os = "macos")]
fn procargs(pid: u32) -> Option<(Vec<String>, Vec<String>)> {
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
    while p < buf.len() && buf[p] != 0 {
        p += 1;
    }
    while p < buf.len() && buf[p] == 0 {
        p += 1;
    }
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
        p += 1;
    }
    let env = buf[p.min(buf.len())..]
        .split(|&b| b == 0)
        .take_while(|e| !e.is_empty())
        .map(|e| String::from_utf8_lossy(e).into_owned())
        .collect();
    Some((args, env))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_command(_pid: u32) -> Option<Vec<String>> {
    None
}

/// The regular files a process holds open, used to resolve an agent's live session file (e.g.
/// the Codex rollout an interactive TUI keeps open). Linux reads `/proc/<pid>/fd`; macOS shells
/// out to `lsof -p <pid> -Fn`.
///
/// `None` means the open files COULD NOT BE ENUMERATED; `Some(v)` means they were, and `v` may
/// be empty. A caller that caches a source must tell a transient failure from an agent that has
/// closed its log (decision #88 draws the same line for [`ProcTree::snapshot`]).
#[cfg(target_os = "linux")]
pub fn open_files(pid: u32) -> Option<Vec<PathBuf>> {
    let rd = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    let mut out = Vec::new();
    for entry in rd {
        // An error mid-walk is an enumeration we did not finish, so the list we would return
        // is INCOMPLETE — and an incomplete `Some` reads as "we looked, it is not there".
        let entry = entry.ok()?;
        match std::fs::read_link(entry.path()) {
            Ok(target) => out.push(target),
            // The descriptor was closed while we walked: genuinely gone.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // Anything else (EACCES on a process we do not own) is us being unable to look.
            Err(_) => return None,
        }
    }
    Some(out)
}

#[cfg(target_os = "macos")]
pub fn open_files(pid: u32) -> Option<Vec<PathBuf>> {
    // `lsof -Fn` emits one field per line prefixed by its type letter; `n` = the file name.
    let output = Command::new("lsof")
        .args(["-p", &pid.to_string(), "-Fn"])
        .output()
        .ok()?;
    // A nonzero exit is a FAILED enumeration, not an empty one: `lsof` exits 1 both when the
    // pid is gone and when it could not read the process, and we cannot tell those apart.
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|l| l.strip_prefix('n'))
            .map(PathBuf::from)
            .collect(),
    )
}

/// `None`, not `Some(vec![])`: on a platform with no enumeration we genuinely cannot see, and
/// claiming "this process holds nothing open" would make every caller drop a working source.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn open_files(_pid: u32) -> Option<Vec<PathBuf>> {
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Agent,
    Shell,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    pub text: String,
    pub kind: Kind,
    pub pid: u32,
}

const AGENTS: &[&str] = &[
    "claude", "codex", "agy", "grok", "aider", "cursor", "gemini", "opencode", "droid", "copilot",
    "qwen", "crush",
];

/// The built-in AI-agent basenames: the default whitelist for restoring programs on session
/// restore (config `restore_processes` overrides/extends it).
pub fn agent_basenames() -> &'static [&'static str] {
    AGENTS
}
const SHELLS: &[&str] = &["zsh", "bash", "fish", "sh", "nu", "dash", "tcsh", "ksh"];

fn basename(comm: &str) -> String {
    let b = comm.trim().rsplit('/').next().unwrap_or(comm).trim();
    b.strip_prefix('-').unwrap_or(b).to_string()
}

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

struct ProcRec {
    ppid: u32,
    pgid: u32,
    comm: String,
}

pub struct ProcTree {
    procs: HashMap<u32, ProcRec>,
}

/// Restore a resolved label's text to the process's own `argv[0]` (macOS only).
///
/// `ps -o comm=` printed argv[0] but the libproc sweep reports the EXEC name, so a command
/// reached through a shim symlink would relabel: with coreutils' gnubin early on `$PATH`,
/// `sleep` renders as `gsleep`. Costs one `KERN_PROCARGS2` sysctl per PANE. Leaves the
/// sweep's name when argv is unreadable.
#[cfg(target_os = "macos")]
pub fn refine_label(label: &mut Label) {
    let Some(argv0) = process_command(label.pid).and_then(|a| a.into_iter().next()) else {
        return;
    };
    let name = basename(&argv0);
    if name.is_empty() {
        return;
    }
    // Reclassify, so an agent launched through a wrapper is still detected from argv[0].
    label.kind = classify(&name);
    label.text = name;
}

#[cfg(not(target_os = "macos"))]
pub fn refine_label(_label: &mut Label) {}

/// Read the whole process table. `None` only when the table could not be read at all; a
/// process that exits mid-sweep is skipped. Zombies are dropped: a pane's just-exited child
/// would otherwise win [`ProcTree::foreground`]'s highest-pid descent.
#[cfg(target_os = "linux")]
fn sweep() -> Option<HashMap<u32, ProcRec>> {
    let mut procs = HashMap::new();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if let Some(rec) = parse_proc_stat(&stat) {
            procs.insert(pid, rec);
        }
    }
    Some(procs)
}

/// Parse `/proc/<pid>/stat`: `pid (comm) state ppid pgrp ...`. `comm` may contain spaces AND
/// parens, so it is delimited by the FIRST `(` and LAST `)`. `None` for a zombie or an
/// unparsable row.
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

/// macOS has no readable `/proc`, so sweep via libproc: one call for the pid list, then one
/// `PROC_PIDTBSDINFO` per pid. Cheaper than forking `ps`, and cannot fail under process
/// pressure.
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
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        // A short read means the process exited mid-sweep or belongs to another user (EPERM).
        if read != size || info.pbi_status == libc::SZOMB {
            continue;
        }
        // Prefer the 32-char `pbi_name` over the 16-char `pbi_comm` so a long binary name
        // survives. Both are the EXEC name, where `ps -o comm=` printed argv[0].
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

/// Every pid on the system, via libproc. `proc_listallpids` TRUNCATES to the buffer it is given
/// and reports only how many it wrote, so a full buffer is indistinguishable from an exact fit
/// and a short table can omit a pane's own shell. Size it, over-allocate (the table can grow
/// between calls), and retry with double the room on a full buffer. If still full after the
/// last attempt, report failure: a `None` sweep retains the previous labels.
#[cfg(target_os = "macos")]
fn list_all_pids() -> Option<Vec<i32>> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return None;
    }
    let mut cap = count as usize + 128;
    for _ in 0..4 {
        let mut pids = vec![0i32; cap];
        let bytes = (cap * std::mem::size_of::<i32>()) as libc::c_int;
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

#[cfg(target_os = "macos")]
fn c_str_field(buf: &[libc::c_char]) -> Option<String> {
    // SAFETY: `buf` is a live, contiguous C char array from libproc.
    let bytes = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len()) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    (end > 0).then(|| String::from_utf8_lossy(&bytes[..end]).into_owned())
}

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
/// whitespace-delimited field at a time; `comm` (the tail) may hold a path.
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
    /// One sweep of the process table. `None` means the SWEEP ITSELF failed, never "nothing is
    /// running": a caller that reads it as an empty machine unlabels every pane at once (see
    /// `docs/troubleshooting.md`) and must keep its last-known labels. An empty table is
    /// failure too, since the sweeper is always in its own table.
    pub fn snapshot() -> Option<Self> {
        let procs = sweep()?;
        (!procs.is_empty()).then_some(Self { procs })
    }

    /// Every pid whose comm basename satisfies `pred`.
    pub fn pids_where(&self, pred: impl Fn(&str) -> bool) -> Vec<u32> {
        self.procs
            .iter()
            .filter(|(_, r)| pred(&r.comm))
            .map(|(&pid, _)| pid)
            .collect()
    }

    pub fn parent_of(&self, pid: u32) -> Option<(u32, String)> {
        self.procs.get(&pid).map(|r| (r.ppid, r.comm.clone()))
    }

    /// The classified label of the terminal's foreground PROCESS GROUP. The pgid is NOT
    /// necessarily a live pid (in `true | sleep 300` the leader exits while `sleep` runs), so
    /// resolve a LIVE member: the leader if alive, else the highest-pid member.
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

    #[test]
    #[ignore]
    fn env_probe_child() {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }

    /// The child is this test binary rather than `/bin/sleep`: macOS withholds the environment
    /// of platform binaries from `KERN_PROCARGS2`.
    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn process_env_reads_the_target_process_environment() {
        let exe = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(&exe)
            .args([
                "--exact",
                "procinfo::tests::env_probe_child",
                "--ignored",
                "--quiet",
            ])
            .env("GROK_HOME", "/tmp/comux-grok-home-probe")
            .env("COMUX_EMPTY_PROBE", "")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let mut got = None;
        for _ in 0..200 {
            got = process_env(pid, "GROK_HOME");
            if got.is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let empty = process_env(pid, "COMUX_EMPTY_PROBE");
        let prefix = process_env(pid, "GROK");
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(got.as_deref(), Some("/tmp/comux-grok-home-probe"));
        assert_eq!(empty, None, "an empty value counts as unset");
        assert_eq!(prefix, None, "a name must match whole, not as a prefix");
    }

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
        assert_eq!(tree.foreground(100).unwrap().text, "node");
        assert!(tree.foreground(999).is_none());
    }

    #[test]
    fn snapshot_reads_the_live_process_table() {
        // Must SUCCEED and contain the sweeping process itself: the guarantee `refresh_labels`
        // relies on to tell "sweep broke" from "this pane has nothing running".
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
        // `comm` is unquoted and can hold spaces and parens, so only the FIRST `(` and LAST `)`
        // delimit it.
        let rec = parse_proc_stat("42 (my (odd) name) S 7 9 0 0 -1 4194304 100").unwrap();
        assert_eq!(rec.comm, "my (odd) name");
        assert_eq!(rec.ppid, 7);
        assert_eq!(rec.pgid, 9);
        assert_eq!(parse_proc_stat("1 (zsh) S 0 1 0").unwrap().comm, "zsh");
        assert!(parse_proc_stat("42 (ps) Z 7 9 0").is_none());
        assert!(parse_proc_stat("garbage").is_none());
    }

    #[test]
    fn pgroup_resolves_to_a_live_member_when_leader_is_dead() {
        // Pipeline `true | sleep 300`: the group leader (pid 500) has exited and only `sleep`
        // (pid 501, pgid 500) survives. The label must be `sleep`, not the shell.
        let mut procs = HashMap::new();
        procs.insert(400, rec(1, 400, "zsh"));
        procs.insert(501, rec(400, 500, "sleep")); // leader 500 gone
        let tree = ProcTree { procs };
        let label = tree.command_of_pgroup(500).unwrap();
        assert_eq!(label.text, "sleep");
        assert_eq!(label.kind, Kind::Other);
        let mut procs = HashMap::new();
        procs.insert(600, rec(400, 600, "claude"));
        procs.insert(601, rec(600, 600, "node"));
        let tree = ProcTree { procs };
        assert_eq!(tree.command_of_pgroup(600).unwrap().text, "claude");
        assert!(tree.command_of_pgroup(9999).is_none());
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn enumeration_failure_is_not_reported_as_an_empty_set() {
        let mine = open_files(std::process::id());
        assert!(
            mine.as_ref().is_some_and(|v| !v.is_empty()),
            "this process holds files open; enumerating itself must succeed: {mine:?}"
        );
        assert_eq!(
            open_files(u32::MAX),
            None,
            "a pid that cannot be inspected must be None, never an empty list"
        );
    }
}
