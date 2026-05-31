//! Phase 24.6 — orchestration cockpit data layer.
//!
//! Composes the three orchestration surfaces decision #51 keeps separate:
//! the pilot goal queue (`pilot.status`, the source of truth, fetched via
//! the daemon RPC in `main.rs`), the live `csd ps --json` driven-session
//! view, and the `tmx agents --json` observation snapshot. pilot owns the
//! queue, `csd` drives, `tmx` observes — web-bridge only aggregates, it
//! does not reimplement any of them (the `agents::read_snapshot` tmx
//! shell-out is the established precedent).

use std::process::Command;

use serde_json::{Value, json};

/// `csd ps --json` → its parsed JSON (`{ sessions: [...] }`). Tries the
/// configured binary then the usual install locations, mirroring
/// `agents::read_snapshot`'s tmx lookup — the daemon env that `copadd`
/// spawns plugins under may lack the interactive shell's PATH.
pub fn read_csd_ps() -> Result<Value, String> {
    read_csd_ps_from(&csd_candidates(std::env::var("COPAD_PILOT_CSD_BIN").ok()))
}

/// Candidate binary paths in priority order: the configured override, then
/// `csd` on PATH, then the usual install locations. Pure (the env read
/// lives in [`read_csd_ps`]) so it's testable without mutating process env.
fn csd_candidates(configured: Option<String>) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(bin) = configured.filter(|s| !s.is_empty()) {
        candidates.push(bin);
    }
    candidates.push("csd".to_string());
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/csd").to_string_lossy().into_owned());
        candidates.push(home.join(".cargo/bin/csd").to_string_lossy().into_owned());
    }
    candidates
}

fn read_csd_ps_from(candidates: &[String]) -> Result<Value, String> {
    let mut last_err = String::from("no candidate tried");
    for bin in candidates {
        match Command::new(bin).args(["ps", "--json"]).output() {
            Ok(out) if out.status.success() => {
                return serde_json::from_slice(&out.stdout)
                    .map_err(|e| format!("parse `{bin} ps --json` output: {e}"));
            }
            Ok(out) => {
                last_err = format!(
                    "{bin} exit={:?}: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Err(e) => last_err = format!("{bin}: {e}"),
        }
    }
    Err(last_err)
}

/// Build the cockpit view from the three (independently fallible) sources.
/// `pilot` is required (the daemon RPC already errored out the request if
/// it failed); `csd` and `tmx` are best-effort — a failure becomes a null
/// body plus an entry under `errors`, so the queue still renders when the
/// observation side-cars are down.
pub fn aggregate(pilot: Value, csd: Result<Value, String>, tmx: Result<Value, String>) -> Value {
    let mut errors = serde_json::Map::new();
    let csd = unwrap_or_record(csd, "csd", &mut errors);
    let tmx = unwrap_or_record(tmx, "tmx", &mut errors);
    json!({
        "pilot": pilot,
        "csd": csd,
        "tmx": tmx,
        "errors": Value::Object(errors),
    })
}

fn unwrap_or_record(
    r: Result<Value, String>,
    key: &str,
    errors: &mut serde_json::Map<String, Value>,
) -> Value {
    match r {
        Ok(v) => v,
        Err(e) => {
            errors.insert(key.to_string(), json!(e));
            Value::Null
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_passes_through_all_ok() {
        let out = aggregate(
            json!({ "goals": [], "active": null }),
            Ok(json!({ "sessions": [{ "name": "g-1" }] })),
            Ok(json!({ "agents": [] })),
        );
        assert_eq!(out["pilot"]["goals"], json!([]));
        assert_eq!(out["csd"]["sessions"][0]["name"], "g-1");
        assert_eq!(out["tmx"]["agents"], json!([]));
        assert_eq!(out["errors"], json!({}));
    }

    #[test]
    fn aggregate_degrades_on_sidecar_failure() {
        // pilot always present; csd down, tmx ok → csd nulled + recorded,
        // pilot/tmx intact (the queue still renders).
        let out = aggregate(
            json!({ "active": "g-7" }),
            Err("csd: No such file or directory".into()),
            Ok(json!({ "agents": [{ "id": "a" }] })),
        );
        assert_eq!(out["pilot"]["active"], "g-7");
        assert_eq!(out["csd"], Value::Null);
        assert!(out["errors"]["csd"].as_str().unwrap().contains("csd:"));
        assert!(out["errors"].get("tmx").is_none());
        assert_eq!(out["tmx"]["agents"][0]["id"], "a");
    }

    #[test]
    fn csd_candidates_prepends_configured_bin() {
        let c = csd_candidates(Some("/custom/csd".into()));
        assert_eq!(c[0], "/custom/csd");
        assert!(c.contains(&"csd".to_string()));
        // Empty override is ignored (falls back to PATH `csd`).
        assert_eq!(csd_candidates(Some(String::new()))[0], "csd");
        assert_eq!(csd_candidates(None)[0], "csd");
    }

    #[test]
    fn read_csd_ps_from_parses_stub_output() {
        // Drive the parse path with a stub binary — no process-env mutation
        // (csd_candidates already isolates the env read).
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("csd");
        std::fs::write(
            &stub,
            "#!/usr/bin/env bash\nprintf '{\"sessions\":[{\"name\":\"g-9\",\"alive\":true}]}\\n'\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let v = read_csd_ps_from(&[stub.to_string_lossy().into_owned()]).unwrap();
        assert_eq!(v["sessions"][0]["name"], "g-9");
        assert_eq!(v["sessions"][0]["alive"], true);
    }
}
