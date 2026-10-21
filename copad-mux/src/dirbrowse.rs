//! The `Ctrl-b C` directory browser's model: list a directory's subdirectories off the
//! render loop, and turn a listing + the typed filter into the rows the overlay shows.
//!
//! Listing runs on a detached thread ([`spawn_list`]) because `read_dir` — and following a
//! symlink to see whether it is a directory — can block indefinitely on a network mount or a
//! dead automount, and the App loop also serves every other client and the control socket.
//! A thread stuck that way is simply abandoned: the next navigation bumps the request id and
//! its late result is discarded.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::picker::fuzzy_match;

/// Raw directory entries examined per listing. Past this the listing is cut short (and says
/// so) rather than walking a million-file directory on every keypress-driven navigation.
const MAX_SCANNED: usize = 50_000;
/// Subdirectories kept per listing — applied AFTER filtering to directories, so a directory
/// full of files can't crowd every usable row out.
const MAX_DIRS: usize = 5_000;

/// The subdirectories of one directory, sorted case-insensitively.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Listing {
    /// Kept as `OsString` so a non-UTF-8 name stays navigable (it only DISPLAYS lossily).
    pub names: Vec<OsString>,
    /// A cap was hit: the list is incomplete. Typing a path still reaches anything.
    pub truncated: bool,
}

/// A finished listing, tagged with the request it answers.
#[derive(Debug, Clone)]
pub struct ListResult {
    pub req: u64,
    pub dir: PathBuf,
    pub listing: Result<Listing, String>,
}

/// Where the listing thread drops its result for the App loop to adopt.
pub type Slot = Arc<Mutex<Option<ListResult>>>;

/// List `dir`'s subdirectories (symlinks to directories included).
pub fn list_dirs(dir: &Path) -> Result<Listing, String> {
    let rd = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut names = Vec::new();
    let mut truncated = false;
    for (i, entry) in rd.enumerate() {
        if i >= MAX_SCANNED {
            truncated = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        let is_dir = match entry.file_type() {
            Ok(t) if t.is_dir() => true,
            Ok(t) if t.is_symlink() => entry.path().is_dir(),
            _ => false,
        };
        if is_dir {
            names.push(entry.file_name());
        }
    }
    names.sort_by_cached_key(|n| n.to_string_lossy().to_lowercase());
    if names.len() > MAX_DIRS {
        names.truncate(MAX_DIRS);
        truncated = true;
    }
    Ok(Listing { names, truncated })
}

/// List `dir` on a detached thread, publishing into `slot` tagged with `req` (which must
/// increase with every call). Threads finish in any order, so a result never replaces a
/// NEWER one: a slow listing of the directory you just left would otherwise overwrite the
/// one you are waiting for, and the browser would sit on "loading…" forever.
pub fn spawn_list(slot: Slot, req: u64, dir: PathBuf) {
    std::thread::spawn(move || {
        let listing = list_dirs(&dir);
        publish(&slot, ListResult { req, dir, listing });
    });
}

fn publish(slot: &Slot, res: ListResult) {
    if let Ok(mut g) = slot.lock()
        && g.as_ref().is_none_or(|cur| cur.req < res.req)
    {
        *g = Some(res);
    }
}

/// How long a probe may go unanswered before its directory stops counting as usable: a stat
/// stuck on a dead mount is exactly what the probe keeps off the App loop, and a shell spawned
/// there would hang the same way.
const PROBE_HANG: Duration = Duration::from_secs(2);

/// Whether each session's start directory still exists, learned OFF the App loop.
///
/// The loop only ever reads the last answer ([`DirProbe::usable`] never touches the
/// filesystem); [`DirProbe::refresh`] stats on detached threads, at most one in flight per
/// directory, so a dead mount strands one thread rather than one per refresh.
#[derive(Clone, Default)]
pub struct DirProbe(Arc<Mutex<HashMap<PathBuf, Probe>>>);

#[derive(Default)]
struct Probe {
    /// The last answer; `None` until the first probe lands.
    alive: Option<bool>,
    /// A probe is in flight, started then.
    pending: Option<Instant>,
}

impl DirProbe {
    /// Whether a new tab may open in `dir`: confirmed to exist, and not stuck on a probe.
    /// Unconfirmed is NOT usable — a shell spawned in a missing directory silently starts in
    /// the server's cwd instead of failing — so a directory counts only once a probe (or a
    /// shell started there, [`Self::mark_alive`]) has vouched for it.
    pub fn usable(&self, dir: &Path) -> bool {
        let Ok(g) = self.0.lock() else { return false };
        g.get(dir).is_some_and(|p| {
            p.alive == Some(true) && p.pending.is_none_or(|t| t.elapsed() < PROBE_HANG)
        })
    }

    /// A shell just started in `dir`, which is as good as a probe.
    pub fn mark_alive(&self, dir: PathBuf) {
        if let Ok(mut g) = self.0.lock() {
            let p = g.entry(dir).or_default();
            p.alive = Some(true);
            p.pending = None;
        }
    }

    /// Re-check `dirs` (and forget any other directory) on detached threads.
    pub fn refresh(&self, dirs: impl IntoIterator<Item = PathBuf>) {
        let dirs: HashSet<PathBuf> = dirs.into_iter().collect();
        let Ok(mut g) = self.0.lock() else { return };
        g.retain(|d, _| dirs.contains(d));
        for dir in dirs {
            let p = g.entry(dir.clone()).or_default();
            if p.pending.is_some() {
                continue;
            }
            p.pending = Some(Instant::now());
            let map = self.0.clone();
            std::thread::spawn(move || {
                let alive = dir.is_dir();
                // `get_mut`, not `entry`: a directory forgotten meanwhile stays forgotten.
                if let Ok(mut g) = map.lock()
                    && let Some(p) = g.get_mut(&dir)
                {
                    p.alive = Some(alive);
                    p.pending = None;
                }
            });
        }
    }
}

/// One row of the browser.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    /// Create the session in the directory being browsed.
    Here,
    /// Go to the parent directory.
    Up,
    /// A subdirectory, by name.
    Dir(OsString),
    /// The filter is a path (`/…` or `~…`): jump there.
    Path(PathBuf),
}

/// Whether the filter is a path to jump to rather than a name to narrow by.
pub fn is_path_filter(filter: &str) -> bool {
    filter.starts_with('/') || filter.starts_with('~')
}

/// Expand a path filter: a leading `~` is `home`.
pub fn expand(filter: &str, home: &Path) -> PathBuf {
    match filter.strip_prefix('~') {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(filter),
    }
}

/// The rows for `dir`'s `listing` under `filter`.
///
/// No filter: `Here`, `Up` (unless at the root), then every non-hidden subdirectory. A
/// filter narrows to matching subdirectories only — so the best match is the first row and
/// Enter descends into it — ranked prefix, then substring, then scattered (fzf-style)
/// matches; a filter starting with `.` is what reveals hidden directories.
pub fn rows(dir: &Path, listing: &Listing, filter: &str, home: &Path) -> Vec<Row> {
    if is_path_filter(filter) {
        return vec![Row::Path(expand(filter, home))];
    }
    if filter.is_empty() {
        let mut out = vec![Row::Here];
        if dir.parent().is_some() {
            out.push(Row::Up);
        }
        out.extend(
            listing
                .names
                .iter()
                .filter(|n| !n.to_string_lossy().starts_with('.'))
                .cloned()
                .map(Row::Dir),
        );
        return out;
    }
    let show_hidden = filter.starts_with('.');
    let needle = filter.to_lowercase();
    let mut ranked: Vec<(u8, &OsString)> = listing
        .names
        .iter()
        .filter_map(|n| {
            let name = n.to_string_lossy();
            if name.starts_with('.') && !show_hidden {
                return None;
            }
            let lower = name.to_lowercase();
            let rank = if lower.starts_with(&needle) {
                0
            } else if lower.contains(&needle) {
                1
            } else if fuzzy_match(filter, &name) {
                2
            } else {
                return None;
            };
            Some((rank, n))
        })
        .collect();
    // Stable: names keep their sorted order within a rank.
    ranked.sort_by_key(|(r, _)| *r);
    ranked
        .into_iter()
        .map(|(_, n)| Row::Dir(n.clone()))
        .collect()
}

/// The default session name for `dir`: its last component (empty for `/`).
pub fn default_name(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing(names: &[&str]) -> Listing {
        Listing {
            names: names.iter().map(OsString::from).collect(),
            truncated: false,
        }
    }

    #[test]
    fn unfiltered_rows_lead_with_here_and_up_and_hide_dotdirs() {
        let l = listing(&[".git", "copad", "docs"]);
        let r = rows(Path::new("/home/u"), &l, "", Path::new("/home/u"));
        assert_eq!(
            r,
            vec![
                Row::Here,
                Row::Up,
                Row::Dir("copad".into()),
                Row::Dir("docs".into())
            ]
        );
        // The root has no parent to go up to.
        let r = rows(Path::new("/"), &l, "", Path::new("/home/u"));
        assert_eq!(r[..2], [Row::Here, Row::Dir("copad".into())]);
    }

    #[test]
    fn a_filter_ranks_prefix_then_substring_then_scattered() {
        let l = listing(&["acopy", "c-o-p", "copad", "xcop"]);
        let r = rows(Path::new("/d"), &l, "cop", Path::new("/h"));
        assert_eq!(
            r,
            vec![
                Row::Dir("copad".into()),
                Row::Dir("acopy".into()),
                Row::Dir("xcop".into()),
                Row::Dir("c-o-p".into()),
            ]
        );
    }

    #[test]
    fn a_dot_filter_reveals_hidden_directories() {
        let l = listing(&[".config", "config"]);
        let h = Path::new("/h");
        assert_eq!(
            rows(Path::new("/d"), &l, "conf", h),
            vec![Row::Dir("config".into())]
        );
        assert_eq!(
            rows(Path::new("/d"), &l, ".conf", h),
            vec![Row::Dir(".config".into())]
        );
    }

    #[test]
    fn a_path_filter_is_one_jump_row_with_tilde_expanded() {
        let h = Path::new("/home/u");
        let l = listing(&["x"]);
        assert_eq!(
            rows(Path::new("/d"), &l, "~/dev", h),
            vec![Row::Path(PathBuf::from("/home/u/dev"))]
        );
        assert_eq!(
            rows(Path::new("/d"), &l, "/etc", h),
            vec![Row::Path(PathBuf::from("/etc"))]
        );
        assert_eq!(expand("~", h), PathBuf::from("/home/u"));
    }

    #[test]
    fn lists_only_directories_including_symlinked_ones() {
        let tmp = std::env::temp_dir().join(format!("comux-dirbrowse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("Beta")).unwrap();
        std::fs::create_dir_all(tmp.join("alpha")).unwrap();
        std::fs::write(tmp.join("file.txt"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.join("alpha"), tmp.join("link")).unwrap();
        let l = list_dirs(&tmp).unwrap();
        let names: Vec<_> = l
            .names
            .iter()
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["alpha", "Beta", "link"]);
        assert!(!l.truncated);
        assert!(list_dirs(&tmp.join("missing")).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_late_result_never_replaces_a_newer_one() {
        let slot: Slot = Default::default();
        let res = |req| ListResult {
            req,
            dir: PathBuf::from("/"),
            listing: Ok(Listing::default()),
        };
        publish(&slot, res(2));
        publish(&slot, res(1));
        assert_eq!(slot.lock().unwrap().as_ref().unwrap().req, 2);
        publish(&slot, res(3));
        assert_eq!(slot.lock().unwrap().as_ref().unwrap().req, 3);
    }

    #[test]
    fn the_probe_learns_off_thread_and_never_blocks_the_reader() {
        let probe = DirProbe::default();
        let gone = std::env::temp_dir().join("comux-dirprobe-definitely-missing");
        let here = std::env::temp_dir();
        // Unconfirmed is not usable.
        assert!(!probe.usable(&here));
        probe.refresh([gone.clone(), here.clone()]);
        for _ in 0..200 {
            if probe.usable(&here) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(probe.usable(&here));
        assert!(!probe.usable(&gone));
        // A stuck probe stops counting as usable once it overruns.
        probe.0.lock().unwrap().get_mut(&here).unwrap().pending = Some(Instant::now() - PROBE_HANG);
        assert!(!probe.usable(&here));
        probe.mark_alive(here.clone());
        assert!(probe.usable(&here));
        // A directory no session uses any more is forgotten.
        probe.refresh([here.clone()]);
        assert!(!probe.0.lock().unwrap().contains_key(&gone));
    }

    #[test]
    fn default_name_is_the_last_component() {
        assert_eq!(default_name(Path::new("/home/u/dev/copad")), "copad");
        assert_eq!(default_name(Path::new("/")), "");
    }
}
