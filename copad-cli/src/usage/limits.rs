//! `coctl usage --limits` — subscription rate-limit window utilization.
//!
//! A DIFFERENT data source than the token/cost aggregation in this module:
//! percentages are what Claude Code's `/usage` and Codex's TUI show ("X% of
//! your 5h limit"), sourced per provider:
//!
//!   * **Claude** — a live `GET https://api.anthropic.com/api/oauth/usage`
//!     with the OAuth bearer token from `~/.claude/.credentials.json` (or, on
//!     macOS with a keychain-only login, the `Claude Code-credentials` login
//!     Keychain item). Returns `five_hour.utilization` / `seven_day.utilization`
//!     (already a percent). Skipped (→ `None`) when the token is missing or
//!     already expired — refreshing the OAuth token is Claude Code's job, not ours.
//!   * **Codex** — the newest `~/.codex/sessions/**/rollout-*.jsonl` records a
//!     `rate_limits` snapshot (`payload.rate_limits`) on every turn. Read the
//!     most recent one — no auth, no network, but only as fresh as the last
//!     Codex turn.

use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// How long a cached value may stand in for a live one. Bridges the minutes-to-
/// hours gaps where Claude's OAuth token has lapsed (Claude Code idle) so the
/// readout doesn't blink out — but old enough to be misleading past this, so
/// beyond it the provider drops rather than showing a stale percent.
const STALE_MAX_MS: i64 = 3 * 60 * 60 * 1000;

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

impl ClaudeLimits {
    /// At least one window has a value — an all-`None` result is a 200 with no
    /// usable data and must NOT be cached over a good prior reading.
    fn has_window(&self) -> bool {
        self.five_hour.is_some() || self.seven_day.is_some()
    }
}

#[derive(Debug, Clone, Default)]
pub struct CodexLimits {
    pub weekly: Option<f64>,
}

impl CodexLimits {
    fn has_window(&self) -> bool {
        self.weekly.is_some()
    }
}

/// Which providers were served from the on-disk cache (a live fetch failed) —
/// rendered with a leading `~` so a stale value never reads as current.
#[derive(Debug, Clone, Default)]
pub struct Stale {
    pub claude: bool,
    pub codex: bool,
}

/// Last-known-good cache. Each provider is persisted to its OWN file
/// (`~/.cache/copad/usage-limits-{claude,codex}.json`) so independent refreshes
/// never clobber each other; this in-memory pair just carries both for
/// `apply_cache`. Each entry has its own timestamp.
#[derive(Default)]
struct Cache {
    claude: Option<ClaudeCache>,
    codex: Option<CodexCache>,
}

#[derive(Serialize, Deserialize, Clone)]
struct ClaudeCache {
    ts: i64,
    five_hour: Option<f64>,
    seven_day: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone)]
struct CodexCache {
    ts: i64,
    weekly: Option<f64>,
}

/// A cache entry's write timestamp, for the monotonic-write guard.
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

/// Gather both providers' limits, honoring a `--tool` filter. The second tuple
/// element is human-readable diagnostics for any provider that was requested but
/// came back empty — printed to stderr by `run_limits` (comux reads only stdout,
/// so this never pollutes the status bar) so a "why is Claude missing?" is
/// answerable without a debugger.
pub fn collect(home: &str, want_claude: bool, want_codex: bool) -> (Limits, Vec<String>) {
    let mut diags = Vec::new();
    // A parsed-but-empty result (200 with no window fields) is treated as
    // unavailable, NOT a fresh value — otherwise it would overwrite a good cache
    // with `None`s and defeat the stale fallback (codex R4/C1).
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

/// Live limits, backfilled from a short-lived on-disk cache when a provider is
/// momentarily unavailable (the common case: Claude's OAuth token lapsed while
/// Claude Code sat idle — codex, a local file read, keeps working). Fresh
/// values refresh the cache; a gap is filled from it and flagged in `Stale`
/// (rendered with `~`). Diagnostics from the live attempt pass through.
pub fn resolve(home: &str, want_claude: bool, want_codex: bool) -> (Limits, Stale, Vec<String>) {
    let (mut live, diags) = collect(home, want_claude, want_codex);
    let now = Local::now().timestamp_millis();
    // Only touch a provider's cache when it was requested (`--tool`).
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
    // Persist ONLY a provider we FRESHLY fetched (`live.X` present AND not
    // stale-filled), each to its OWN file — so a claude write never clobbers a
    // concurrent codex write, and we never rewrite a cache we merely loaded (which
    // could overwrite another process's fresh value with the older one we read).
    // `save_json` is monotonic (skips if the file is already newer), so a delayed
    // same-provider write can't regress a fresher one.
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

/// Refresh the cache from any fresh values, and backfill any requested-but-
/// missing provider from a cache entry younger than [`STALE_MAX_MS`]. Pure
/// (no I/O) so the staleness policy is unit-testable.
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
            });
        }
        None if want_codex => {
            if let Some(xc) = &cache.codex
                && fresh_enough(now, xc.ts)
                && xc.weekly.is_some()
            {
                live.codex = Some(CodexLimits { weekly: xc.weekly });
                stale.codex = true;
            }
        }
        None => {}
    }
    stale
}

/// Is a cache entry stamped `ts` usable at `now`? `saturating_sub` so a garbage
/// `i64::MIN` timestamp can't panic a checked build; the `0..=` lower bound
/// rejects a FUTURE timestamp (clock skew / corruption) instead of treating its
/// negative age as fresh.
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

/// Best-effort atomic, monotonic write. The temp file is PER-PROCESS
/// (`.<pid>.tmp`) so two concurrent `coctl` invocations (the comux poller + a
/// manual run) never write the same inode; each renames its own temp over `path`
/// (rename is atomic on one filesystem, so a reader always sees a whole file).
/// Before writing we re-read the file and SKIP if it already holds a newer entry
/// — so a delayed process can't regress a fresher value another just wrote (the
/// residual window between this check and the rename leaves at most a one-poll-
/// old percent, which self-heals next cycle). Any failure is silently ignored —
/// the cache is only an optimization.
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

// ── Claude: live OAuth usage endpoint ──────────────────────────────────────

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

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(6))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
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

/// Load Claude's OAuth credentials JSON. Prefers the plaintext file
/// `~/.claude/.credentials.json`; on macOS — where Claude Code may keep the credentials
/// in the login Keychain with NO file on disk — falls back to the Keychain, which stores
/// the same JSON blob. If both fail, surfaces the file error (readout hides, as before).
fn load_claude_credentials(home: &str) -> Result<Value, String> {
    let path = Path::new(home).join(".claude/.credentials.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        // File missing/unreadable → try the Keychain (macOS keychain-only logins land here).
        Err(file_err) => claude_keychain_credentials()
            .ok_or_else(|| format!("cannot read {}: {file_err}", path.display()))?,
    };
    serde_json::from_str(&raw).map_err(|e| format!("credentials not valid JSON: {e}"))
}

/// Read the Claude OAuth credentials blob from the macOS login Keychain
/// (`security find-generic-password -s "Claude Code-credentials" -w`, the same JSON Claude
/// Code writes). `None` off macOS, or when the item is absent / access is denied / headless —
/// the Keychain item is ACL-restricted, so a first read from a new binary may prompt for
/// access in a GUI session and fails silently when denied or without a GUI.
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

fn codex_limits(home: &str) -> Result<CodexLimits, String> {
    let root = Path::new(home).join(".codex/sessions");
    let newest = newest_rollout(&root)
        .ok_or_else(|| format!("no rollout files under {}", root.display()))?;
    let bytes = std::fs::read(&newest).map_err(|e| format!("cannot read newest rollout: {e}"))?;
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
            return Ok(parse_codex_rate_limits(rl));
        }
    }
    Err("newest rollout has no rate_limits snapshot yet".into())
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
/// that are unavailable are omitted; a provider served from the cache is prefixed
/// `~` (stale); all-absent → `no limits`.
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

/// Machine shape. Mirrors `oneline`: unavailable providers/windows are OMITTED,
/// not emitted as `null` — so `--tool codex` never mentions `claude`, and a
/// provider with no populated windows drops out entirely. A cache-served
/// provider carries `"stale": true`.
pub fn to_json(l: &Limits, stale: &Stale) -> Value {
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
    }

    #[test]
    fn claude_body_missing_window_is_none() {
        let body = json!({ "five_hour": {"utilization": 5.0} });
        let c = parse_claude_usage(&body);
        assert_eq!(c.five_hour, Some(5.0));
        assert_eq!(c.seven_day, None);
    }

    #[test]
    fn load_claude_credentials_reads_the_file_first() {
        // A present, valid file is parsed directly (no Keychain needed on any platform).
        let dir = std::env::temp_dir().join(format!("copad-cli-creds-{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join(".claude"));
        let creds = r#"{"claudeAiOauth":{"accessToken":"tok","expiresAt":9999999999999}}"#;
        std::fs::write(dir.join(".claude/.credentials.json"), creds).unwrap();
        let v = load_claude_credentials(dir.to_str().unwrap()).unwrap();
        assert_eq!(v["claudeAiOauth"]["accessToken"], "tok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Non-macOS only: on macOS this would invoke the real login Keychain (`security`), which
    // can display or block on an ACL prompt during `cargo test`. Off macOS the fallback is a
    // compile-time no-op, so a missing file deterministically yields the file read error.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn load_claude_credentials_errors_when_absent() {
        let missing = std::env::temp_dir().join(format!("copad-cli-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let err = load_claude_credentials(missing.to_str().unwrap()).unwrap_err();
        assert!(err.contains("cannot read"));
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
            }),
            codex: None,
        };
        assert_eq!(oneline(&l, &Stale::default()), "claude 5h 80%");
    }

    #[test]
    fn json_omits_unavailable_providers_and_windows() {
        // --tool codex: no claude key at all (not `"claude": null`).
        let l = Limits {
            claude: None,
            codex: Some(CodexLimits { weekly: Some(45.0) }),
        };
        let v = to_json(&l, &Stale::default());
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
        let v = to_json(&l, &Stale::default());
        assert_eq!(v["claude"]["five_hour"], json!(6.0));
        assert!(v["claude"].get("seven_day").is_none());
        assert!(v.get("codex").is_none());

        // Nothing available → empty object, never `{"claude":null,"codex":null}`.
        assert_eq!(to_json(&Limits::default(), &Stale::default()), json!({}));
    }

    #[test]
    fn cache_backfills_missing_claude_and_marks_stale() {
        // Live has codex but not claude; a recent cache entry fills claude in.
        let mut live = Limits {
            claude: None,
            codex: Some(CodexLimits { weekly: Some(50.0) }),
        };
        let mut cache = Cache {
            claude: Some(ClaudeCache {
                ts: 1000,
                five_hour: Some(6.0),
                seven_day: Some(27.0),
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
        // The live codex value refreshed the cache with the current timestamp.
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
            }),
            codex: None,
        };
        // One ms past the window → not backfilled.
        let stale = apply_cache(&mut live, &mut cache, 1000 + STALE_MAX_MS + 1, true, true);
        assert!(!stale.claude);
        assert!(live.claude.is_none());
    }

    #[test]
    fn empty_result_has_no_window() {
        // A 200 with no utilization fields → not cacheable (would clobber good data).
        assert!(!ClaudeLimits::default().has_window());
        assert!(
            ClaudeLimits {
                five_hour: Some(1.0),
                seven_day: None
            }
            .has_window()
        );
        assert!(!CodexLimits::default().has_window());
        assert!(CodexLimits { weekly: Some(1.0) }.has_window());
    }

    #[test]
    fn stamped_reports_ts() {
        let c = ClaudeCache {
            ts: 42,
            five_hour: None,
            seven_day: None,
        };
        assert_eq!(c.ts(), 42);
        assert_eq!(
            CodexCache {
                ts: 7,
                weekly: None
            }
            .ts(),
            7
        );
    }

    #[test]
    fn fresh_enough_rejects_future_and_garbage_timestamps() {
        let now = 1_000_000_000_000;
        assert!(fresh_enough(now, now)); // just now
        assert!(fresh_enough(now, now - STALE_MAX_MS)); // exactly the boundary
        assert!(!fresh_enough(now, now - STALE_MAX_MS - 1)); // just too old
        assert!(!fresh_enough(now, now + 60_000)); // future ts → rejected, not "fresh"
        assert!(!fresh_enough(now, i64::MIN)); // garbage → no panic, rejected
        assert!(!fresh_enough(now, i64::MAX)); // garbage future → rejected
    }

    #[test]
    fn cache_not_consulted_for_unrequested_tool() {
        let mut live = Limits::default();
        let mut cache = Cache {
            claude: Some(ClaudeCache {
                ts: 1000,
                five_hour: Some(6.0),
                seven_day: None,
            }),
            codex: None,
        };
        // want_claude=false → even a fresh cache entry is ignored.
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
        // A claude struct with no populated windows also collapses to empty.
        let l = Limits {
            claude: Some(ClaudeLimits::default()),
            codex: None,
        };
        assert_eq!(oneline(&l, &Stale::default()), "no limits");
    }
}
