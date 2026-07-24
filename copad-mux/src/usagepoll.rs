//! Background poller for the status-bar usage/limits readout.
//!
//! Fetches `coctl usage --limits --json` — Claude 5h + weekly (a live OAuth
//! call) and Codex weekly (newest rollout snapshot) — parses the percentages
//! into a [`UsageSnapshot`], and shares it with the render loop. Numbers (not a
//! pre-formatted string) so the status bar can render either text or a progress
//! bar per config + width. Shelling out does network I/O, so it runs on a
//! dedicated thread, never the render loop. `COPAD_MUX_USAGE=0` disables it.

use std::ffi::{OsStr, OsString};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

/// How often to re-poll. The 5h / weekly windows move slowly and each poll is a
/// process spawn + network round-trip, so a minute is plenty.
const POLL: Duration = Duration::from_secs(60);

const BAR_FILLED: char = '━';
const BAR_EMPTY: char = '╌';

/// Parsed rate-limit percentages. `None` window = unavailable (omitted); `stale`
/// = the provider was served from coctl's cache (rendered with a `~`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsageSnapshot {
    pub claude_5h: Option<f64>,
    pub claude_wk: Option<f64>,
    pub claude_stale: bool,
    pub codex_wk: Option<f64>,
    pub codex_stale: bool,
}

impl UsageSnapshot {
    pub fn is_empty(&self) -> bool {
        self.claude_5h.is_none() && self.claude_wk.is_none() && self.codex_wk.is_none()
    }

    /// Percentages: `claude 5h 5% wk 34% · codex wk 60%` (stale provider `~`-prefixed).
    pub fn text(&self) -> String {
        self.parts(None).iter().map(UsagePart::text).collect()
    }

    /// A progress bar per window: `claude 5h ━━╌╌╌╌╌╌ 5% wk ━━━╌╌╌╌╌ 34% · codex wk …`.
    pub fn bar(&self, width: u16) -> String {
        self.parts(Some(width))
            .iter()
            .map(UsagePart::text)
            .collect()
    }

    /// The readout broken into render parts so the status bar can color each
    /// window by its utilization (threshold coloring) while leaving labels and
    /// separators neutral. `bar_width = None` = text; `Some(w)` = a `w`-cell bar
    /// before each percent. The concatenation equals [`Self::text`]/[`Self::bar`].
    pub fn parts(&self, bar_width: Option<u16>) -> Vec<UsagePart> {
        let cell = |pct: f64| match bar_width {
            Some(w) => format!("{} ", bar_glyphs(pct, w)),
            None => String::new(),
        };
        // A window = neutral " <label> " + a gauge (`bar %`) colored by threshold.
        let window = |out: &mut Vec<UsagePart>, label: &str, pct: f64| {
            out.push(UsagePart::Neutral(format!(" {label} ")));
            out.push(UsagePart::window(format!("{}{pct:.0}%", cell(pct)), pct));
        };
        let mut out = Vec::new();
        let has_claude = self.claude_5h.is_some() || self.claude_wk.is_some();
        if has_claude {
            out.push(UsagePart::Neutral(
                if self.claude_stale {
                    "~claude"
                } else {
                    "claude"
                }
                .to_string(),
            ));
            if let Some(p) = self.claude_5h {
                window(&mut out, "5h", p);
            }
            if let Some(p) = self.claude_wk {
                window(&mut out, "wk", p);
            }
        }
        if let Some(p) = self.codex_wk {
            if has_claude {
                out.push(UsagePart::Neutral(" · ".to_string()));
            }
            out.push(UsagePart::Neutral(
                if self.codex_stale { "~codex" } else { "codex" }.to_string(),
            ));
            window(&mut out, "wk", p);
        }
        out
    }
}

/// One piece of the rendered readout. `Window` chunks carry their utilization so
/// the caller can color them by threshold; `Neutral` is labels/separators.
#[derive(Debug, Clone, PartialEq)]
pub enum UsagePart {
    Window { text: String, pct: f64 },
    Neutral(String),
}

impl UsagePart {
    fn window(text: String, pct: f64) -> Self {
        UsagePart::Window { text, pct }
    }

    pub fn text(&self) -> &str {
        match self {
            UsagePart::Window { text, .. } => text,
            UsagePart::Neutral(s) => s,
        }
    }
}

/// `filled`/`empty` glyphs proportional to `pct` (0–100) over `width` cells.
fn bar_glyphs(pct: f64, width: u16) -> String {
    let w = width as usize;
    let filled = ((pct / 100.0) * width as f64)
        .round()
        .clamp(0.0, width as f64) as usize;
    let mut s = String::with_capacity(w * 3);
    s.extend(std::iter::repeat_n(BAR_FILLED, filled));
    s.extend(std::iter::repeat_n(BAR_EMPTY, w - filled));
    s
}

/// Display width of the bar rendering — used by the status bar to decide whether
/// the terminal is wide enough for bars before falling back to text.
pub fn bar_display_width(u: &UsageSnapshot, width: u16) -> usize {
    u.bar(width).width()
}

/// Latest snapshot shared with the render loop. `None` = nothing fetched yet /
/// disabled; `Some(empty)` = fetched but nothing to show.
pub type Shared = Arc<Mutex<Option<UsageSnapshot>>>;

/// An empty handle with no poller behind it (default before `spawn`, and in
/// tests that construct an `App` without a server).
pub fn idle() -> Shared {
    Arc::new(Mutex::new(None))
}

/// Spawn the detached poller thread and return the handle the status bar reads.
/// `COPAD_MUX_USAGE=0` returns an idle handle that stays empty forever.
pub fn spawn() -> Shared {
    let shared = idle();
    if std::env::var("COPAD_MUX_USAGE").is_ok_and(|v| v == "0") {
        return shared;
    }
    let out = shared.clone();
    let _ = std::thread::Builder::new()
        .name("usage-poll".into())
        .spawn(move || {
            let coctl = coctl_path();
            loop {
                if let Some(s) = fetch(&coctl)
                    && let Ok(mut g) = out.lock()
                {
                    *g = Some(s);
                }
                std::thread::sleep(POLL);
            }
        });
    shared
}

/// Run `coctl usage --limits --json` and parse it. `None` = the command couldn't
/// run / errored → keep the previous snapshot rather than blanking on a transient
/// failure; `Some(snapshot)` (possibly empty) = a fresh reading.
fn fetch(coctl: &OsStr) -> Option<UsageSnapshot> {
    let out = Command::new(coctl)
        .args(["usage", "--limits", "--json"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_json(&String::from_utf8_lossy(&out.stdout))
}

fn parse_json(s: &str) -> Option<UsageSnapshot> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    let mut snap = UsageSnapshot::default();
    if let Some(c) = v.get("claude") {
        snap.claude_5h = c.get("five_hour").and_then(serde_json::Value::as_f64);
        snap.claude_wk = c.get("seven_day").and_then(serde_json::Value::as_f64);
        snap.claude_stale = c.get("stale").and_then(serde_json::Value::as_bool) == Some(true);
    }
    if let Some(x) = v.get("codex") {
        snap.codex_wk = x.get("weekly").and_then(serde_json::Value::as_f64);
        snap.codex_stale = x.get("stale").and_then(serde_json::Value::as_bool) == Some(true);
    }
    Some(snap)
}

/// Prefer the `coctl` next to the running `comux` binary (install scripts drop
/// them together) — the server is often launched from a desktop entry / cron
/// with a PATH that lacks `~/.local/bin`. Fall back to bare `coctl` on PATH.
fn coctl_path() -> OsString {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let sibling = dir.join("coctl");
        if sibling.is_file() {
            return sibling.into_os_string();
        }
    }
    "coctl".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> UsageSnapshot {
        UsageSnapshot {
            claude_5h: Some(5.0),
            claude_wk: Some(34.0),
            claude_stale: false,
            codex_wk: Some(60.0),
            codex_stale: false,
        }
    }

    #[test]
    fn parses_json_shape() {
        let s = r#"{"claude":{"five_hour":5.0,"seven_day":34.0},"codex":{"weekly":60.0}}"#;
        assert_eq!(parse_json(s).unwrap(), full());
    }

    #[test]
    fn parses_stale_and_partial() {
        let s = r#"{"claude":{"five_hour":5.0,"stale":true}}"#;
        let snap = parse_json(s).unwrap();
        assert_eq!(snap.claude_5h, Some(5.0));
        assert_eq!(snap.claude_wk, None);
        assert!(snap.claude_stale);
        assert!(snap.codex_wk.is_none());
    }

    #[test]
    fn empty_json_is_empty_snapshot() {
        assert!(parse_json("{}").unwrap().is_empty());
        assert!(parse_json("not json").is_none());
    }

    #[test]
    fn text_matches_percent_format() {
        assert_eq!(full().text(), "claude 5h 5% wk 34% · codex wk 60%");
    }

    #[test]
    fn parts_carry_pct_and_concat_to_text() {
        let parts = full().parts(None);
        // Each gauge chunk carries its utilization (for threshold coloring); the
        // order is claude 5h, claude wk, codex wk.
        let pcts: Vec<f64> = parts
            .iter()
            .filter_map(|p| match p {
                UsagePart::Window { pct, .. } => Some(*pct),
                UsagePart::Neutral(_) => None,
            })
            .collect();
        assert_eq!(pcts, vec![5.0, 34.0, 60.0]);
        // Concatenation is byte-identical to the flat text form.
        let concat: String = parts.iter().map(UsagePart::text).collect();
        assert_eq!(concat, full().text());
    }

    #[test]
    fn text_marks_stale_provider() {
        let mut u = full();
        u.claude_stale = true;
        assert_eq!(u.text(), "~claude 5h 5% wk 34% · codex wk 60%");
    }

    #[test]
    fn bar_glyphs_are_proportional() {
        assert_eq!(bar_glyphs(0.0, 8), "╌╌╌╌╌╌╌╌");
        assert_eq!(bar_glyphs(100.0, 8), "━━━━━━━━");
        assert_eq!(bar_glyphs(50.0, 8), "━━━━╌╌╌╌");
        // rounds to nearest cell; clamps out-of-range
        assert_eq!(bar_glyphs(12.5, 8), "━╌╌╌╌╌╌╌");
        assert_eq!(bar_glyphs(150.0, 4), "━━━━");
    }

    #[test]
    fn bar_render_has_a_bar_per_window() {
        let s = full().bar(8);
        // 5% of 8 rounds to 0 filled cells; the "5%" label carries the value.
        assert!(s.contains("5h ╌╌╌╌╌╌╌╌ 5%"), "got: {s}");
        assert!(s.contains("wk ━━━╌╌╌╌╌ 34%"), "got: {s}"); // 34% → 3/8
        assert!(s.contains("codex wk ━━━━━╌╌╌ 60%"), "got: {s}"); // 60% → 5/8
    }
}
