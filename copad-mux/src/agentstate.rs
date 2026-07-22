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

/// Read `~/.claude/sessions/<pid>.json` → `status` field. `None` on any failure
/// (missing/unreadable/malformed/unknown status) so the caller falls back.
pub fn claude_session_status(pid: u32) -> Option<AgentStatus> {
    let home = std::env::var_os("HOME")?;
    let path = std::path::Path::new(&home)
        .join(".claude")
        .join("sessions")
        .join(format!("{pid}.json"));
    parse_session_status(&std::fs::read_to_string(path).ok()?)
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
/// priority — tmx `classify.rs`).
const DECISION_MARKERS: &[&str] = &[
    "Enter to select",
    "to navigate ·",
    "Do you want to proceed",
    "Allow this tool",
];
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

    // 1) a decision dialog wins over everything.
    if DECISION_MARKERS.iter().any(|m| joined.contains(m)) {
        return AgentStatus::Blocked;
    }
    // 2) explicit "running, esc to interrupt".
    if joined.contains("esc to interrupt") {
        return AgentStatus::Working;
    }
    // 3) a free `❯` prompt → ready (checked before the spinner so decorative
    //    sparkles in a message don't override a genuine prompt).
    if lines.iter().any(|l| is_ready_prompt(l)) {
        return AgentStatus::Ready;
    }
    // 4) a spinner anywhere → still working.
    if joined.chars().any(|c| SPINNERS.contains(&c)) {
        return AgentStatus::Working;
    }
    // 5) nothing recognised.
    AgentStatus::Idle
}

/// A "free" Claude prompt: first non-space char is `❯` and what follows is empty or
/// whitespace or free text — but NOT a numbered selection cursor (`❯ 1.`), which is a
/// decision, not a ready prompt (tmx `has_ready_prompt`).
fn is_ready_prompt(line: &str) -> bool {
    let t = line.trim_start();
    let mut chars = t.chars();
    if chars.next() != Some('❯') {
        return false;
    }
    let rest = chars.as_str().trim_start();
    if rest.is_empty() {
        return true; // bare `❯`
    }
    // reject a numbered selection like `❯ 1.` / `❯ 12.`
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && rest[digits.len()..].starts_with('.') {
        return false;
    }
    true
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
        Snapshot {
            cols: 80,
            rows: cells.len() as u16,
            cells,
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
}
