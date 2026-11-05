//! Subscription rate-limit window utilization, shared by `coctl usage --limits` and the comux status bar.
//!
//! Claude: live `GET /api/oauth/usage` with the OAuth token from `~/.claude/.credentials.json` (macOS login
//! Keychain as fallback); skipped when the token is missing or expired, since refreshing it is Claude Code's job.
//! Codex: the last `rate_limits` snapshot in the newest `~/.codex/sessions/**/rollout-*.jsonl`.

use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

/// Max age of a cached value that may stand in for a live one; past it the provider drops rather than showing a stale percent.
const STALE_MAX_MS: i64 = 3 * 60 * 60 * 1000;

/// Percentages are backend-rounded. `None` = unavailable, rendered as absent, never as `0%`.
#[derive(Debug, Clone, Default)]
pub struct Limits {
    pub claude: Option<ClaudeLimits>,
    pub codex: Option<CodexLimits>,
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeLimits {
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
    /// UNIX epoch seconds at which each window resets; `None` when the backend omitted it.
    pub five_hour_reset: Option<i64>,
    pub seven_day_reset: Option<i64>,
}

impl ClaudeLimits {
    /// An all-`None` result is a 200 with no usable data and must not be cached over a good prior reading.
    fn has_window(&self) -> bool {
        self.five_hour.is_some() || self.seven_day.is_some()
    }
}

#[derive(Debug, Clone, Default)]
pub struct CodexLimits {
    pub weekly: Option<f64>,
    pub weekly_reset: Option<i64>,
}

impl CodexLimits {
    fn has_window(&self) -> bool {
        self.weekly.is_some()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Stale {
    pub claude: bool,
    pub codex: bool,
}

/// Last-known-good cache. Each provider persists to its own file (`~/.cache/copad/usage-limits-{claude,codex}.json`)
/// so independent refreshes never clobber each other.
#[derive(Default)]
struct Cache {
    claude: Option<ClaudeCache>,
    codex: Option<CodexCache>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct ClaudeCache {
    ts: i64,
    five_hour: Option<f64>,
    seven_day: Option<f64>,
    // Absent in caches written before resets were stored; serde defaults an absent `Option` to `None`.
    five_hour_reset: Option<i64>,
    seven_day_reset: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct CodexCache {
    ts: i64,
    weekly: Option<f64>,
    weekly_reset: Option<i64>,
}

trait Stamped {
    fn ts(&self) -> i64;
}
impl Stamped for ClaudeCache {
    fn ts(&self) -> i64 {
        self.ts
    }
}
impl Stamped for CodexCache {
    fn ts(&self) -> i64 {
        self.ts
    }
}

/// The second element is diagnostics for each requested provider that came back empty; coctl prints them to
/// stderr, comux ignores them.
pub fn collect(home: &str, want_claude: bool, want_codex: bool) -> (Limits, Vec<String>) {
    let mut diags = Vec::new();
    // A parsed-but-empty result (200 with no window fields) counts as unavailable: caching it would
    // overwrite a good entry with `None`s and defeat the stale fallback.
    let claude = if want_claude {
        match claude_limits(home) {
            Ok(c) if c.has_window() => Some(c),
            Ok(_) => {
                diags.push("claude limits unavailable — response had no window data".into());
                None
            }
            Err(why) => {
                diags.push(format!("claude limits unavailable — {why}"));
                None
            }
        }
    } else {
        None
    };
    let codex = if want_codex {
        match codex_limits(home) {
            Ok(x) if x.has_window() => Some(x),
            Ok(_) => {
                diags.push("codex limits unavailable — snapshot had no weekly window".into());
                None
            }
            Err(why) => {
                diags.push(format!("codex limits unavailable — {why}"));
                None
            }
        }
    } else {
        None
    };
    (Limits { claude, codex }, diags)
}

pub fn resolve(home: &str, want_claude: bool, want_codex: bool) -> (Limits, Stale, Vec<String>) {
    let (mut live, diags) = collect(home, want_claude, want_codex);
    let now = Local::now().timestamp_millis();
    let mut cache = Cache {
        claude: if want_claude {
            load_json(&claude_cache_path(home))
        } else {
            None
        },
        codex: if want_codex {
            load_json(&codex_cache_path(home))
        } else {
            None
        },
    };
    let stale = apply_cache(&mut live, &mut cache, now, want_claude, want_codex);
    // Persist only providers freshly fetched (not stale-filled), each to its own file, so a claude write never
    // clobbers a concurrent codex write and a merely-loaded cache is never rewritten over a fresher one.
    // `save_json` is monotonic, so a delayed same-provider write can't regress it.
    if live.claude.is_some()
        && !stale.claude
        && let Some(c) = &cache.claude
    {
        save_json(&claude_cache_path(home), c);
    }
    if live.codex.is_some()
        && !stale.codex
        && let Some(x) = &cache.codex
    {
        save_json(&codex_cache_path(home), x);
    }
    (live, stale, diags)
}

fn apply_cache(
    live: &mut Limits,
    cache: &mut Cache,
    now: i64,
    want_claude: bool,
    want_codex: bool,
) -> Stale {
    let mut stale = Stale::default();
    match &live.claude {
        Some(c) => {
            cache.claude = Some(ClaudeCache {
                ts: now,
                five_hour: c.five_hour,
                seven_day: c.seven_day,
                five_hour_reset: c.five_hour_reset,
                seven_day_reset: c.seven_day_reset,
            });
        }
        None if want_claude => {
            if let Some(cc) = &cache.claude
                && fresh_enough(now, cc.ts)
                && (cc.five_hour.is_some() || cc.seven_day.is_some())
            {
                live.claude = Some(ClaudeLimits {
                    five_hour: cc.five_hour,
                    seven_day: cc.seven_day,
                    five_hour_reset: cc.five_hour_reset,
                    seven_day_reset: cc.seven_day_reset,
                });
                stale.claude = true;
            }
        }
        None => {}
    }
    match &live.codex {
        Some(x) => {
            cache.codex = Some(CodexCache {
                ts: now,
                weekly: x.weekly,
                weekly_reset: x.weekly_reset,
            });
        }
        None if want_codex => {
            if let Some(xc) = &cache.codex
                && fresh_enough(now, xc.ts)
                && xc.weekly.is_some()
            {
                live.codex = Some(CodexLimits {
                    weekly: xc.weekly,
                    weekly_reset: xc.weekly_reset,
                });
                stale.codex = true;
            }
        }
        None => {}
    }
    stale
}

/// `saturating_sub` keeps a garbage `i64::MIN` timestamp from panicking; the `0..=` bound rejects a
/// future timestamp (clock skew, corruption) instead of treating its negative age as fresh.
fn fresh_enough(now: i64, ts: i64) -> bool {
    (0..=STALE_MAX_MS).contains(&now.saturating_sub(ts))
}

fn claude_cache_path(home: &str) -> PathBuf {
    Path::new(home).join(".cache/copad/usage-limits-claude.json")
}

fn codex_cache_path(home: &str) -> PathBuf {
    Path::new(home).join(".cache/copad/usage-limits-codex.json")
}

fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let s = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&s).ok()
}

/// Best-effort atomic, monotonic write. The temp file is per-process so concurrent writers never share an
/// inode. Skips the write when the file already holds a newer entry, so a delayed process can't regress a
/// fresher value; the residual race window self-heals next poll. Failures are ignored: the cache is an optimization.
fn save_json<T: Serialize + serde::de::DeserializeOwned + Stamped>(path: &Path, val: &T) {
    if let Some(existing) = load_json::<T>(path)
        && existing.ts() > val.ts()
    {
        return;
    }
    let Ok(json) = serde_json::to_string(val) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn claude_limits(home: &str) -> Result<ClaudeLimits, String> {
    let creds = load_claude_credentials(home)?;
    let oauth = creds
        .get("claudeAiOauth")
        .ok_or("no `claudeAiOauth` in credentials (not logged in?)")?;
    let token = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or("no `accessToken` in credentials")?;

    // `expiresAt` is epoch-millis. A live call with an expired token just 401s;
    // short-circuit so we don't spend a request (and a timeout) to learn that.
    if let Some(exp) = oauth.get("expiresAt").and_then(Value::as_i64) {
        let now = Local::now().timestamp_millis();
        if now >= exp {
            // saturating so a garbage `expiresAt` (e.g. i64::MIN) can't overflow.
            let mins = now.saturating_sub(exp) / 60_000;
            return Err(format!(
                "OAuth token expired {mins} min ago — run Claude Code to refresh it"
            ));
        }
    }

    // One client for the process: comux asks every minute, and building one each time
    // reloaded the platform root certificates for every request.
    static CLIENT: OnceLock<Result<reqwest::blocking::Client, String>> = OnceLock::new();
    let client = CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(6))
                .build()
                .map_err(|e| format!("http client: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("Authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .send()
        .map_err(|e| format!("request failed (network/TLS?): {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status} from /api/oauth/usage"));
    }
    let body: Value = resp.json().map_err(|e| format!("response not JSON: {e}"))?;
    Ok(parse_claude_usage(&body))
}

fn load_claude_credentials(home: &str) -> Result<Value, String> {
    let path = Path::new(home).join(".claude/.credentials.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(file_err) => claude_keychain_credentials()
            .ok_or_else(|| format!("cannot read {}: {file_err}", path.display()))?,
    };
    serde_json::from_str(&raw).map_err(|e| format!("credentials not valid JSON: {e}"))
}

/// Reads the Claude OAuth credentials blob from the macOS login Keychain. The item is ACL-restricted: a
/// first read from a new binary may prompt in a GUI session, and fails silently when denied or headless.
#[cfg(target_os = "macos")]
fn claude_keychain_credentials() -> Option<String> {
    let out = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let blob = String::from_utf8(out.stdout).ok()?;
    let blob = blob.trim();
    (!blob.is_empty()).then(|| blob.to_string())
}

#[cfg(not(target_os = "macos"))]
fn claude_keychain_credentials() -> Option<String> {
    None
}

fn parse_claude_usage(body: &Value) -> ClaudeLimits {
    let pct = |k: &str| {
        body.get(k)
            .and_then(|w| w.get("utilization"))
            .and_then(Value::as_f64)
    };
    let reset = |k: &str| {
        body.get(k)
            .and_then(|w| w.get("resets_at"))
            .and_then(parse_epoch)
    };
    ClaudeLimits {
        five_hour: pct("five_hour"),
        seven_day: pct("seven_day"),
        five_hour_reset: reset("five_hour"),
        seven_day_reset: reset("seven_day"),
    }
}

/// Accepts an integer/float epoch or an RFC3339 string; anything else yields `None`.
fn parse_epoch(v: &Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(f) = v.as_f64() {
        return Some(f as i64);
    }
    if let Some(s) = v.as_str() {
        return chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.timestamp());
    }
    None
}

fn codex_limits(home: &str) -> Result<CodexLimits, String> {
    let root = Path::new(home).join(".codex/sessions");
    let newest = newest_rollout(&root)
        .ok_or_else(|| format!("no rollout files under {}", root.display()))?;
    let mut file =
        std::fs::File::open(&newest).map_err(|e| format!("cannot read newest rollout: {e}"))?;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    // The snapshot sits near the end of a rollout that can run to tens of MB, read every
    // minute: look in a growing tail before ever reading the whole file.
    let mut window = TAIL_START.min(len);
    loop {
        let mut buf = Vec::with_capacity(window as usize);
        file.seek(SeekFrom::Start(len - window))
            .and_then(|_| file.by_ref().take(window).read_to_end(&mut buf))
            .map_err(|e| format!("cannot read newest rollout: {e}"))?;
        // The window's first line is usually cut mid-way; only a whole line can be a snapshot.
        let text = String::from_utf8_lossy(&buf);
        let whole = if window < len {
            text.split_once('\n').map_or("", |(_, rest)| rest)
        } else {
            &text
        };
        if let Some(rl) = last_rate_limits(whole) {
            return Ok(rl);
        }
        if window >= len || window >= TAIL_MAX {
            break;
        }
        window = (window * 2).min(len).min(TAIL_MAX);
    }
    Err("newest rollout has no rate_limits snapshot yet".into())
}

const TAIL_START: u64 = 256 * 1024;
const TAIL_MAX: u64 = 8 * 1024 * 1024;

fn last_rate_limits(text: &str) -> Option<CodexLimits> {
    text.lines()
        .rev()
        .filter(|l| l.contains("\"rate_limits\""))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find_map(|v| find_key(&v, "rate_limits").map(parse_codex_rate_limits))
}

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

/// Codex reports 5h + weekly as primary/secondary in no fixed order, so pick by `window_minutes`.
fn parse_codex_rate_limits(rl: &Value) -> CodexLimits {
    const DAY_MINUTES: f64 = 1440.0;
    let mut weekly = None;
    let mut weekly_reset = None;
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
            weekly_reset = w.get("resets_at").and_then(parse_epoch);
        }
    }
    CodexLimits {
        weekly,
        weekly_reset,
    }
}

pub fn oneline(l: &Limits, stale: &Stale) -> String {
    let mut parts = Vec::new();
    if let Some(c) = &l.claude {
        let mut seg = String::new();
        if let Some(p) = c.five_hour {
            seg.push_str(&format!(" 5h {p:.0}%"));
        }
        if let Some(p) = c.seven_day {
            seg.push_str(&format!(" wk {p:.0}%"));
        }
        if !seg.is_empty() {
            let mark = if stale.claude { "~" } else { "" };
            parts.push(format!("{mark}claude{seg}"));
        }
    }
    if let Some(x) = &l.codex
        && let Some(p) = x.weekly
    {
        let mark = if stale.codex { "~" } else { "" };
        parts.push(format!("{mark}codex wk {p:.0}%"));
    }
    if parts.is_empty() {
        return "no limits".to_string();
    }
    parts.join(" · ")
}

/// Unavailable providers/windows are omitted, not `null`. A cache-served provider carries `"stale": true`.
pub fn to_json(l: &Limits, stale: &Stale) -> Value {
    let mut root = serde_json::Map::new();
    if let Some(c) = &l.claude {
        let mut m = serde_json::Map::new();
        if let Some(p) = c.five_hour {
            m.insert("five_hour".into(), json!(p));
        }
        if let Some(r) = c.five_hour_reset {
            m.insert("five_hour_reset".into(), json!(r));
        }
        if let Some(p) = c.seven_day {
            m.insert("seven_day".into(), json!(p));
        }
        if let Some(r) = c.seven_day_reset {
            m.insert("seven_day_reset".into(), json!(r));
        }
        if !m.is_empty() {
            if stale.claude {
                m.insert("stale".into(), json!(true));
            }
            root.insert("claude".into(), Value::Object(m));
        }
    }
    if let Some(x) = &l.codex
        && let Some(p) = x.weekly
    {
        let mut m = serde_json::Map::new();
        m.insert("weekly".into(), json!(p));
        if let Some(r) = x.weekly_reset {
            m.insert("weekly_reset".into(), json!(r));
        }
        if stale.codex {
            m.insert("stale".into(), json!(true));
        }
        root.insert("codex".into(), Value::Object(m));
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
        assert_eq!(c.five_hour_reset, None);
    }

    #[test]
    fn parses_claude_reset() {
        let body = json!({
            "five_hour": {"utilization": 5.0, "resets_at": 1_786_160_089_i64},
            "seven_day": {"utilization": 27.0, "resets_at": "2026-08-06T00:00:00Z"},
        });
        let c = parse_claude_usage(&body);
        assert_eq!(c.five_hour_reset, Some(1_786_160_089));
        assert_eq!(c.seven_day_reset, Some(1_785_974_400));
    }

    #[test]
    fn parse_epoch_accepts_int_string_and_rejects_junk() {
        assert_eq!(parse_epoch(&json!(1_786_160_089_i64)), Some(1_786_160_089));
        assert_eq!(parse_epoch(&json!(1_786_160_089.0)), Some(1_786_160_089));
        assert_eq!(
            parse_epoch(&json!("2026-08-06T00:00:00Z")),
            Some(1_785_974_400)
        );
        assert_eq!(parse_epoch(&json!("...")), None);
        assert_eq!(parse_epoch(&json!(null)), None);
    }

    #[test]
    fn claude_body_missing_window_is_none() {
        let body = json!({ "five_hour": {"utilization": 5.0} });
        let c = parse_claude_usage(&body);
        assert_eq!(c.five_hour, Some(5.0));
        assert_eq!(c.seven_day, None);
        assert_eq!(c.five_hour_reset, None);
    }

    #[test]
    fn load_claude_credentials_reads_the_file_first() {
        let dir = std::env::temp_dir().join(format!("copad-cli-creds-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join(".claude"));
        let creds = r#"{"claudeAiOauth":{"accessToken":"tok","expiresAt":9999999999999}}"#;
        std::fs::write(dir.join(".claude/.credentials.json"), creds).unwrap();
        let v = load_claude_credentials(dir.to_str().unwrap()).unwrap();
        assert_eq!(v["claudeAiOauth"]["accessToken"], "tok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Non-macOS only: on macOS this would hit the real login Keychain, which can block on an ACL prompt.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn load_claude_credentials_errors_when_absent() {
        let missing = std::env::temp_dir().join(format!("copad-cli-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let err = load_claude_credentials(missing.to_str().unwrap()).unwrap_err();
        assert!(err.contains("cannot read"));
    }

    #[test]
    fn codex_snapshot_is_found_in_the_tail_or_deeper() {
        let home = std::env::temp_dir().join(format!("copad-usage-tail-{}", std::process::id()));
        let dir = home.join(".codex/sessions/2026/10/28");
        let _ = std::fs::create_dir_all(&dir);
        let snap = |pct: f64| {
            json!({"payload": {"rate_limits": {
                "secondary": {"used_percent": pct, "window_minutes": 10080}
            }}})
            .to_string()
        };
        let filler = format!("{{\"t\":\"{}\"}}\n", "x".repeat(200)).repeat(3_000);
        let file = dir.join("rollout-a.jsonl");
        std::fs::write(&file, format!("{}\n{filler}{}\n", snap(10.0), snap(42.0))).unwrap();
        let h = home.to_str().unwrap();
        assert_eq!(codex_limits(h).unwrap().weekly, Some(42.0));
        std::fs::write(&file, format!("{}\n{filler}", snap(7.0))).unwrap();
        assert_eq!(codex_limits(h).unwrap().weekly, Some(7.0));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn picks_weekly_window_by_width() {
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
    fn codex_captures_weekly_reset() {
        let rl = json!({
            "primary": {"used_percent": 1.0, "window_minutes": 10080, "resets_at": 1_786_160_089_i64},
            "secondary": null,
        });
        let x = parse_codex_rate_limits(&rl);
        assert_eq!(x.weekly, Some(1.0));
        assert_eq!(x.weekly_reset, Some(1_786_160_089));
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
                ..Default::default()
            }),
            codex: Some(CodexLimits {
                weekly: Some(45.0),
                ..Default::default()
            }),
        };
        assert_eq!(
            oneline(&l, &Stale::default()),
            "claude 5h 5% wk 27% · codex wk 45%"
        );
    }

    #[test]
    fn oneline_claude_only() {
        let l = Limits {
            claude: Some(ClaudeLimits {
                five_hour: Some(80.0),
                seven_day: None,
                ..Default::default()
            }),
            codex: None,
        };
        assert_eq!(oneline(&l, &Stale::default()), "claude 5h 80%");
    }

    #[test]
    fn json_omits_unavailable_providers_and_windows() {
        let l = Limits {
            claude: None,
            codex: Some(CodexLimits {
                weekly: Some(45.0),
                ..Default::default()
            }),
        };
        let v = to_json(&l, &Stale::default());
        assert!(v.get("claude").is_none());
        assert_eq!(v["codex"]["weekly"], json!(45.0));

        let l = Limits {
            claude: Some(ClaudeLimits {
                five_hour: Some(6.0),
                seven_day: None,
                ..Default::default()
            }),
            codex: None,
        };
        let v = to_json(&l, &Stale::default());
        assert_eq!(v["claude"]["five_hour"], json!(6.0));
        assert!(v["claude"].get("seven_day").is_none());
        assert!(v.get("codex").is_none());

        assert_eq!(to_json(&Limits::default(), &Stale::default()), json!({}));
    }

    #[test]
    fn cache_backfills_missing_claude_and_marks_stale() {
        let mut live = Limits {
            claude: None,
            codex: Some(CodexLimits {
                weekly: Some(50.0),
                ..Default::default()
            }),
        };
        let mut cache = Cache {
            claude: Some(ClaudeCache {
                ts: 1000,
                five_hour: Some(6.0),
                seven_day: Some(27.0),
                ..Default::default()
            }),
            codex: None,
        };
        let stale = apply_cache(&mut live, &mut cache, 1000 + STALE_MAX_MS, true, true);
        assert!(stale.claude, "claude came from cache");
        assert!(!stale.codex, "codex was live");
        assert_eq!(live.claude.as_ref().unwrap().five_hour, Some(6.0));
        assert_eq!(
            oneline(&live, &stale),
            "~claude 5h 6% wk 27% · codex wk 50%"
        );
        assert_eq!(cache.codex.as_ref().unwrap().ts, 1000 + STALE_MAX_MS);
    }

    #[test]
    fn cache_expires_past_max_age() {
        let mut live = Limits::default();
        let mut cache = Cache {
            claude: Some(ClaudeCache {
                ts: 1000,
                five_hour: Some(6.0),
                seven_day: None,
                ..Default::default()
            }),
            codex: None,
        };
        let stale = apply_cache(&mut live, &mut cache, 1000 + STALE_MAX_MS + 1, true, true);
        assert!(!stale.claude);
        assert!(live.claude.is_none());
    }

    #[test]
    fn empty_result_has_no_window() {
        assert!(!ClaudeLimits::default().has_window());
        assert!(
            ClaudeLimits {
                five_hour: Some(1.0),
                seven_day: None,
                ..Default::default()
            }
            .has_window()
        );
        assert!(!CodexLimits::default().has_window());
        assert!(
            CodexLimits {
                weekly: Some(1.0),
                ..Default::default()
            }
            .has_window()
        );
    }

    #[test]
    fn stamped_reports_ts() {
        let c = ClaudeCache {
            ts: 42,
            five_hour: None,
            seven_day: None,
            ..Default::default()
        };
        assert_eq!(c.ts(), 42);
        assert_eq!(
            CodexCache {
                ts: 7,
                weekly: None,
                ..Default::default()
            }
            .ts(),
            7
        );
    }

    #[test]
    fn fresh_enough_rejects_future_and_garbage_timestamps() {
        let now = 1_000_000_000_000;
        assert!(fresh_enough(now, now));
        assert!(fresh_enough(now, now - STALE_MAX_MS));
        assert!(!fresh_enough(now, now - STALE_MAX_MS - 1));
        assert!(!fresh_enough(now, now + 60_000));
        assert!(!fresh_enough(now, i64::MIN));
        assert!(!fresh_enough(now, i64::MAX));
    }

    #[test]
    fn cache_not_consulted_for_unrequested_tool() {
        let mut live = Limits::default();
        let mut cache = Cache {
            claude: Some(ClaudeCache {
                ts: 1000,
                five_hour: Some(6.0),
                seven_day: None,
                ..Default::default()
            }),
            codex: None,
        };
        let stale = apply_cache(&mut live, &mut cache, 1000, false, true);
        assert!(!stale.claude);
        assert!(live.claude.is_none());
    }

    #[test]
    fn json_marks_stale_provider() {
        let l = Limits {
            claude: Some(ClaudeLimits {
                five_hour: Some(6.0),
                seven_day: None,
                ..Default::default()
            }),
            codex: None,
        };
        let stale = Stale {
            claude: true,
            codex: false,
        };
        let v = to_json(&l, &stale);
        assert_eq!(v["claude"]["stale"], json!(true));
    }

    #[test]
    fn oneline_empty() {
        assert_eq!(oneline(&Limits::default(), &Stale::default()), "no limits");
        let l = Limits {
            claude: Some(ClaudeLimits::default()),
            codex: None,
        };
        assert_eq!(oneline(&l, &Stale::default()), "no limits");
    }
}
