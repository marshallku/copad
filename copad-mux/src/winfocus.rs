//! Best-effort **window raising** for the notification jump path: bring the terminal
//! emulator hosting an attached comux client to the front, so clicking an agent toast
//! lands you looking at the pane instead of merely switching it behind another window.
//!
//! Scope, deliberately narrow: this activates an *application*, not a window or a tab.
//! A terminal with several windows (or native tabs) sharing one pid will surface
//! whichever one the WM/compositor considers current — pid is the only identity a
//! client hands us. Documented as best-effort rather than papered over.
//!
//! Who runs this matters. The raise executes in the short-lived `comux jump` process
//! (the one the notifier spawns on click), NOT in the server: the server scrubs
//! `DISPLAY`/`DBUS_SESSION_BUS_ADDRESS`/… out of its own environment at startup (tmux
//! `update-environment`), so a GUI command spawned there would have no session to talk
//! to. The caller passes the environment explicitly ([`raise`] takes an `env` overlay)
//! because even the click-spawned process is a child of whatever spawned the notifier.

use std::process::{Command, Stdio};

/// Terminal-emulator process basenames worth activating. Mirrors the list the retired
/// `~/.claude/scripts/notify-attention.sh` walked, plus copad's own binaries.
const TERMINALS: &[&str] = &[
    "kitty",
    "ghostty",
    "alacritty",
    "foot",
    "wezterm-gui",
    "wezterm",
    "st",
    "urxvt",
    "rxvt",
    "xterm",
    "konsole",
    "terminator",
    "tilix",
    "copad",
    "sakura",
    "hyper",
    "tilda",
    "guake",
    "gnome-terminal-server",
    "gnome-terminal",
    "io.elementary.terminal",
    "qterminal",
    "cool-retro-term",
    "Terminal",
    "iTerm2",
    "WezTerm",
    "Ghostty",
    "Alacritty",
    "kitty-wrapper",
];

/// How far up the process tree to look for a terminal emulator. A client is normally
/// 1–3 hops from its terminal (shell → comux, sometimes a `tmux`/`ssh` hop between).
const MAX_DEPTH: usize = 12;

/// Is `comm` a known terminal emulator? Case-insensitive: macOS reports `Ghostty`
/// where Linux reports `ghostty`.
pub fn is_terminal(comm: &str) -> bool {
    TERMINALS.iter().any(|t| t.eq_ignore_ascii_case(comm))
}

/// Walk up from `pid` to the first ancestor that is a known terminal emulator,
/// returning `(pid, comm)`. `lookup` maps a pid to its `(ppid, comm)` — injected so the
/// walk is unit-testable without a live process table.
///
/// The starting process is examined too: a client launched directly by the emulator
/// (no shell in between) would otherwise be missed on the first hop.
pub fn terminal_ancestor(
    pid: u32,
    lookup: impl Fn(u32) -> Option<(u32, String)>,
) -> Option<(u32, String)> {
    let mut cur = pid;
    for _ in 0..MAX_DEPTH {
        let (ppid, comm) = lookup(cur)?;
        if is_terminal(&comm) {
            return Some((cur, comm));
        }
        // pid 1 / 0 terminate the walk — and a self-parenting record would otherwise spin.
        if ppid == 0 || ppid == 1 || ppid == cur {
            return None;
        }
        cur = ppid;
    }
    None
}

/// The same walk against the live process table.
pub fn terminal_ancestor_live(pid: u32) -> Option<(u32, String)> {
    let tree = crate::procinfo::ProcTree::snapshot()?;
    terminal_ancestor(pid, |p| tree.parent_of(p))
}

/// Spawn `cmd` detached, reaping it on a short-lived thread. Returns whether it spawned.
fn spawn_reaped(mut cmd: Command) -> bool {
    match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(_) => false,
    }
}

/// Raise the application owning `pid`, best-effort. `env` is overlaid on the child's
/// environment (the session vars a scrubbed server can't supply — `DISPLAY`,
/// `XAUTHORITY`, `DBUS_SESSION_BUS_ADDRESS`, `HYPRLAND_INSTANCE_SIGNATURE`, …).
///
/// Returns whether a raise command was launched — not whether the window actually came
/// forward, which no exit code reliably reports.
pub fn raise(pid: u32, env: &[(String, String)]) -> bool {
    #[cfg(target_os = "macos")]
    {
        raise_macos(pid, env)
    }
    #[cfg(target_os = "linux")]
    {
        raise_linux(pid, env)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (pid, env);
        false
    }
}

/// macOS: `System Events` addresses a GUI app by unix id, so this works for any terminal
/// without a bundle-id table. Needs Automation (TCC) permission for whichever app macOS
/// holds responsible for this process — the first click may prompt.
#[cfg(target_os = "macos")]
fn raise_macos(pid: u32, env: &[(String, String)]) -> bool {
    let script = format!(
        "tell application \"System Events\" to set frontmost of (first process whose unix id is {pid}) to true"
    );
    let mut os = Command::new("osascript");
    os.args(["-e", &script]);
    for (k, v) in env {
        os.env(k, v);
    }
    spawn_reaped(os)
}

/// Linux: Hyprland's `focuswindow pid:N` when we're on it, else resolve the pid's window
/// through `wmctrl`.
#[cfg(target_os = "linux")]
fn raise_linux(pid: u32, env: &[(String, String)]) -> bool {
    let hypr = env.iter().any(|(k, _)| k == "HYPRLAND_INSTANCE_SIGNATURE")
        || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some();
    if hypr {
        let mut h = Command::new("hyprctl");
        h.args(["dispatch", "focuswindow", &format!("pid:{pid}")]);
        for (k, v) in env {
            h.env(k, v);
        }
        if spawn_reaped(h) {
            return true;
        }
    }
    // X11 / wlroots-with-wmctrl: `wmctrl -lp` prints `<winid> <desktop> <pid> <host> <title>`.
    let mut list = Command::new("wmctrl");
    list.arg("-lp");
    for (k, v) in env {
        list.env(k, v);
    }
    let Ok(out) = list.stderr(Stdio::null()).output() else {
        return false;
    };
    let Some(winid) = window_id_for_pid(&String::from_utf8_lossy(&out.stdout), pid) else {
        return false;
    };
    let mut act = Command::new("wmctrl");
    act.args(["-ia", &winid]);
    for (k, v) in env {
        act.env(k, v);
    }
    spawn_reaped(act)
}

/// The first `wmctrl -lp` window id owned by `pid`. Split out as a pure function so the
/// parse is testable on both platforms (the column layout is fixed and documented).
pub fn window_id_for_pid(listing: &str, pid: u32) -> Option<String> {
    listing.lines().find_map(|line| {
        let mut f = line.split_whitespace();
        let win = f.next()?;
        let _desktop = f.next()?;
        let owner: u32 = f.next()?.parse().ok()?;
        (owner == pid).then(|| win.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn tree(pairs: &[(u32, u32, &str)]) -> HashMap<u32, (u32, String)> {
        pairs
            .iter()
            .map(|(pid, ppid, comm)| (*pid, (*ppid, comm.to_string())))
            .collect()
    }

    #[test]
    fn walks_up_to_the_emulator() {
        // comux client → zsh → ghostty → launchd
        let t = tree(&[(400, 300, "comux"), (300, 200, "zsh"), (200, 1, "ghostty")]);
        let got = terminal_ancestor(400, |p| t.get(&p).cloned());
        assert_eq!(got, Some((200, "ghostty".to_string())));
    }

    #[test]
    fn starting_process_can_itself_be_the_terminal() {
        let t = tree(&[(200, 1, "Ghostty")]);
        assert_eq!(
            terminal_ancestor(200, |p| t.get(&p).cloned()),
            Some((200, "Ghostty".to_string()))
        );
    }

    #[test]
    fn no_terminal_in_the_chain_is_none() {
        // A server started by systemd/launchd has no emulator above it.
        let t = tree(&[(400, 300, "comux"), (300, 1, "systemd")]);
        assert_eq!(terminal_ancestor(400, |p| t.get(&p).cloned()), None);
    }

    /// A self-parenting or cyclic record must not spin the walk (defensive: the table is
    /// sampled, so a pid can be recycled between reads).
    #[test]
    fn cycles_terminate() {
        let t = tree(&[(400, 400, "comux")]);
        assert_eq!(terminal_ancestor(400, |p| t.get(&p).cloned()), None);
        let cyc = tree(&[(1, 2, "a"), (2, 1, "b")]);
        assert_eq!(terminal_ancestor(1, |p| cyc.get(&p).cloned()), None);
    }

    #[test]
    fn unknown_pid_is_none() {
        let t = tree(&[]);
        assert_eq!(terminal_ancestor(9999, |p| t.get(&p).cloned()), None);
    }

    #[test]
    fn terminal_match_is_case_insensitive() {
        assert!(is_terminal("ghostty"));
        assert!(is_terminal("Ghostty"));
        assert!(is_terminal("WEZTERM"));
        assert!(!is_terminal("claude"));
        assert!(!is_terminal(""));
    }

    #[test]
    fn wmctrl_listing_picks_the_owning_window() {
        let listing = "0x01 0 111 host one\n0x02 1 222 host two\n0x03 0 222 host three\n";
        assert_eq!(window_id_for_pid(listing, 222).as_deref(), Some("0x02"));
        assert_eq!(window_id_for_pid(listing, 999), None);
        assert_eq!(window_id_for_pid("", 1), None);
        // A malformed row must not panic or match.
        assert_eq!(
            window_id_for_pid("garbage\n0x04 0 7 h t\n", 7).as_deref(),
            Some("0x04")
        );
    }
}
