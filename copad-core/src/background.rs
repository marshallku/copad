//! Wallpaper-rotation primitives shared by Linux and macOS, which read the same `wallpapers.txt`
//! list and `bg-mode` flag from different platform paths. Callers pass resolved paths in.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// File-system locations the rotator reads/writes. `fallback_list` lets a platform try a native
/// path first and fall back to the cross-platform XDG location, for users sharing one
/// wallpapers.txt across machines.
#[derive(Debug, Clone)]
pub struct BackgroundPaths {
    pub primary_list: PathBuf,
    pub fallback_list: Option<PathBuf>,
    pub mode_file: PathBuf,
}

/// A directory used as the wallpaper source (`[background] image` pointing at a directory);
/// bypasses the list file entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirSource {
    pub root: PathBuf,
    pub recursive: bool,
    pub extensions: Vec<String>,
}

impl DirSource {
    /// Strips a leading dot, lowercases, drops blanks, so `[".JPG"]` and `["jpg"]` match alike.
    pub fn new(root: PathBuf, recursive: bool, extensions: &[String]) -> Self {
        let extensions = extensions
            .iter()
            .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        Self {
            root,
            recursive,
            extensions,
        }
    }

    fn accepts(&self, path: &Path) -> bool {
        if self.extensions.is_empty() {
            return true;
        }
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| self.extensions.contains(&e.to_ascii_lowercase()))
    }
}

/// Bounds a scan against a pathological tree or a symlink cycle through a bind mount.
const MAX_SCAN_DEPTH: usize = 16;

/// Pick a random image from the configured list; `None` when neither file exists or all lines
/// are blank. Does not gate on `is_active`; the caller decides.
pub fn pick_random(paths: &BackgroundPaths) -> Option<String> {
    pick_one(&list_entries(paths)).cloned()
}

/// Every usable list line in file order, split out of [`pick_random`] so callers can exclude
/// entries (e.g. known missing) before picking. Content is verbatim, since paths can have
/// leading/trailing spaces; only empty lines drop.
pub fn list_entries(paths: &BackgroundPaths) -> Vec<String> {
    read_list(&paths.primary_list)
        .or_else(|| paths.fallback_list.as_deref().and_then(read_list))
        .map(|contents| {
            contents
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Walk `source` and collect matching image files, sorted so the result is stable (readdir
/// order is filesystem-dependent). Only real directories are descended into: `file_type` does
/// not follow symlinks, so a symlinked directory is skipped and cannot form a cycle. Symlinked
/// files are still collected. Unreadable subdirectories are skipped; only an unreadable root is
/// an error.
pub fn scan_dir(source: &DirSource) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut stack = vec![(source.root.clone(), 0usize)];
    let mut first = true;

    while let Some((dir, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if first => return Err(e),
            Err(_) => continue,
        };
        first = false;

        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                if source.recursive && depth + 1 < MAX_SCAN_DEPTH {
                    stack.push((path, depth + 1));
                }
            } else if source.accepts(&path) {
                found.push(path);
            }
        }
    }

    found.sort();
    Ok(found)
}

pub fn pick_one<T>(entries: &[T]) -> Option<&T> {
    if entries.is_empty() {
        return None;
    }
    // Subsec nanos give enough jitter for a seconds-scale rotation without a `rand` dep.
    let seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize;
    entries.get(seed % entries.len())
}

/// Write `entries` one per line, creating the parent directory. Temp + rename so an interrupted
/// write can't truncate a list the GUI is reading. Returns lines written; non-UTF-8 paths are
/// skipped since the line format cannot represent them.
pub fn write_list(list: &Path, entries: &[PathBuf]) -> std::io::Result<usize> {
    if let Some(parent) = list.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lines: Vec<&str> = entries.iter().filter_map(|p| p.to_str()).collect();
    let mut body = lines.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    let tmp = list.with_extension("tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, list)?;
    Ok(lines.len())
}

pub fn is_active(mode_file: &Path) -> bool {
    match std::fs::read_to_string(mode_file) {
        Ok(s) => s.trim() != "deactive",
        Err(_) => true,
    }
}

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

/// Remove every line exactly equal to `entry` (`grep -vxF` parity), via temp file + rename so a
/// crash can't truncate the catalog. A missing list is `Ok(false)`; returns whether anything was
/// removed.
pub fn remove_from_list(list: &Path, entry: &str) -> std::io::Result<bool> {
    let contents = match std::fs::read_to_string(list) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let kept: Vec<&str> = contents.lines().filter(|l| *l != entry).collect();
    let removed = kept.len() != contents.lines().count();
    if !removed {
        return Ok(false);
    }
    let mut body = kept.join("\n");
    if !body.is_empty() {
        body.push('\n');
    }
    let tmp = list.with_extension("tmp");
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, list)?;
    Ok(true)
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
        // Empty lines drop, but whitespace-only lines pass through verbatim, since paths can
        // have leading/trailing spaces.
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
    fn remove_from_list_drops_exact_lines_only() {
        let p = tmpfile("remove", "/a.png\n/sub/a.png\n/b with space.png \n/a.png\n");
        assert!(remove_from_list(&p, "/a.png").unwrap());
        let after = std::fs::read_to_string(&p).unwrap();
        // Exact-line match: both /a.png copies go; the substring and trailing-space variants stay.
        assert_eq!(after, "/sub/a.png\n/b with space.png \n");
        assert!(!remove_from_list(&p, "/not-there.png").unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), after);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn remove_from_list_tolerates_missing_file_and_empties_cleanly() {
        assert!(!remove_from_list(Path::new("/nonexistent/list"), "/a.png").unwrap());
        let p = tmpfile("remove-all", "/only.png\n");
        assert!(remove_from_list(&p, "/only.png").unwrap());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "");
        let _ = std::fs::remove_file(&p);
    }

    fn tmpdir(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("copad-bg-dir-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn touch(dir: &Path, name: &str) {
        if let Some(parent) = Path::new(name).parent() {
            std::fs::create_dir_all(dir.join(parent)).unwrap();
        }
        std::fs::File::create(dir.join(name)).unwrap();
    }

    #[test]
    fn scan_dir_filters_by_extension_case_insensitively() {
        let d = tmpdir("ext");
        for f in ["a.jpg", "b.JPG", "c.PnG", "d.txt", "e", "f.jpeg"] {
            touch(&d, f);
        }
        let src = DirSource::new(d.clone(), false, &[".JPG".into(), "png".into()]);
        let found = scan_dir(&src).unwrap();
        let names: Vec<_> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        // `.jpeg` must not match `jpg`; the config value's dot/case is normalized away.
        assert_eq!(names, vec!["a.jpg", "b.JPG", "c.PnG"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn scan_dir_honors_recursive_flag() {
        let d = tmpdir("rec");
        touch(&d, "top.png");
        touch(&d, "sub/nested.png");
        touch(&d, "sub/deeper/deep.png");

        let flat = scan_dir(&DirSource::new(d.clone(), false, &["png".into()])).unwrap();
        assert_eq!(flat.len(), 1, "non-recursive must stay at maxdepth 1");

        let deep = scan_dir(&DirSource::new(d.clone(), true, &["png".into()])).unwrap();
        assert_eq!(deep.len(), 3);
        // readdir order is filesystem-dependent; callers rely on sorted output.
        let mut sorted = deep.clone();
        sorted.sort();
        assert_eq!(deep, sorted);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn scan_dir_empty_extensions_accepts_everything_and_errors_on_missing_root() {
        let d = tmpdir("any");
        touch(&d, "a.txt");
        touch(&d, "b");
        let all = scan_dir(&DirSource::new(d.clone(), false, &[])).unwrap();
        assert_eq!(all.len(), 2);
        // An unreadable root must be an error, not an empty list that looks like "no wallpapers".
        assert!(scan_dir(&DirSource::new(d.join("nope"), false, &[])).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn list_entries_returns_every_usable_line_in_order() {
        let d = tmpdir("entries");
        let list = d.join("wallpapers.txt");
        std::fs::write(&list, "/a.png\n\n/b with space.png\n/c.png\n").unwrap();
        let paths = BackgroundPaths {
            primary_list: list,
            fallback_list: None,
            mode_file: PathBuf::from("/nonexistent/m"),
        };
        assert_eq!(
            list_entries(&paths),
            vec!["/a.png", "/b with space.png", "/c.png"]
        );

        let absent = BackgroundPaths {
            primary_list: d.join("nope.txt"),
            fallback_list: None,
            mode_file: PathBuf::from("/nonexistent/m"),
        };
        assert!(list_entries(&absent).is_empty());
        assert!(pick_random(&absent).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pick_one_is_none_for_empty_and_in_range_otherwise() {
        let empty: Vec<u8> = vec![];
        assert!(pick_one(&empty).is_none());
        let items = vec![1, 2, 3, 4, 5];
        for _ in 0..50 {
            assert!(items.contains(pick_one(&items).unwrap()));
        }
    }

    #[test]
    fn write_list_round_trips_through_pick_random() {
        let d = tmpdir("write");
        let list = d.join("nested").join("wallpapers.txt");
        let entries = vec![PathBuf::from("/a.png"), PathBuf::from("/b.png")];
        assert_eq!(write_list(&list, &entries).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(&list).unwrap(), "/a.png\n/b.png\n");

        let picked = pick_random(&BackgroundPaths {
            primary_list: list.clone(),
            fallback_list: None,
            mode_file: PathBuf::from("/nonexistent/m"),
        })
        .unwrap();
        assert!(picked == "/a.png" || picked == "/b.png");

        assert_eq!(write_list(&list, &[]).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&list).unwrap(), "");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn toggle_flips_and_persists() {
        let m = std::env::temp_dir().join(format!("copad-bg-mode-{}", std::process::id()));
        let _ = std::fs::remove_file(&m);
        let after_first = toggle(&m);
        assert!(!after_first);
        assert!(!is_active(&m));
        let after_second = toggle(&m);
        assert!(after_second);
        assert!(is_active(&m));
        let _ = std::fs::remove_file(&m);
    }
}
