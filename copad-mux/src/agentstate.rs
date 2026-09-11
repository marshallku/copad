//! Per-agent rolled-up status — a clean-room port of the algorithm in the owner's
//! `tmx` (`~/dev/tmx/src/agents/{session_meta,classify}.rs`), adapted to copad-mux's
//! in-process PTY (we already hold the agent pid + the pane's screen snapshot, so no
//! `tmux capture-pane` / cwd is needed).
//!
//! PRIMARY signal (Claude): read Claude's own status file
//! `~/.claude/sessions/<pid>.json` keyed by the agent process pid — `status`:
//! `busy`→Working, `idle`→Ready, `waiting`→Blocked. FALLBACK (Codex/custom, or no
//! file): scrape the last 30 screen lines for the same UI cues tmx matches.

use crate::term::Snapshot;

/// A rolled-up agent status for the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// Mid-turn: running tools / composing. No user input needed.
    Working,
    /// Parked at the chat prompt — the user can type a new request.
    Ready,
    /// Blocking on a permission / selection dialog until the user acts.
    Blocked,
    /// No recognised agent UI (plain shell view, editor, etc.).
    Idle,
}

impl AgentStatus {
    pub fn label(self) -> &'static str {
        match self {
            AgentStatus::Working => "working",
            AgentStatus::Ready => "ready",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Idle => "idle",
        }
    }
}

/// Resolve an agent pane's status: Claude session file first (accurate, pid-keyed),
/// else the screen-text fallback.
pub fn resolve(pid: Option<u32>, snap: &Snapshot) -> AgentStatus {
    if let Some(pid) = pid
        && let Some(s) = claude_session_status(pid)
    {
        return s;
    }
    screen_status(snap)
}

/// Path of Claude's per-process status file, `~/.claude/sessions/<pid>.json`.
fn claude_session_file(pid: u32) -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        std::path::Path::new(&home)
            .join(".claude")
            .join("sessions")
            .join(format!("{pid}.json")),
    )
}

/// Read `~/.claude/sessions/<pid>.json` → `status` field. `None` on any failure
/// (missing/unreadable/malformed/unknown status) so the caller falls back.
pub fn claude_session_status(pid: u32) -> Option<AgentStatus> {
    parse_session_status(&std::fs::read_to_string(claude_session_file(pid)?).ok()?)
}

/// Read Claude's live conversation id from `~/.claude/sessions/<pid>.json` → `sessionId`.
/// This is the AUTHORITATIVE current session for the process, even if it was launched with
/// `--resume <old-id>` / `--continue` (Claude may have forked to a new id) — so restore
/// resumes what's actually on screen, not the stale launch argument. `None` on any failure
/// or a value that isn't a UUID (reject rather than resume the wrong thing).
pub fn claude_session_id(pid: u32) -> Option<String> {
    let json = std::fs::read_to_string(claude_session_file(pid)?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let id = v.get("sessionId").and_then(|s| s.as_str())?;
    is_session_uuid(id).then(|| id.to_string())
}

/// A Claude/Codex session id is a UUID (`8-4-4-4-12` hex groups). Validate strictly so a
/// malformed/hostile value never lands on a restored command line.
pub fn is_session_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && [8usize, 4, 4, 4, 12]
            .iter()
            .zip(&groups)
            .all(|(&len, g)| g.len() == len && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Parse Claude's session JSON → status. `None` on malformed JSON or an unrecognized
/// status (reject rather than guess — tmx does the same); tolerates extra fields.
fn parse_session_status(json: &str) -> Option<AgentStatus> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    match v.get("status").and_then(|s| s.as_str())? {
        "busy" => Some(AgentStatus::Working),
        "idle" => Some(AgentStatus::Ready),
        "waiting" => Some(AgentStatus::Blocked),
        _ => None,
    }
}

/// Substrings that indicate a blocking permission / selection dialog (highest
/// priority — tmx `classify.rs`). Matched against the window VERBATIM, exactly as before:
/// these are short (`to navigate ·` is 13 characters), and the wrap-tolerant matching used
/// for [`WRAP_TOLERANT_MARKERS`] would let a short marker straddle two unrelated rows.
const DECISION_MARKERS: &[&str] = &[
    "Enter to select",
    "to navigate ·",
    "Do you want to proceed",
    "Allow this tool",
];

/// Codex's blocking dialogs. Captured one at a time from a live codex-cli 0.154.0 pane; the
/// last entry is the confirm footer codex renders under EVERY picker, so it also catches the
/// ones not enumerated above (model picker, usage-limit picker, …).
///
/// These are matched WRAP-TOLERANTLY (see [`squeeze`]) because they are full sentences that
/// a narrow pane hard-wraps. That is safe only because they are full sentences: a marker
/// this long cannot plausibly be assembled by accident out of two adjacent screen rows.
///
/// Every entry also has to be specific enough that it cannot show up in an agent's ordinary
/// OUTPUT — a false Blocked is not a silent mis-label, it raises a desktop toast and bumps
/// the status-bar `⚑N`. That is why codex's generic footer is carried as the long
/// `"Press enter to confirm or esc to"` rather than the far broader
/// `"Press enter to continue"`, which an agent quoting a pager footer or documentation would
/// trip while genuinely working.
const WRAP_TOLERANT_MARKERS: &[&str] = &[
    "Would you like to run the following command?",
    "Would you like to make the following edits?",
    "Would you like to grant these permissions?",
    "Would you like to send input to the existing terminal?",
    "Do you trust the contents of this directory?",
    "Press enter to confirm or esc to",
];

/// `WRAP_TOLERANT_MARKERS` + the running hint, pre-squeezed once rather than on every sweep.
/// `screen_status` runs per agent pane twice a second while a client is attached, and an
/// idle server is held to 0.17% of a core (decision #92), so the per-call work stays to the
/// single unavoidable pass over the window.
static SQUEEZED_MARKERS: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
    WRAP_TOLERANT_MARKERS
        .iter()
        .map(|m| squeeze(m.chars()))
        .collect()
});

/// Codex's "a turn is running" hint, pre-squeezed. It wraps in a narrow pane just as the
/// dialogs do (`esc to inte` / `rrupt)`), and it is distinctive enough to be safe squeezed.
static SQUEEZED_INTERRUPT: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| squeeze("esc to interrupt".chars()));

/// Composing/tool-running spinners (Claude/Codex).
const SPINNERS: &[char] = &['✱', '✻', '✷', '✸', '✹', '✺'];

/// Screen-text fallback: inspect the last 30 lines of the pane in tmx's strict
/// priority order — decision > interrupt > ready-prompt > spinner > idle.
pub fn screen_status(snap: &Snapshot) -> AgentStatus {
    let lines: Vec<String> = snap
        .cells
        .iter()
        .rev()
        .take(30)
        .map(|row| {
            row.iter()
                .map(|c| c.sym.as_str())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    let joined = lines.join("\n");
    // Markers are matched with ALL whitespace removed from both sides. A terminal hard-wraps
    // at the pane width, and it wraps MID-WORD: in a 27-column split pane codex's
    // `Would you like to run the following command?` arrives as `…run the f` / `ollowing
    // command?`, so a plain substring match silently fails — which is how a blocked agent in
    // a narrow pane went on reading `idle`. Rejoining with a space does not fix it either;
    // that just reassembles `the f ollowing`. Deleting whitespace entirely is insensitive to
    // WHERE the break fell, which is the only property we can rely on, and it lets the
    // markers stay long and specific — the thing that stops them firing on ordinary output.
    //
    // NOTE `lines` is bottom-up, so this must run in SCREEN order or the two halves of a
    //      wrapped phrase swap and match nothing.
    let flat = squeeze(lines.iter().rev().flat_map(|l| l.chars()));

    // 1) a decision dialog wins over everything.
    if DECISION_MARKERS.iter().any(|m| joined.contains(m))
        || SQUEEZED_MARKERS.iter().any(|m| flat.contains(m.as_str()))
    {
        return AgentStatus::Blocked;
    }
    // 2) explicit "running, esc to interrupt".
    if flat.contains(SQUEEZED_INTERRUPT.as_str()) {
        return AgentStatus::Working;
    }
    // 3) the BOTTOM-most composer cursor line decides readiness (checked before the
    //    spinner so decorative sparkles in a message don't override a genuine prompt).
    //
    //    Position, not presence. Both CLIs keep their composer at the bottom of the
    //    screen, and a dialog awaiting input sits below whatever preceded it — while an
    //    already-ANSWERED numbered list scrolls up and can linger inside the window above
    //    a live prompt. Asking `any()` therefore let a stale line vote, in both
    //    directions: it could call a codex approval prompt `ready` (codex keeps its
    //    composer rendered during a turn) and a Claude pane `idle` (an answered list
    //    sitting above the prompt). The last cursor line on screen is right in both cases.
    //
    //    `lines` is built bottom-up (`.rev().take(30)`), so the FIRST match here is the
    //    bottom-most screen row. Reversing that would silently pick the top-most cursor
    //    line and reintroduce the stale-line vote while still passing any single-line test.
    if let Some(line) = lines.iter().find(|l| cursor_rest(l).is_some())
        && is_ready_prompt(line)
    {
        return AgentStatus::Ready;
    }
    // 4) a spinner anywhere → still working.
    if joined.chars().any(|c| SPINNERS.contains(&c)) {
        return AgentStatus::Working;
    }
    // 5) nothing recognised.
    AgentStatus::Idle
}

/// Drop every whitespace character, so a phrase the terminal hard-wrapped across two rows
/// still matches. Applied to both the screen window and the marker before comparing.
fn squeeze(chars: impl Iterator<Item = char>) -> String {
    chars.filter(|c| !c.is_whitespace()).collect()
}

/// Composer cursor glyphs. Claude Code draws `❯` (U+276F); codex draws `›` (U+203A) — for
/// its prompt AND for the highlighted row of a selection dialog, exactly like Claude.
/// Matching only Claude's glyph is why a codex pane used to fall through every branch of
/// [`screen_status`] to `Idle`, both when parked at its composer and when blocked on an
/// approval — and `idle` is a status `comux wait-agent` refuses to wait on.
const CURSOR_GLYPHS: &[char] = &['❯', '›'];

/// If `line` is a cursor line, what follows the glyph (leading space trimmed); `None` when
/// the first non-space character is not a cursor glyph.
fn cursor_rest(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let mut chars = t.chars();
    CURSOR_GLYPHS
        .contains(&chars.next()?)
        .then(|| chars.as_str().trim_start())
}

/// A "free" composer prompt: a cursor line whose remainder is empty, whitespace, or free
/// text — but NOT a numbered selection cursor (`❯ 1.` / `› 1.`), which is a decision, not a
/// ready prompt (tmx `has_ready_prompt`).
fn is_ready_prompt(line: &str) -> bool {
    let Some(rest) = cursor_rest(line) else {
        return false;
    };
    if rest.is_empty() {
        return true; // bare cursor
    }
    // reject a numbered selection like `❯ 1.` / `› 12.`
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && rest[digits.len()..].starts_with('.') {
        return false;
    }
    true
}

// ===== session restore: resume the actual conversation, not just re-run the agent =====
//
// On session restore, comux re-runs a whitelisted agent's saved argv. Bare re-execution
// starts a FRESH conversation, losing the work in progress. These helpers rebuild the
// command so it RESUMES the live conversation instead: they resolve the agent's current
// session id (Claude: a pid-keyed status file; Codex: the rollout file the process holds
// open) and reconstruct a canonical resume command.
//
// Reconstruction is canonical (not a surgical argv edit) on purpose: without each flag's
// arity we can't reliably tell an option's value from a positional prompt, so re-running an
// edited argv risks replaying the initial prompt or carrying a conflicting selector. Instead
// we rebuild `claude --resume <id> <safe-flags…>` / `codex resume <id>` from scratch, keeping
// only an allowlist of arity-known, session-independent runtime flags.

/// Resolve an agent's live session id from its pid, dispatching on the command basename.
/// `None` (→ leave the argv untouched → fresh start) when the agent is unknown or its id
/// can't be resolved.
pub fn agent_session_id(comm: &str, pid: u32) -> Option<String> {
    match comm.to_ascii_lowercase().as_str() {
        "claude" => claude_session_id(pid),
        "codex" => codex_session_id(pid),
        _ => None,
    }
}

/// Rebuild a whitelisted agent's argv into a resume command for `session_id`. `comm` is the
/// program basename; `argv` is what it was launched with. Returns the argv unchanged when the
/// agent is unknown, the invocation shouldn't be resumed (one-shot / non-interactive), or
/// `session_id` isn't a UUID.
pub fn resume_argv(comm: &str, argv: &[String], session_id: &str) -> Vec<String> {
    if argv.is_empty() || !is_session_uuid(session_id) {
        return argv.to_vec();
    }
    match comm.to_ascii_lowercase().as_str() {
        "claude" => claude_resume_argv(argv, session_id),
        "codex" => codex_resume_argv(argv, session_id),
        _ => argv.to_vec(),
    }
}

/// Claude runtime flags safe to carry into a resumed session, with their arity. Session
/// SELECTORS (`--resume`/`-r`/`--continue`/`-c`/`--session-id`/`--fork-session`) are
/// deliberately absent — the injected `--resume <id>` is the single source of truth. So are
/// value flags whose arity is variadic/ambiguous (`--add-dir`, `--mcp-config`), which we drop
/// rather than risk mis-parsing.
const CLAUDE_BOOL_FLAGS: &[&str] = &[
    "--dangerously-skip-permissions",
    "--verbose",
    "--ide",
    "--safe-mode",
    "--bare",
];
const CLAUDE_VALUE_FLAGS: &[&str] = &["--model", "--permission-mode", "--settings", "--agent"];

/// `claude … → claude --resume <id> <carried-flags…>`. Skips (returns argv unchanged) for
/// non-interactive `-p`/`--print` and `--no-session-persistence` invocations, which have no
/// resumable interactive conversation.
fn claude_resume_argv(argv: &[String], session_id: &str) -> Vec<String> {
    if argv
        .iter()
        .any(|a| matches!(a.as_str(), "-p" | "--print" | "--no-session-persistence"))
    {
        return argv.to_vec();
    }
    let mut out = vec![
        argv[0].clone(),
        "--resume".to_string(),
        session_id.to_string(),
    ];
    out.extend(carry_flags(
        &argv[1..],
        CLAUDE_BOOL_FLAGS,
        CLAUDE_VALUE_FLAGS,
    ));
    out
}

/// `codex [flags] [prompt] → codex resume <id>`. Only rebuilt for the interactive TUI form;
/// an explicit subcommand (`codex exec`, `codex resume`, `codex review`, …) is left as-is so
/// we never wrap a non-interactive run or double a resume. Codex `resume` accepts a UUID
/// directly; flags/prompt are dropped (the conversation they seeded is already in the rollout).
fn codex_resume_argv(argv: &[String], session_id: &str) -> Vec<String> {
    // Skip if ANY token is a subcommand name, not just the first non-option one: a value-taking
    // global flag can put a non-option token AHEAD of the subcommand (`codex --cd /repo exec …`),
    // and we don't track per-flag arity. Over-approximating toward "skip" is the safe direction —
    // a missed resume just restarts fresh, whereas wrapping a real `exec`/`review` run would break
    // it. A bare prompt equal to a subcommand word (`codex review`) is ambiguous; skipping is safe.
    if argv[1..].iter().any(|a| is_codex_subcommand(a)) {
        return argv.to_vec();
    }
    vec![
        argv[0].clone(),
        "resume".to_string(),
        session_id.to_string(),
    ]
}

fn is_codex_subcommand(s: &str) -> bool {
    const SUBCOMMANDS: &[&str] = &[
        "exec",
        "e",
        "review",
        "login",
        "logout",
        "mcp",
        "plugin",
        "mcp-server",
        "app-server",
        "remote-control",
        "app",
        "completion",
        "update",
        "doctor",
        "sandbox",
        "debug",
        "apply",
        "a",
        "resume",
        "archive",
        "delete",
        "unarchive",
        "fork",
        "cloud",
        "exec-server",
        "features",
        "help",
    ];
    SUBCOMMANDS.contains(&s)
}

/// Filter an argv tail to an allowlist of boolean (arity-0) and value (arity-1) flags,
/// carrying each recognized flag (and, for value flags, its following argument) through and
/// dropping everything else (positional prompt, unknown flags, session selectors).
fn carry_flags(tail: &[String], bool_flags: &[&str], value_flags: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < tail.len() {
        let a = tail[i].as_str();
        if bool_flags.contains(&a) {
            out.push(tail[i].clone());
        } else if let Some(eq) = a.find('=') {
            // `--flag=value` form: carry whole if the flag part is an allowed value flag.
            if value_flags.contains(&&a[..eq]) {
                out.push(tail[i].clone());
            }
        } else if value_flags.contains(&a) {
            out.push(tail[i].clone());
            if i + 1 < tail.len() {
                out.push(tail[i + 1].clone());
                i += 1; // consume the value
            }
        }
        i += 1;
    }
    out
}

/// Read Codex's live conversation id from the rollout file the process holds open
/// (`~/.codex/sessions/**/rollout-<ts>-<uuid>.jsonl`). An interactive Codex TUI keeps exactly
/// its one session file open; if several are open (broker/subsessions), the most-recently
/// modified one is the live session. `None` if no rollout is open (→ fresh start on restore).
pub fn codex_session_id(pid: u32) -> Option<String> {
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for path in crate::procinfo::open_files(pid) {
        let Some(id) = rollout_session_id(&path) else {
            continue;
        };
        let mtime = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(bm, _)| mtime >= *bm) {
            best = Some((mtime, id));
        }
    }
    best.map(|(_, id)| id)
}

/// Extract the UUID from a Codex rollout path `…/sessions/…/rollout-<ts>-<uuid>.jsonl`. The
/// timestamp segment also contains dashes, so the id is the trailing five hex groups. `None`
/// unless the path is under a `sessions` dir and ends in a valid rollout filename.
pub fn is_rollout_path(path: &std::path::Path) -> bool {
    rollout_session_id(path).is_some()
}

fn rollout_session_id(path: &std::path::Path) -> Option<String> {
    if !path.components().any(|c| c.as_os_str() == "sessions") {
        return None;
    }
    let stem = path
        .file_name()?
        .to_str()?
        .strip_prefix("rollout-")?
        .strip_suffix(".jsonl")?;
    let groups: Vec<&str> = stem.split('-').collect();
    if groups.len() < 5 {
        return None;
    }
    let id = groups[groups.len() - 5..].join("-");
    is_session_uuid(&id).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::{CellColor, CellSnap, Snapshot};

    fn snap_from(lines: &[&str]) -> Snapshot {
        let cells: Vec<Vec<CellSnap>> = lines
            .iter()
            .map(|l| {
                l.chars()
                    .map(|ch| CellSnap {
                        sym: ch.to_string(),
                        spacer: false,
                        fg: CellColor::Default,
                        bg: CellColor::Default,
                        bold: false,
                        reverse: false,
                    })
                    .collect()
            })
            .collect();
        let wrapped = vec![false; cells.len()];
        Snapshot {
            cols: 80,
            rows: cells.len() as u16,
            cells,
            wrapped,
            cursor: (0, 0),
        }
    }

    #[test]
    fn decision_beats_everything() {
        let s = snap_from(&["✻ working…", "Do you want to proceed?", "❯ 1. Yes"]);
        assert_eq!(screen_status(&s), AgentStatus::Blocked);
    }

    #[test]
    fn interrupt_is_working() {
        let s = snap_from(&["Baking… (esc to interrupt)"]);
        assert_eq!(screen_status(&s), AgentStatus::Working);
    }

    #[test]
    fn free_prompt_is_ready() {
        let s = snap_from(&["some output", "❯ ", ""]);
        assert_eq!(screen_status(&s), AgentStatus::Ready);
        let s2 = snap_from(&["❯ draft text here"]);
        assert_eq!(screen_status(&s2), AgentStatus::Ready);
    }

    #[test]
    fn numbered_cursor_is_not_ready() {
        // `❯ 1.` alone (no decision marker) must NOT read as ready.
        let s = snap_from(&["❯ 1. Yes", "  2. No"]);
        assert_ne!(screen_status(&s), AgentStatus::Ready);
    }

    // ===== codex (codex-cli 0.154.0). Every fixture below is a VERBATIM capture from a
    // live codex pane read back with `comux capture-pane`, not a reconstruction. Before
    // this unit all three reported `Idle`, because `is_ready_prompt` only knew Claude's
    // `❯` and the spinner set only knew Claude's glyphs.

    #[test]
    fn codex_parked_at_its_composer_is_ready() {
        let s = snap_from(&[
            "  Done.",
            "  ───────────────────────────────────────────",
            "› Ask Codex to do anything",
            "  Luna Reserve medium · /private/tmp",
        ]);
        assert_eq!(screen_status(&s), AgentStatus::Ready);
    }

    #[test]
    fn codex_running_a_turn_is_working_even_though_its_composer_is_still_drawn() {
        // The regression this guards: codex keeps the composer live during a turn (so you
        // can queue a follow-up), so a ready-prompt check that ran first would call a
        // WORKING pane ready. `esc to interrupt` is matched at step 2, ahead of it.
        let s = snap_from(&[
            "› list the files here then say done",
            "• Working (2s • esc to interrupt)",
            "› Ask Codex to do anything",
            "  Luna Reserve medium · /private/tmp",
        ]);
        assert_eq!(screen_status(&s), AgentStatus::Working);
    }

    #[test]
    fn codex_approval_prompt_is_blocked() {
        let s = snap_from(&[
            "  Would you like to run the following command?",
            "  $ touch /private/tmp/approval-probe.txt",
            "› 1. Yes, proceed (y)",
            "  2. Yes, and don't ask again for commands that start with `touch` (p)",
            "  3. No, and tell Codex what to do differently (esc)",
            "  Press enter to confirm or esc to cancel",
        ]);
        assert_eq!(screen_status(&s), AgentStatus::Blocked);
    }

    #[test]
    fn codex_directory_trust_prompt_is_blocked() {
        let s = snap_from(&[
            "> You are in /private/tmp",
            "  Do you trust the contents of this directory?",
            "› 1. Yes, continue",
            "  2. No, quit",
        ]);
        assert_eq!(screen_status(&s), AgentStatus::Blocked);
    }

    #[test]
    fn an_unenumerated_codex_picker_is_caught_by_the_confirm_footer() {
        // The usage-limit picker: none of the `Would you like…` strings match, so only the
        // generic footer identifies it. This is what keeps a dialog we never enumerated
        // from being reported as something confident.
        let s = snap_from(&[
            "  You're now using Luna, a faster model for simpler tasks.",
            "› 1. Upgrade",
            "  2. Reset usage",
            "  3. Continue with Luna Reserve",
            "  Press enter to confirm or esc to continue working",
        ]);
        assert_eq!(screen_status(&s), AgentStatus::Blocked);
    }

    #[test]
    fn a_marker_hard_wrapped_by_a_narrow_pane_still_matches() {
        // Found by the e2e, not by reasoning: in a SPLIT pane codex's 43-column approval
        // question arrives as two rows, and a plain substring match over the joined window
        // misses it — so a blocked agent in a narrow pane kept reading `idle`, the exact
        // bug this unit exists to kill, just in a different place.
        // The break is MID-WORD, which is what a terminal actually does — the first version
        // of this test wrapped at a space and so passed against a join-with-space fix that
        // reassembled `the f` + `ollowing` as `the f ollowing` and still matched nothing.
        let s = snap_from(&[
            "Would you like to run the f",
            "ollowing command?",
            "› 1. Yes, proceed (y)",
        ]);
        assert_eq!(screen_status(&s), AgentStatus::Blocked);
        // Same for the working marker, which wraps just as easily.
        let w = snap_from(&["• Working (2s • esc to inte", "rrupt)"]);
        assert_eq!(screen_status(&w), AgentStatus::Working);
    }

    #[test]
    fn cursor_rest_handles_the_degenerate_lines() {
        assert_eq!(cursor_rest(""), None);
        assert_eq!(cursor_rest("   "), None);
        assert_eq!(cursor_rest("   \t "), None);
        assert_eq!(cursor_rest("plain text"), None);
        // A multibyte NON-cursor first char must not be mistaken for one, and must not panic.
        assert_eq!(cursor_rest("→ arrow"), None);
        assert_eq!(cursor_rest("한글 줄"), None);
        // A bare cursor yields an empty remainder (which IS a ready prompt).
        assert_eq!(cursor_rest("  ❯"), Some(""));
        assert_eq!(cursor_rest("›   "), Some(""));
        assert_eq!(cursor_rest(" › 1. Yes"), Some("1. Yes"));
        // Multibyte content after the glyph slices on a char boundary.
        assert_eq!(cursor_rest("❯ 한글"), Some("한글"));
        assert!(is_ready_prompt("❯ 한글"));
    }

    #[test]
    fn a_window_with_no_usable_cursor_line_falls_through_to_idle() {
        // Both fall-throughs must reach `Idle`, never a confident status: no cursor line at
        // all, and a cursor line that is only a numbered selection with no decision marker.
        assert_eq!(
            screen_status(&snap_from(&["just output", "more output"])),
            AgentStatus::Idle
        );
        assert_eq!(
            screen_status(&snap_from(&["› 1. Yes", "  2. No"])),
            AgentStatus::Idle
        );
    }

    #[test]
    fn a_short_claude_marker_may_not_straddle_two_rows() {
        // The cost of wrap-tolerant matching, contained on purpose: `to navigate ·` is 13
        // characters, short enough that squeezing the whole window could assemble it out of
        // two unrelated rows. Claude's markers therefore keep the old verbatim match, so
        // this stays NOT blocked — a straddle must not manufacture a desktop toast.
        let s = snap_from(&["press Enter to", "select a file", "some other output"]);
        assert_ne!(screen_status(&s), AgentStatus::Blocked);
    }

    // ===== the position rule (REV 4b): the BOTTOM-most cursor line decides.

    #[test]
    fn a_stale_answered_list_above_a_live_prompt_is_still_ready() {
        // `lines` is built bottom-up, so this is the case that fails if the scan direction
        // is inverted: the top-most cursor line is a numbered selection, the bottom-most is
        // the live prompt. Under the old `any()` this passed by luck; it must now pass by
        // construction.
        let s = snap_from(&["❯ 2. No", "  (answered)", "❯ "]);
        assert_eq!(screen_status(&s), AgentStatus::Ready);
    }

    #[test]
    fn a_selection_below_a_prompt_is_not_ready() {
        // The mirror image, and the one `any()` got wrong: a dialog opened UNDER what was
        // the composer. No decision marker here, so the fall-through must be `Idle` — never
        // a confident `Ready`, which `wait-agent --status ready` would act on.
        let s = snap_from(&["❯ draft text", "  Pick one:", "❯ 1. Yes"]);
        assert_ne!(screen_status(&s), AgentStatus::Ready);
        assert_eq!(screen_status(&s), AgentStatus::Idle);
    }

    #[test]
    fn a_codex_numbered_cursor_is_not_a_ready_prompt() {
        assert!(!is_ready_prompt("› 1. Yes, proceed (y)"));
        assert!(is_ready_prompt("› Ask Codex to do anything"));
        assert!(is_ready_prompt("›"));
    }

    #[test]
    fn decision_markers_do_not_fire_on_ordinary_agent_output() {
        // A false Blocked raises a desktop toast and bumps `⚑N`, so the markers have to be
        // inert against text an agent merely PRINTS. `Press enter to continue` — a pager
        // footer an agent quotes all the time — was dropped from the set for exactly this.
        let s = snap_from(&[
            "  I ran `git log` and it printed:",
            "  Press enter to continue",
            "  …then the diff. Would you like me to run the following command?",
            "• Working (4s • esc to interrupt)",
        ]);
        assert_eq!(screen_status(&s), AgentStatus::Working);
    }

    #[test]
    fn spinner_is_working_when_no_prompt() {
        let s = snap_from(&["✷ Thinking"]);
        assert_eq!(screen_status(&s), AgentStatus::Working);
    }

    #[test]
    fn plain_shell_is_idle() {
        let s = snap_from(&["~/dev/copad $ ls", "Cargo.toml  src"]);
        assert_eq!(screen_status(&s), AgentStatus::Idle);
    }

    #[test]
    fn session_status_maps_and_tolerates_extra_fields() {
        // extra fields (pid/cwd/version/…) are ignored, like Claude's real file.
        let busy = r#"{"status":"busy","pid":42,"cwd":"/x","version":"2.1"}"#;
        assert_eq!(parse_session_status(busy), Some(AgentStatus::Working));
        assert_eq!(
            parse_session_status(r#"{"status":"idle"}"#),
            Some(AgentStatus::Ready)
        );
        assert_eq!(
            parse_session_status(r#"{"status":"waiting"}"#),
            Some(AgentStatus::Blocked)
        );
        // unknown / missing / malformed → None (caller falls back to the screen).
        assert_eq!(parse_session_status(r#"{"status":"weird"}"#), None);
        assert_eq!(parse_session_status(r#"{"other":1}"#), None);
        assert_eq!(parse_session_status("not json"), None);
    }

    const ID: &str = "36213526-fd15-4fc9-b146-842f71382088";

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn is_session_uuid_validates_shape() {
        assert!(is_session_uuid(ID));
        assert!(is_session_uuid("019f91a2-b982-7681-9493-14ad6d653f1e"));
        assert!(!is_session_uuid("not-a-uuid"));
        assert!(!is_session_uuid("36213526fd154fc9b146842f71382088")); // no dashes
        assert!(!is_session_uuid("36213526-fd15-4fc9-b146-842f7138208g")); // non-hex
        assert!(!is_session_uuid("")); // empty
    }

    #[test]
    fn claude_resume_rebuilds_and_carries_safe_flags() {
        // Bare launch → resume with the live id.
        assert_eq!(
            resume_argv("claude", &argv(&["claude"]), ID),
            argv(&["claude", "--resume", ID])
        );
        // A full path basename still classifies as claude via the caller; here comm is passed.
        assert_eq!(
            resume_argv(
                "claude",
                &argv(&["claude", "--dangerously-skip-permissions"]),
                ID
            ),
            argv(&["claude", "--resume", ID, "--dangerously-skip-permissions"])
        );
        // Value flag + its argument carried; unknown flag + positional prompt dropped.
        assert_eq!(
            resume_argv(
                "claude",
                &argv(&["claude", "--model", "opus", "--unknown", "fix the bug"]),
                ID
            ),
            argv(&["claude", "--resume", ID, "--model", "opus"])
        );
        // `--model=opus` (equals form) carried whole.
        assert_eq!(
            resume_argv("claude", &argv(&["claude", "--model=opus"]), ID),
            argv(&["claude", "--resume", ID, "--model=opus"])
        );
    }

    #[test]
    fn claude_resume_replaces_stale_selectors_with_live_id() {
        // Launched with an explicit (now stale) --resume: the live id wins, the old id is gone.
        assert_eq!(
            resume_argv(
                "claude",
                &argv(&["claude", "--resume", "00000000-0000-0000-0000-000000000000"]),
                ID
            ),
            argv(&["claude", "--resume", ID])
        );
        // Bare --resume (interactive picker) / --continue likewise collapse to the live id.
        assert_eq!(
            resume_argv("claude", &argv(&["claude", "--resume"]), ID),
            argv(&["claude", "--resume", ID])
        );
        assert_eq!(
            resume_argv("claude", &argv(&["claude", "--continue"]), ID),
            argv(&["claude", "--resume", ID])
        );
        // --session-id / --fork-session must not survive alongside the injected --resume.
        assert_eq!(
            resume_argv(
                "claude",
                &argv(&["claude", "--session-id", ID, "--fork-session"]),
                ID
            ),
            argv(&["claude", "--resume", ID])
        );
    }

    #[test]
    fn claude_resume_skips_non_interactive() {
        // -p / --print one-shots and --no-session-persistence are left verbatim (nothing to
        // resume interactively).
        for flag in ["-p", "--print", "--no-session-persistence"] {
            let a = argv(&["claude", flag]);
            assert_eq!(resume_argv("claude", &a, ID), a);
        }
    }

    #[test]
    fn resume_argv_rejects_bad_input() {
        // Non-UUID id → untouched.
        assert_eq!(
            resume_argv("claude", &argv(&["claude"]), "bogus"),
            argv(&["claude"])
        );
        // Unknown agent → untouched.
        assert_eq!(
            resume_argv("vim", &argv(&["vim", "file"]), ID),
            argv(&["vim", "file"])
        );
        // Empty argv → untouched.
        assert!(resume_argv("claude", &[], ID).is_empty());
    }

    #[test]
    fn codex_resume_uses_subcommand_form() {
        assert_eq!(
            resume_argv("codex", &argv(&["codex"]), ID),
            argv(&["codex", "resume", ID])
        );
        // Flags and a prompt are dropped (already captured in the rollout being resumed).
        assert_eq!(
            resume_argv("codex", &argv(&["codex", "-m", "gpt-5", "do it"]), ID),
            argv(&["codex", "resume", ID])
        );
        // An explicit subcommand is never wrapped (would break a non-interactive run / double
        // a resume).
        for sub in ["exec", "resume", "review", "apply"] {
            let a = argv(&["codex", sub, "arg"]);
            assert_eq!(resume_argv("codex", &a, ID), a);
        }
        // A subcommand preceded by a value-taking global flag (whose value is a bare token) is
        // still detected — we scan every token, not just the first non-option one.
        let a = argv(&["codex", "--cd", "/repo", "exec", "run tests"]);
        assert_eq!(resume_argv("codex", &a, ID), a);
    }

    #[test]
    fn rollout_session_id_parses_trailing_uuid() {
        let p = std::path::Path::new(
            "/home/u/.codex/sessions/2026/07/24/rollout-2026-07-24T09-59-48-36213526-fd15-4fc9-b146-842f71382088.jsonl",
        );
        assert_eq!(rollout_session_id(p).as_deref(), Some(ID));
        // Not under a `sessions` dir → rejected.
        let outside = std::path::Path::new(
            "/tmp/rollout-2026-07-24T09-59-48-36213526-fd15-4fc9-b146-842f71382088.jsonl",
        );
        assert_eq!(rollout_session_id(outside), None);
        // Wrong shape → rejected.
        let bad = std::path::Path::new("/x/sessions/rollout-nope.jsonl");
        assert_eq!(rollout_session_id(bad), None);
    }
}
