//! Durable durations for the Pomodoro plugin.
//!
//! Only the *configuration* (work/break/long-break minutes + round count)
//! is persisted — NOT the in-flight session. A daemon restart therefore
//! comes back Idle, which avoids the whole class of stale-toast / wall-clock
//! catch-up problems that restoring a live `ends_at` would introduce
//! (decision: keep restart semantics trivially safe; live-session resume is
//! a deliberate non-goal). copad has no config.toml writer, so this file is
//! where a right-click "set minutes" change lands — the plugin owns it, the
//! way the todo plugin owns its markdown files.
//!
//! Writes are atomic (temp in the same dir → fsync → rename over the
//! target) and mode 0600. A missing or corrupt file silently falls back to
//! defaults so a bad edit can never wedge startup.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::Durations;

const STATE_VERSION: u32 = 1;

/// `$COPAD_POMODORO_STATE` (tests / power users) else
/// `<state_dir>/pomodoro.json`.
pub fn state_file_path() -> PathBuf {
    if let Ok(p) = std::env::var("COPAD_POMODORO_STATE")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    copad_core::paths::state_dir().join("pomodoro.json")
}

#[derive(Serialize, Deserialize)]
struct Persisted {
    version: u32,
    work_min: u32,
    break_min: u32,
    long_break_min: u32,
    rounds_before_long: u32,
}

/// Load persisted durations, or [`Durations::default`] on any problem
/// (absent file, unreadable, bad JSON, unknown version, out-of-range
/// values). Never fails — startup must always proceed.
pub fn load_durations() -> Durations {
    load_durations_from(&state_file_path())
}

/// Atomically persist durations (see [`save_durations_to`]).
pub fn save_durations(d: &Durations) -> io::Result<()> {
    save_durations_to(&state_file_path(), d)
}

/// Path-injected core of [`load_durations`] — keeps the tests off the
/// process-global `COPAD_POMODORO_STATE` env var so they stay parallel-safe.
fn load_durations_from(path: &Path) -> Durations {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(_) => return Durations::default(),
    };
    let parsed: Persisted = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[pomodoro] ignoring corrupt state {}: {e}", path.display());
            return Durations::default();
        }
    };
    if parsed.version != STATE_VERSION {
        eprintln!(
            "[pomodoro] state {} has unknown version {}; using defaults",
            path.display(),
            parsed.version
        );
        return Durations::default();
    }
    match Durations::validated(
        parsed.work_min,
        parsed.break_min,
        parsed.long_break_min,
        parsed.rounds_before_long,
    ) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "[pomodoro] state {} out of range ({e}); using defaults",
                path.display()
            );
            Durations::default()
        }
    }
}

/// Atomically persist durations to `path`: write a sibling temp file, fsync
/// it, then rename over the target (POSIX rename is atomic and replaces).
fn save_durations_to(path: &Path, d: &Durations) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let record = Persisted {
        version: STATE_VERSION,
        work_min: d.work_min,
        break_min: d.break_min,
        long_break_min: d.long_break_min,
        rounds_before_long: d.rounds_before_long,
    };
    let json = serde_json::to_vec_pretty(&record)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    // Clean up any stale temp from a previous crashed write of this pid
    // (pid reuse) before creating ours.
    let _ = fs::remove_file(&tmp);
    {
        let mut f: File = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&json)?;
        f.flush()?;
        f.sync_all()?;
    }
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test owns a unique tempdir + path, passed directly to the
    // path-injected core — no shared env var, so tests run in parallel
    // without clobbering each other.
    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("copad-pomo-{}-{}", std::process::id(), name));
        let _ = fs::create_dir_all(&dir);
        dir.join("pomodoro.json")
    }

    #[test]
    fn missing_file_yields_defaults() {
        let p = temp_path("missing");
        let _ = fs::remove_file(&p);
        assert_eq!(load_durations_from(&p), Durations::default());
    }

    #[test]
    fn round_trips() {
        let p = temp_path("round");
        let d = Durations::validated(50, 10, 30, 3).unwrap();
        save_durations_to(&p, &d).unwrap();
        assert_eq!(load_durations_from(&p), d);
    }

    #[test]
    fn corrupt_file_falls_back() {
        let p = temp_path("corrupt");
        fs::write(&p, b"{ not json").unwrap();
        assert_eq!(load_durations_from(&p), Durations::default());
    }

    #[test]
    fn out_of_range_falls_back() {
        let p = temp_path("range");
        fs::write(
            &p,
            br#"{"version":1,"work_min":9999,"break_min":5,"long_break_min":15,"rounds_before_long":4}"#,
        )
        .unwrap();
        assert_eq!(load_durations_from(&p), Durations::default());
    }

    #[test]
    fn unknown_version_falls_back() {
        let p = temp_path("ver");
        fs::write(
            &p,
            br#"{"version":999,"work_min":50,"break_min":10,"long_break_min":30,"rounds_before_long":3}"#,
        )
        .unwrap();
        assert_eq!(load_durations_from(&p), Durations::default());
    }
}
