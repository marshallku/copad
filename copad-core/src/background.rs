//! Wallpaper-rotation primitives. Both Linux (`socket.rs`'s
//! `background.next`/`background.toggle` actions) and macOS
//! (`BackgroundRotator`) read the same `wallpapers.txt` flat-file list and
//! `bg-mode` flag, but the paths differ per platform (Linux's `~/.cache`
//! XDG vs macOS's `~/Library/Caches/copad`). Callers pass the resolved
//! paths in; core handles the rest.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// File-system locations the rotator reads/writes. `fallback_list` lets a
/// platform try a native path first (e.g. macOS `~/Library/Caches/copad/`)
/// and fall through to the cross-platform XDG location for users who share
/// a single wallpapers.txt across machines.
#[derive(Debug, Clone)]
pub struct BackgroundPaths {
    pub primary_list: PathBuf,
    pub fallback_list: Option<PathBuf>,
    pub mode_file: PathBuf,
}

/// Pick a random image path from the configured list. Returns None when
/// neither file exists, both are empty, or every line is blank. Doesn't
/// gate on `is_active` — the caller decides whether deactive rotation
/// should suppress the call (matches Linux's existing
/// `background.next` socket handler semantics).
pub fn pick_random(paths: &BackgroundPaths) -> Option<String> {
    let contents = read_list(&paths.primary_list)
        .or_else(|| paths.fallback_list.as_deref().and_then(read_list))?;
    // Preserve verbatim line content (matches Linux's prior
    // `socket.rs::select_random_image` semantics — paths can legally
    // contain leading/trailing spaces). Only skip empty-after-LF
    // entries; `lines()` already drops the trailing newline.
    let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
    // Same poor-man's entropy Linux's socket.rs has used for ages —
    // wallpaper rotation cadence is on the order of seconds, so subsec
    // nanos give enough jitter without pulling in a `rand` dep.
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize;
    Some(lines[seed % lines.len()].to_string())
}

/// True if rotation is active. Missing mode file = active (default).
pub fn is_active(mode_file: &Path) -> bool {
    match std::fs::read_to_string(mode_file) {
        Ok(s) => s.trim() != "deactive",
        Err(_) => true,
    }
}

/// Flip the mode bit and persist. Returns the new state. Creates the
/// parent directory if missing — a fresh install won't have it.
pub fn toggle(mode_file: &Path) -> bool {
    let new_active = !is_active(mode_file);
    let mode = if new_active { "active" } else { "deactive" };
    if let Some(parent) = mode_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(mode_file, mode);
    new_active
}

fn read_list(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpfile(name: &str, body: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("copad-bg-test-{name}-{}", std::process::id()));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn pick_random_returns_none_for_missing_primary_no_fallback() {
        let paths = BackgroundPaths {
            primary_list: PathBuf::from("/nonexistent/x"),
            fallback_list: None,
            mode_file: PathBuf::from("/nonexistent/m"),
        };
        assert!(pick_random(&paths).is_none());
    }

    #[test]
    fn pick_random_falls_through_to_fallback() {
        let fb = tmpfile("fallback", "/a.png\n/b.png\n");
        let paths = BackgroundPaths {
            primary_list: PathBuf::from("/nonexistent/x"),
            fallback_list: Some(fb.clone()),
            mode_file: PathBuf::from("/nonexistent/m"),
        };
        let picked = pick_random(&paths).expect("fallback should yield a line");
        assert!(picked == "/a.png" || picked == "/b.png");
        let _ = std::fs::remove_file(fb);
    }

    #[test]
    fn pick_random_skips_empty_lines() {
        // Empty-after-LF entries drop, but whitespace-only lines pass
        // through verbatim — paths with leading/trailing spaces are
        // valid POSIX filenames, and Linux's prior
        // `socket.rs::select_random_image` preserves them. Single
        // non-empty entry → that's what we get.
        let p = tmpfile("blanks", "\n\n/only.png\n\n");
        let paths = BackgroundPaths {
            primary_list: p.clone(),
            fallback_list: None,
            mode_file: PathBuf::from("/nonexistent/m"),
        };
        assert_eq!(pick_random(&paths), Some("/only.png".to_string()));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn is_active_defaults_true_for_missing_file() {
        assert!(is_active(Path::new("/nonexistent/mode")));
    }

    #[test]
    fn toggle_flips_and_persists() {
        let m = std::env::temp_dir().join(format!("copad-bg-mode-{}", std::process::id()));
        let _ = std::fs::remove_file(&m);
        // Missing file: defaults to active → toggle should write "deactive"
        let after_first = toggle(&m);
        assert!(!after_first);
        assert!(!is_active(&m));
        let after_second = toggle(&m);
        assert!(after_second);
        assert!(is_active(&m));
        let _ = std::fs::remove_file(&m);
    }
}
