//! `coctl usage --limits` — subscription rate-limit window utilization.
//!
//! A DIFFERENT data source than the token/cost aggregation in this module:
//! percentages are what Claude Code's `/usage` and Codex's TUI show ("X% of
//! your 5h limit"), sourced per provider:
//!
//!   * **Claude** — a live `GET https://api.anthropic.com/api/oauth/usage`
//!     with the OAuth bearer token from `~/.claude/.credentials.json`. Returns
//!     `five_hour.utilization` / `seven_day.utilization` (already a percent).
//!     Skipped (→ `None`) when the token is missing or already expired —
//!     refreshing the OAuth token is Claude Code's job, not ours.
//!   * **Codex** — the newest `~/.codex/sessions/**/rollout-*.jsonl` records a
//!     `rate_limits` snapshot (`payload.rate_limits`) on every turn. Read the
//!     most recent one — no auth, no network, but only as fresh as the last
//!     Codex turn.

use chrono::Local;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Per-provider rate-limit windows. Percentages are backend-rounded (e.g.
/// `27.0` = 27%). `None` = provider/window unavailable (no auth, expired token,
/// no rollout, network error) — rendered as absent, never as `0%`.
#[derive(Debug, Clone, Default)]
pub struct Limits {
    pub claude: Option<ClaudeLimits>,
    pub codex: Option<CodexLimits>,
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeLimits {
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct CodexLimits {
    pub weekly: Option<f64>,
}

/// Gather both providers' limits, honoring a `--tool` filter.
pub fn collect(home: &str, want_claude: bool, want_codex: bool) -> Limits {
    Limits {
        claude: if want_claude {
            claude_limits(home)
        } else {
            None
        },
        codex: if want_codex { codex_limits(home) } else { None },
    }
}

// ── Claude: live OAuth usage endpoint ──────────────────────────────────────

fn claude_limits(home: &str) -> Option<ClaudeLimits> {
    let raw = std::fs::read_to_string(Path::new(home).join(".claude/.credentials.json")).ok()?;
    let creds: Value = serde_json::from_str(&raw).ok()?;
    let oauth = creds.get("claudeAiOauth")?;
    let token = oauth.get("accessToken")?.as_str()?;

    // `expiresAt` is epoch-millis. A live call with an expired token just 401s;
    // short-circuit so we don't spend a request (and a timeout) to learn that.
    if let Some(exp) = oauth.get("expiresAt").and_then(Value::as_i64)
        && Local::now().timestamp_millis() >= exp
    {
        return None;
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()
        .ok()?;
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().ok()?;
    Some(parse_claude_usage(&body))
}

/// Pull the two window utilizations out of the `/api/oauth/usage` body. Pure so
/// it can be tested against a captured response.
fn parse_claude_usage(body: &Value) -> ClaudeLimits {
    let pct = |k: &str| {
        body.get(k)
            .and_then(|w| w.get("utilization"))
            .and_then(Value::as_f64)
    };
    ClaudeLimits {
        five_hour: pct("five_hour"),
        seven_day: pct("seven_day"),
    }
}

// ── Codex: newest rollout rate_limits snapshot ─────────────────────────────

fn codex_limits(home: &str) -> Option<CodexLimits> {
    let root = Path::new(home).join(".codex/sessions");
    let newest = newest_rollout(&root)?;
    let bytes = std::fs::read(&newest).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // Reverse-scan for the LAST rate_limits snapshot; parse only that line
    // rather than every line of a possibly-large rollout.
    for line in text.lines().rev() {
        if !line.contains("\"rate_limits\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(rl) = find_key(&v, "rate_limits") {
            return Some(parse_codex_rate_limits(rl));
        }
    }
    None
}

/// The rollout file with the most recent mtime under `root` (recursive).
fn newest_rollout(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    collect_newest(root, &mut best);
    best.map(|(_, p)| p)
}

fn collect_newest(dir: &Path, best: &mut Option<(SystemTime, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        // Don't follow symlinks → no cycle on a self-referential dir.
        let Ok(ft) = e.file_type() else { continue };
        let p = e.path();
        if ft.is_dir() {
            collect_newest(&p, best);
        } else if ft.is_file()
            && p.extension().and_then(|s| s.to_str()) == Some("jsonl")
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("rollout-"))
            && let Ok(mt) = e.metadata().and_then(|m| m.modified())
            && best.as_ref().is_none_or(|(bt, _)| mt > *bt)
        {
            *best = Some((mt, p));
        }
    }
}

/// Recursively find the first value under `key` anywhere in the JSON tree.
fn find_key<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(map) => {
            if let Some(hit) = map.get(key) {
                return Some(hit);
            }
            map.values().find_map(|child| find_key(child, key))
        }
        Value::Array(items) => items.iter().find_map(|child| find_key(child, key)),
        _ => None,
    }
}

/// From `{primary: {used_percent, window_minutes}, secondary: {...}}` pick the
/// widest window (≥ 1 day) as "weekly". Codex reports 5h + weekly as
/// primary/secondary in no fixed order, so choose by `window_minutes`.
fn parse_codex_rate_limits(rl: &Value) -> CodexLimits {
    const DAY_MINUTES: f64 = 1440.0;
    let mut weekly = None;
    let mut widest = -1.0;
    for slot in ["primary", "secondary"] {
        let Some(w) = rl.get(slot).filter(|w| w.is_object()) else {
            continue;
        };
        let window = w
            .get("window_minutes")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let used = w.get("used_percent").and_then(Value::as_f64);
        if let Some(used) = used
            && window >= DAY_MINUTES
            && window > widest
        {
            widest = window;
            weekly = Some(used);
        }
    }
    CodexLimits { weekly }
}

// ── Rendering ──────────────────────────────────────────────────────────────

/// One line for a status bar: `claude 5h 5% wk 27% · codex wk 45%`. Windows
/// that are unavailable are simply omitted; all-absent → `no limits`.
pub fn oneline(l: &Limits) -> String {
    let mut parts = Vec::new();
    if let Some(c) = &l.claude {
        let mut seg = String::from("claude");
        if let Some(p) = c.five_hour {
            seg.push_str(&format!(" 5h {p:.0}%"));
        }
        if let Some(p) = c.seven_day {
            seg.push_str(&format!(" wk {p:.0}%"));
        }
        if seg != "claude" {
            parts.push(seg);
        }
    }
    if let Some(x) = &l.codex
        && let Some(p) = x.weekly
    {
        parts.push(format!("codex wk {p:.0}%"));
    }
    if parts.is_empty() {
        return "no limits".to_string();
    }
    parts.join(" · ")
}

/// Machine shape. Mirrors `oneline`: unavailable providers/windows are OMITTED,
/// not emitted as `null` — so `--tool codex` never mentions `claude`, and a
/// provider with no populated windows drops out entirely.
pub fn to_json(l: &Limits) -> Value {
    let mut root = serde_json::Map::new();
    if let Some(c) = &l.claude {
        let mut m = serde_json::Map::new();
        if let Some(p) = c.five_hour {
            m.insert("five_hour".into(), json!(p));
        }
        if let Some(p) = c.seven_day {
            m.insert("seven_day".into(), json!(p));
        }
        if !m.is_empty() {
            root.insert("claude".into(), Value::Object(m));
        }
    }
    if let Some(x) = &l.codex
        && let Some(p) = x.weekly
    {
        root.insert("codex".into(), json!({ "weekly": p }));
    }
    Value::Object(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_claude_usage_body() {
        let body = json!({
            "five_hour": {"utilization": 5.0, "resets_at": "..."},
            "seven_day": {"utilization": 27.0, "resets_at": "..."},
        });
        let c = parse_claude_usage(&body);
        assert_eq!(c.five_hour, Some(5.0));
        assert_eq!(c.seven_day, Some(27.0));
    }

    #[test]
    fn claude_body_missing_window_is_none() {
        let body = json!({ "five_hour": {"utilization": 5.0} });
        let c = parse_claude_usage(&body);
        assert_eq!(c.five_hour, Some(5.0));
        assert_eq!(c.seven_day, None);
    }

    #[test]
    fn picks_weekly_window_by_width() {
        // primary is the 5h window, secondary the weekly — weekly must win.
        let rl = json!({
            "primary": {"used_percent": 12.0, "window_minutes": 300},
            "secondary": {"used_percent": 45.0, "window_minutes": 10080},
        });
        assert_eq!(parse_codex_rate_limits(&rl).weekly, Some(45.0));
    }

    #[test]
    fn weekly_ignores_sub_day_windows() {
        let rl = json!({
            "primary": {"used_percent": 12.0, "window_minutes": 300},
            "secondary": null,
        });
        assert_eq!(parse_codex_rate_limits(&rl).weekly, None);
    }

    #[test]
    fn weekly_from_single_primary() {
        let rl = json!({
            "primary": {"used_percent": 45.0, "window_minutes": 10080},
            "secondary": null,
        });
        assert_eq!(parse_codex_rate_limits(&rl).weekly, Some(45.0));
    }

    #[test]
    fn find_key_reaches_nested_payload() {
        let line = json!({
            "type": "event_msg",
            "payload": {"type": "token_count", "rate_limits": {"limit_id": "codex"}},
        });
        let rl = find_key(&line, "rate_limits").unwrap();
        assert_eq!(rl.get("limit_id").and_then(Value::as_str), Some("codex"));
    }

    #[test]
    fn oneline_full() {
        let l = Limits {
            claude: Some(ClaudeLimits {
                five_hour: Some(5.0),
                seven_day: Some(27.0),
            }),
            codex: Some(CodexLimits { weekly: Some(45.0) }),
        };
        assert_eq!(oneline(&l), "claude 5h 5% wk 27% · codex wk 45%");
    }

    #[test]
    fn oneline_claude_only() {
        let l = Limits {
            claude: Some(ClaudeLimits {
                five_hour: Some(80.0),
                seven_day: None,
            }),
            codex: None,
        };
        assert_eq!(oneline(&l), "claude 5h 80%");
    }

    #[test]
    fn json_omits_unavailable_providers_and_windows() {
        // --tool codex: no claude key at all (not `"claude": null`).
        let l = Limits {
            claude: None,
            codex: Some(CodexLimits { weekly: Some(45.0) }),
        };
        let v = to_json(&l);
        assert!(v.get("claude").is_none());
        assert_eq!(v["codex"]["weekly"], json!(45.0));

        // A present provider with an unavailable window omits just that window.
        let l = Limits {
            claude: Some(ClaudeLimits {
                five_hour: Some(6.0),
                seven_day: None,
            }),
            codex: None,
        };
        let v = to_json(&l);
        assert_eq!(v["claude"]["five_hour"], json!(6.0));
        assert!(v["claude"].get("seven_day").is_none());
        assert!(v.get("codex").is_none());

        // Nothing available → empty object, never `{"claude":null,"codex":null}`.
        assert_eq!(to_json(&Limits::default()), json!({}));
    }

    #[test]
    fn oneline_empty() {
        assert_eq!(oneline(&Limits::default()), "no limits");
        // A claude struct with no populated windows also collapses to empty.
        let l = Limits {
            claude: Some(ClaudeLimits::default()),
            codex: None,
        };
        assert_eq!(oneline(&l), "no limits");
    }
}
