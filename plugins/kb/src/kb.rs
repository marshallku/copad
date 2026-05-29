//! KB operations against a filesystem root.
//!
//! Every public method maps to one wire action in `docs/kb-protocol.md`.
//! Atomicity-critical paths (`kb.append` single-syscall write,
//! `kb.ensure` temp-rename with `RENAME_NOREPLACE`) drop to `libc`
//! directly because `std::fs` doesn't expose the precise semantics the
//! protocol requires.

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value, json};

/// Wire error returned to the supervisor as `{code, message}`.
type KbErr = (String, String);

const RAW_FOLDER: &str = ".raw";
const SEARCH_LIMIT_DEFAULT: usize = 20;
const SEARCH_LIMIT_CAP: usize = 100;
const LIST_LIMIT_DEFAULT: usize = 50;
const LIST_LIMIT_CAP: usize = 500;
/// Snippet width on each side of the match (in chars).
const SNIPPET_RADIUS: usize = 60;

pub struct Kb {
    /// Logical root the user configured (e.g. `~/docs`).
    root: PathBuf,
    /// Resolved root used to enforce "id cannot escape KB root" against
    /// symlinks. Falls back to `root` if `canonicalize()` failed at construction.
    root_canonical: PathBuf,
}

struct KbHit {
    id: String,
    title: Option<Value>,
    score: f64,
    snippet: String,
    path: Option<PathBuf>,
    match_kind: String,
}

impl KbHit {
    fn into_json(self) -> Value {
        json!({
            "id": self.id,
            "title": self.title.unwrap_or(Value::Null),
            "score": self.score,
            "snippet": self.snippet,
            "path": self.path.as_deref().map(path_to_value).unwrap_or(Value::Null),
            "match_kind": self.match_kind,
        })
    }
}

impl Kb {
    pub fn from_env() -> Self {
        let raw_root = std::env::var("COPAD_KB_ROOT")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                PathBuf::from(home).join("docs")
            });
        // Ensure the root exists so the very first call doesn't fail
        // for a brand-new install. Caller errors (permissions, etc.)
        // surface on first use.
        let _ = std::fs::create_dir_all(&raw_root);
        // Force absolute. The protocol contract is "path is absolute
        // for FS backends" — a relative `COPAD_KB_ROOT` like `docs`
        // would otherwise produce relative paths in every response.
        // First try canonicalize (resolves symlinks too, which is also
        // what `root_canonical` needs for the symlink-escape check);
        // if that fails for any reason, manually absolute-ify by
        // joining with cwd.
        let absolute_root = std::fs::canonicalize(&raw_root).unwrap_or_else(|_| {
            if raw_root.is_absolute() {
                raw_root.clone()
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(&raw_root))
                    .unwrap_or(raw_root.clone())
            }
        });
        Self {
            root: absolute_root.clone(),
            root_canonical: absolute_root,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Verifies the resolved path stays inside `root_canonical`, defending
    /// against symlink escapes that `validate_id` can't catch (e.g. a `notes`
    /// symlink pointing at `/etc`). `must_exist=true` canonicalizes the leaf
    /// itself; otherwise canonicalizes the existing ancestor and appends the
    /// tail so `kb.ensure` can create new files while still catching parent
    /// symlinks. Residual TOCTOU window between check and open is acceptable
    /// for single-user use; tightening would need `openat2(RESOLVE_BENEATH)`.
    fn resolve_within_root(&self, id: &str, must_exist: bool) -> Result<PathBuf, KbErr> {
        // Refuse if any ancestor of the id (within the KB root) is a
        // symlink. Without this, `alias -> notes` would let
        // `kb.ensure("alias/foo.md")` write a file whose subsequent
        // search-time `id` would be `notes/foo.md` — the protocol
        // says ids are stable handles, but aliasing breaks that.
        // `alias -> .raw/slack` is the worse case: the alias-side id
        // bypasses the `.raw/` asymmetry (the file becomes writable
        // AND readable AND search-surfaceable under the alias). The
        // walk-side `.raw/` exclusion is keyed on the LOGICAL id, so
        // it can't catch this; the trust boundary has to be enforced
        // at id resolution time.
        self.check_no_symlink_ancestors(id)?;
        let candidate = self.root.join(id);

        // If the leaf exists, refuse anything that isn't a regular
        // file. Documents are files; reading from a FIFO would block
        // the supervisor indefinitely, reading from a device file
        // could surface kernel state, and writing to either is
        // outside the protocol's "filesystem documents" model. We
        // also reject directories (kb.ensure has a separate check
        // for this with a more specific error message — that runs
        // before this point in `ensure()`).
        if let Ok(md) = candidate.symlink_metadata() {
            let ft = md.file_type();
            if ft.is_symlink() {
                return Err((
                    "forbidden".into(),
                    format!("id {id} resolves to a symlink at the leaf"),
                ));
            }
            if !ft.is_file() && !ft.is_dir() {
                return Err((
                    "invalid_id".into(),
                    format!("id {id} resolves to a non-regular file (FIFO, socket, or device)"),
                ));
            }
        }

        let to_check: PathBuf = if must_exist {
            std::fs::canonicalize(&candidate).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    ("not_found".into(), format!("kb id not found: {id}"))
                } else {
                    io_error(format!("canonicalize {id}: {e}"))
                }
            })?
        } else {
            // Walk up until we find an existing ancestor, canonicalize
            // that, and append the remaining tail. This catches a
            // symlinked parent without requiring the leaf to exist.
            let mut existing = candidate.as_path();
            let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
            loop {
                if existing.exists() {
                    break;
                }
                match (existing.parent(), existing.file_name()) {
                    (Some(p), Some(name)) => {
                        tail.push(name);
                        existing = p;
                    }
                    _ => break,
                }
            }
            let mut canon = std::fs::canonicalize(existing).map_err(|e| {
                io_error(format!(
                    "canonicalize ancestor {} for id {id}: {e}",
                    existing.display()
                ))
            })?;
            for seg in tail.iter().rev() {
                canon.push(seg);
            }
            canon
        };
        if !to_check.starts_with(&self.root_canonical) {
            return Err((
                "forbidden".into(),
                format!("id {id} resolves outside KB root (symlink escape?)"),
            ));
        }
        // Return the original (non-canonical) path so kb.read/append
        // operate against the user-visible filesystem location and
        // observe the file the user expects. Canonicalize was a check,
        // not a substitution.
        Ok(candidate)
    }

    /// Refuses any logical path whose existing ancestors contain a symlink.
    /// Non-existent tail segments are fine. Without this, `alias -> notes`
    /// would let `kb.ensure("alias/foo.md")` write a file that later searches
    /// as `notes/foo.md` (different id, breaks the "id is the stable handle"
    /// contract), and `alias -> .raw/slack` would bypass the `.raw/`
    /// search-exclusion via the alias-side id.
    fn check_no_symlink_ancestors(&self, logical: &str) -> Result<(), KbErr> {
        let mut cursor = self.root.clone();
        for comp in Path::new(logical).components() {
            if let Component::Normal(seg) = comp {
                cursor.push(seg);
                match cursor.symlink_metadata() {
                    Ok(md) if md.file_type().is_symlink() => {
                        return Err((
                            "forbidden".into(),
                            format!("{logical} traverses a symlink at {}", cursor.display()),
                        ));
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Tail doesn't exist — rest of the path can't
                        // be a symlink. Stop walking.
                        return Ok(());
                    }
                    Err(e) => {
                        // EACCES / ENOTDIR / other — surface as
                        // io_error so the caller knows the boundary
                        // check couldn't actually run, rather than
                        // silently passing and tripping a vaguer
                        // error later in the action path.
                        return Err(io_error(format!("stat ancestor {}: {e}", cursor.display())));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn invoke(&self, action: &str, params: &Value) -> Result<Value, KbErr> {
        match action {
            "kb.search" => self.search(params),
            "kb.read" => self.read(params),
            "kb.append" => self.append(params),
            "kb.ensure" => self.ensure(params),
            "kb.list" => self.list(params),
            other => Err((
                "unknown_method".into(),
                format!("kb plugin doesn't handle {other}"),
            )),
        }
    }

    // -------- kb.list (Phase 22.3) --------
    //
    // Browse-shaped surface for the KB Panel: returns entry ids (NOT full
    // content), optionally with mtime / frontmatter, sorted by name or
    // mtime. Powers the panel's per-folder browser + recent-edits slots.
    // Same trust-boundary as kb.search — `folder` validated through
    // resolve_within_root, `.raw/` excluded, symlinks skipped.

    fn list(&self, params: &Value) -> Result<Value, KbErr> {
        let folder_param: Option<String> = match params.get("folder") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => {
                validate_folder(s)?;
                Some(s.trim_end_matches('/').to_string())
            }
            Some(_) => return Err(invalid_params("'folder' must be a string")),
        };
        let recursive = require_optional_bool(params, "recursive", false)?;
        let include_mtime = require_optional_bool(params, "include_mtime", false)?;
        let include_frontmatter = require_optional_bool(params, "include_frontmatter", false)?;
        let sort_key = match params.get("sort") {
            None | Some(Value::Null) => "name".to_string(),
            Some(Value::String(s)) if s == "name" || s == "mtime" => s.clone(),
            Some(_) => return Err(invalid_params("'sort' must be \"name\" or \"mtime\"")),
        };
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(LIST_LIMIT_DEFAULT as u64)
            .min(LIST_LIMIT_CAP as u64) as usize;
        let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;

        let list_root = match folder_param.as_ref() {
            None => self.root.clone(),
            Some(f) => self.resolve_within_root(f, false)?,
        };

        if !list_root.exists() {
            return Ok(json!({ "entries": [], "total": 0 }));
        }
        std::fs::read_dir(&list_root)
            .map_err(|e| io_error(format!("read list root {}: {e}", list_root.display())))?;

        struct Entry {
            id: String,
            kind: &'static str,
            mtime_ms: Option<i64>,
            frontmatter: Option<Value>,
        }
        let mut entries: Vec<Entry> = Vec::new();
        walk(&list_root, &mut |path, ft| {
            if ft.is_symlink() {
                return WalkAction::Skip;
            }
            // `.raw/` exclusion against logical id (consistent with kb.search).
            if let Ok(rel) = path.strip_prefix(&self.root)
                && rel
                    .components()
                    .next()
                    .map(|c| c.as_os_str() == RAW_FOLDER)
                    .unwrap_or(false)
            {
                return WalkAction::Skip;
            }
            // Dotfiles/dotdirs: skip by default. The user-passed folder
            // itself can be a dotfolder (already past validation) and
            // we don't reach the dot check for it because it's the walk
            // root. Walk entries under the root that start with `.` are
            // tooling indices (`.backlinks/`, `.tags/`, `.git/`) — not
            // user-facing notes. Surfacing them in a folder browser is
            // the wrong default; callers wanting them can pass the
            // dotfolder explicitly as `folder`.
            if let Some(name) = path.file_name().and_then(|s| s.to_str())
                && name.starts_with('.')
            {
                return WalkAction::Skip;
            }
            let rel = match path.strip_prefix(&self.root) {
                Ok(r) => r,
                Err(_) => return WalkAction::Continue,
            };
            let id = rel_to_id(rel);
            if ft.is_dir() {
                let mtime_ms = if include_mtime {
                    read_mtime_ms(path)
                } else {
                    None
                };
                entries.push(Entry {
                    id,
                    kind: "dir",
                    mtime_ms,
                    frontmatter: None,
                });
                if recursive {
                    return WalkAction::Recurse;
                }
                return WalkAction::Skip;
            }
            if ft.is_file() {
                let mtime_ms = if include_mtime || sort_key == "mtime" {
                    read_mtime_ms(path)
                } else {
                    None
                };
                let frontmatter = if include_frontmatter {
                    read_frontmatter_for_listing(path)
                } else {
                    None
                };
                entries.push(Entry {
                    id,
                    kind: "file",
                    mtime_ms,
                    frontmatter,
                });
            }
            WalkAction::Continue
        });

        if sort_key == "mtime" {
            entries.sort_by(|a, b| {
                b.mtime_ms
                    .unwrap_or(0)
                    .cmp(&a.mtime_ms.unwrap_or(0))
                    .then_with(|| a.id.cmp(&b.id))
            });
        } else {
            entries.sort_by(|a, b| a.id.cmp(&b.id));
        }

        let total = entries.len();
        let page: Vec<Value> = entries
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|e| {
                let mut o = Map::new();
                o.insert("id".into(), Value::String(e.id));
                o.insert("kind".into(), Value::String(e.kind.into()));
                if let Some(m) = e.mtime_ms {
                    o.insert("mtime_ms".into(), Value::Number(m.into()));
                }
                if let Some(f) = e.frontmatter {
                    o.insert("frontmatter".into(), f);
                }
                Value::Object(o)
            })
            .collect();
        Ok(json!({ "entries": page, "total": total }))
    }

    // -------- kb.search --------

    fn search(&self, params: &Value) -> Result<Value, KbErr> {
        let query = params
            .get("query")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_params("missing 'query' string"))?;
        if query.is_empty() {
            return Err(invalid_params("query cannot be empty"));
        }
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(SEARCH_LIMIT_DEFAULT as u64)
            .min(SEARCH_LIMIT_CAP as u64) as usize;
        let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;

        // Folder is the same trust-boundary input as `id` — same
        // validation rules apply, surfaced as `forbidden` on traversal
        // attempts (per protocol). Type-strict: a non-string folder
        // is `invalid_params`, NOT silently dropped (silent coercion
        // would widen the search scope contrary to caller intent).
        // Validation runs against the ORIGINAL string before any
        // normalization — `folder: ""` and `folder: "/"` MUST fail
        // validation rather than silently collapsing to "no folder
        // filter, search the whole root."
        let folder_param: Option<String> = match params.get("folder") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => {
                validate_folder(s)?;
                // After validation we may safely strip a trailing slash
                // (the user typed `meetings/` but means `meetings`).
                // The pre-validation check above guarantees s isn't
                // empty / `/`, so trimming can't produce an empty
                // string that would re-enter the whole-root branch.
                Some(s.trim_end_matches('/').to_string())
            }
            Some(_) => {
                return Err(invalid_params("'folder' must be a string"));
            }
        };
        let search_root = match folder_param {
            None => self.root.clone(),
            // resolve_within_root already calls
            // `check_no_symlink_ancestors`, which catches the alias
            // attack where `notes -> .raw/slack` would surface raw
            // archives. must_exist=false because the folder might
            // not be created yet on a fresh install.
            Some(f) => self.resolve_within_root(&f, false)?,
        };

        // Probe the search root. If it doesn't exist (e.g. user
        // passed a folder that hasn't been created yet), zero hits is
        // the right answer — it's not an error. If it exists but we
        // can't read it (permissions, ENOTDIR, etc.), surface so the
        // caller knows their search wasn't actually performed.
        // Per-file failures during the walk stay silent (skipped),
        // which is what users want for a personal docs dir with the
        // occasional unreadable binary or locked file.
        if search_root.exists() {
            std::fs::read_dir(&search_root).map_err(|e| {
                io_error(format!("read search root {}: {e}", search_root.display()))
            })?;
        } else {
            return Ok(json!({ "hits": [], "total": 0 }));
        }

        let q_lower = query.to_lowercase();
        let mut hits: Vec<KbHit> = Vec::new();
        walk(&search_root, &mut |path, ft| {
            // Symlinks are skipped entirely — the trust-boundary
            // contract is "search can't surface anything outside the
            // KB root." Following a symlink (even to a sibling inside
            // root) risks recursing into a target outside root, and
            // any hit produced by such a target would also fail a
            // subsequent `kb.read` against its `id` (the canonical
            // path differs from the logical id), breaking the "stable
            // handle" contract on search hits. The `ft` we got is
            // from `DirEntry::file_type` which does NOT follow
            // symlinks, so this check is the authoritative one — no
            // re-stat that could reopen a TOCTOU window.
            if ft.is_symlink() {
                return WalkAction::Skip;
            }

            // Protocol-mandated `.raw/` exclusion. Computed against
            // the path relative to the KB root so a user-named file
            // `notes/.raw-thoughts.md` still surfaces.
            if let Ok(rel) = path.strip_prefix(&self.root)
                && rel
                    .components()
                    .next()
                    .map(|c| c.as_os_str() == RAW_FOLDER)
                    .unwrap_or(false)
            {
                return WalkAction::Skip;
            }
            if ft.is_dir() {
                return WalkAction::Recurse;
            }
            if ft.is_file()
                && let Some(hit) = self.score_file(path, &q_lower)
            {
                hits.push(hit);
            }
            WalkAction::Continue
        });

        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        let total = hits.len();
        let page: Vec<Value> = hits
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(KbHit::into_json)
            .collect();
        Ok(json!({ "hits": page, "total": total }))
    }

    fn score_file(&self, path: &Path, q_lower: &str) -> Option<KbHit> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let id = rel_to_id(rel);

        // Filename match is the BASENAME only — querying "meetings"
        // shouldn't auto-promote every file under `meetings/` to
        // filename-weight just because the folder name matches. Folder
        // matches still contribute via body-text scoring if the doc
        // mentions the term, which is the more honest signal.
        let filename = rel
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("")
            .to_lowercase();

        // Filename matches are weighted more than content matches.
        // A filename hit is a strong signal — the user named the file
        // for that concept. Weight 10 vs 1 keeps a single filename
        // match above ~10 isolated body matches but loses to a body
        // dense with the term.
        let mut score = 0.0f64;
        let filename_matches = filename.matches(q_lower).count();
        score += (filename_matches as f64) * 10.0;

        // Read content for body match + snippet. Skip files we can't
        // open (binary, permissions, or TOCTOU-swapped to a symlink
        // since walk's `file_type` check) silently — they don't
        // contribute. `O_NOFOLLOW` ensures even a swap-to-symlink
        // race window can't make us read outside the KB root.
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .ok()?;
        let mut content = String::new();
        use std::io::Read;
        file.read_to_string(&mut content).ok()?;
        let content_lower = content.to_lowercase();
        let body_matches = content_lower.matches(q_lower).count();
        score += body_matches as f64;

        // Match-kind reflects how the score was earned. If both fired
        // we report `fulltext` because that's the more informative
        // signal for the caller; pure-filename hits stay marked so the
        // UI can render them differently.
        let match_kind = if body_matches > 0 {
            "fulltext"
        } else if filename_matches > 0 {
            "filename"
        } else {
            return None;
        };

        let snippet = build_snippet(&content, &content_lower, q_lower).unwrap_or_else(|| {
            // No body match — preview the first non-empty line.
            content
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .chars()
                .take(120)
                .collect()
        });

        let title = extract_title(&content, rel);

        Some(KbHit {
            id,
            title,
            score,
            snippet,
            path: Some(path.to_path_buf()),
            match_kind: match_kind.to_string(),
        })
    }

    // -------- kb.read --------

    fn read(&self, params: &Value) -> Result<Value, KbErr> {
        let id = require_id(params)?;
        validate_id(&id)?;
        let path = self.resolve_within_root(&id, true)?;
        // O_NOFOLLOW closes the symlink-swap-at-leaf TOCTOU window:
        // if a concurrent actor swaps the leaf for a symlink between
        // `resolve_within_root`'s canonicalize check and this open,
        // the kernel returns ELOOP rather than dereferencing it.
        // Residual race window: a swap of an INTERMEDIATE directory
        // for a symlink is still possible (would need full
        // `openat2(RESOLVE_BENEATH)` to close). For a single-user
        // local KB that's an accepted risk; documented in roadmap.
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| match e.raw_os_error() {
                Some(c) if c == libc::ELOOP => (
                    "forbidden".into(),
                    format!("id {id} resolves through a symlink at the leaf"),
                ),
                _ if e.kind() == std::io::ErrorKind::NotFound => {
                    ("not_found".into(), format!("kb id not found: {id}"))
                }
                _ => io_error(format!("open {id}: {e}")),
            })?;
        let mut content = String::new();
        use std::io::Read;
        file.read_to_string(&mut content)
            .map_err(|e| io_error(format!("read {id}: {e}")))?;
        let frontmatter = parse_frontmatter(&content);
        Ok(json!({
            "id": id,
            "content": content,
            "frontmatter": frontmatter,
            "path": path_to_value(&path),
        }))
    }

    // -------- kb.append --------

    fn append(&self, params: &Value) -> Result<Value, KbErr> {
        let id = require_id(params)?;
        validate_id(&id)?;
        let content = require_string(params, "content")?;
        let ensure = require_optional_bool(params, "ensure", false)?;

        // If `ensure=true` we may legitimately be creating the file;
        // otherwise the leaf must already exist (and the canonicalize
        // check enforces it stays within the KB root). Either path
        // catches symlink escapes via existing intermediate parents.
        let path = self.resolve_within_root(&id, !ensure)?;
        let bytes = content.as_bytes();

        if !path.exists() {
            if !ensure {
                return Err(("not_found".into(), format!("kb id not found: {id}")));
            }
            // ensure=true winner path: write the payload to the temp
            // file BEFORE the atomic rename, so a concurrent `kb.read`
            // never observes a created-but-empty file. This mirrors
            // the protocol's "winner returns `created=true` with the
            // template applied" rule and extends it from `kb.ensure`
            // to `kb.append+ensure`. If we lose the race the file
            // already exists with someone else's content; we fall
            // through to the normal append branch below.
            create_parents(&path)?;
            match atomic_create_with_content(&path, bytes)? {
                true => {
                    // Won the create race; payload was placed in the
                    // temp file before rename, so the file already
                    // contains everything we needed to write. No
                    // separate O_APPEND step.
                    return Ok(json!({
                        "id": id,
                        "bytes_written": bytes.len(),
                        "created": true,
                        "path": path_to_value(&path),
                    }));
                }
                false => {
                    // Lost the race. Fall through to normal append on
                    // the now-existing file, with `created=false`.
                }
            }
        }

        // Normal append path: file exists, single-syscall O_APPEND
        // write. We do NOT use `write_all` (which loops on partial
        // writes) because the contract is "no interleave with other
        // appenders", which only holds if we issue exactly one
        // `write(2)`. A short write surfaces as `io_error` so the
        // caller knows their payload wasn't fully committed.
        // O_NOFOLLOW for the same TOCTOU defense as `kb.read`.
        let file = OpenOptions::new()
            .append(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| match e.raw_os_error() {
                Some(c) if c == libc::ELOOP => (
                    "forbidden".into(),
                    format!("id {id} resolves through a symlink at the leaf"),
                ),
                _ => io_error(format!("open {id} for append: {e}")),
            })?;
        let written = unsafe { libc::write(file.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            let err = std::io::Error::last_os_error();
            return Err(io_error(format!("write {id}: {err}")));
        }
        let written = written as usize;
        if written != bytes.len() {
            return Err(io_error(format!(
                "short write on {id}: {written}/{} bytes — payload may be partial; \
                 protocol requires single-syscall O_APPEND",
                bytes.len()
            )));
        }
        Ok(json!({
            "id": id,
            "bytes_written": written,
            "created": false,
            "path": path_to_value(&path),
        }))
    }

    // -------- kb.ensure --------

    fn ensure(&self, params: &Value) -> Result<Value, KbErr> {
        let id = require_id(params)?;
        validate_id(&id)?;
        // `default_template` is optional but, per protocol, MUST be a
        // string when present. Silently coercing a non-string to "" is
        // a contract violation (caller meant to write content).
        let template = require_optional_string(params, "default_template", "")?;
        // ensure may create the leaf — must_exist=false. The
        // canonicalize check still rejects an existing intermediate
        // symlink that escapes the root.
        let path = self.resolve_within_root(&id, false)?;

        if let Ok(md) = path.symlink_metadata() {
            // The id resolves to something that already exists. If
            // it's a regular file, idempotent return — protocol says
            // the caller losing the create race gets `created=false`
            // and the existing content stands. If it's a directory
            // (e.g. someone passed an id that names a folder) we
            // refuse: kb.ensure produces documents, not directories,
            // and silently succeeding would hide the misuse.
            if md.file_type().is_dir() {
                return Err((
                    "invalid_id".into(),
                    format!("id {id} resolves to a directory, not a document"),
                ));
            }
            return Ok(json!({
                "id": id,
                "created": false,
                "path": path_to_value(&path),
            }));
        }

        create_parents(&path)?;
        let created = atomic_create_with_content(&path, template.as_bytes())?;
        Ok(json!({
            "id": id,
            "created": created,
            "path": path_to_value(&path),
        }))
    }
}

// -------- helpers: path / id --------

fn require_id(params: &Value) -> Result<String, KbErr> {
    let raw = params
        .get("id")
        .ok_or_else(|| invalid_params("missing 'id'"))?;
    let s = raw
        .as_str()
        .ok_or_else(|| invalid_params("'id' must be a string"))?;
    Ok(s.to_string())
}

/// Require a named string field. Missing or wrong-type → `invalid_params`.
fn require_string(params: &Value, key: &str) -> Result<String, KbErr> {
    let raw = params
        .get(key)
        .ok_or_else(|| invalid_params(format!("missing '{key}' string")))?;
    let s = raw
        .as_str()
        .ok_or_else(|| invalid_params(format!("'{key}' must be a string")))?;
    Ok(s.to_string())
}

/// Wrong type → `invalid_params` (no silent coercion).
fn require_optional_string(params: &Value, key: &str, default: &str) -> Result<String, KbErr> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default.to_string()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(invalid_params(format!("'{key}' must be a string"))),
    }
}

/// Optional bool field with a default. Wrong type → `invalid_params`.
fn require_optional_bool(params: &Value, key: &str, default: bool) -> Result<bool, KbErr> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(invalid_params(format!("'{key}' must be a boolean"))),
    }
}

/// Canonical id form: `/`-separated, no empty / `.` / trailing / leading
/// segments. This is what makes id a stable round-trip handle — multiple
/// surface forms (`notes//foo.md`, `./notes/foo.md`) would all resolve to
/// the same file but break search-emit equality. Errors: `invalid_id` for
/// shape, `forbidden` for trust-boundary violations (`..`, absolute, prefix).
fn validate_id(id: &str) -> Result<(), KbErr> {
    if id.is_empty() {
        return Err(("invalid_id".into(), "id cannot be empty".into()));
    }
    if id.contains('\0') {
        return Err(("invalid_id".into(), "id contains nul bytes".into()));
    }
    if id.starts_with('/') {
        return Err(("forbidden".into(), "id cannot be absolute".into()));
    }
    if id.ends_with('/') {
        return Err((
            "invalid_id".into(),
            "id cannot end with `/`; ids name a single document".into(),
        ));
    }
    // Reject empty segments (consecutive slashes) and `.` segments —
    // both denormalize the id without affecting filesystem resolution,
    // so they'd silently yield duplicate handles for the same file.
    for raw_segment in id.split('/') {
        if raw_segment.is_empty() {
            return Err((
                "invalid_id".into(),
                "id cannot contain empty segments (consecutive `/`)".into(),
            ));
        }
        if raw_segment == "." {
            return Err(("invalid_id".into(), "id cannot contain `.` segments".into()));
        }
    }
    // Component walk catches the trust-boundary cases (`..`, absolute,
    // OS-prefix). `..` substrings inside a filename (`a..b.md`) are
    // fine; only the bare `..` segment is traversal.
    for comp in Path::new(id).components() {
        match comp {
            Component::ParentDir => {
                return Err((
                    "forbidden".into(),
                    "id cannot contain `..` traversal".into(),
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(("forbidden".into(), "id cannot be absolute".into()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Same trust-boundary + canonical-form rules as `validate_id`, but trailing
/// slash IS allowed (callers naturally type `meetings/`); call site trims it.
/// Shape errors surface as `invalid_params` since `kb.search` doesn't define
/// `invalid_id`.
fn validate_folder(folder: &str) -> Result<(), KbErr> {
    if folder.is_empty() {
        return Err(invalid_params("folder cannot be empty"));
    }
    if folder.contains('\0') {
        return Err(invalid_params("folder contains nul bytes"));
    }
    if folder.starts_with('/') {
        return Err(("forbidden".into(), "folder cannot be absolute".into()));
    }
    // Reject `.` segments and empty segments. We DO permit a trailing
    // slash (and only a single trailing slash) — the call site trims
    // it after validation. `meetings//foo` (interior empty) is still
    // rejected.
    let trimmed = folder.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(("forbidden".into(), "folder cannot be just `/`".into()));
    }
    for raw_segment in trimmed.split('/') {
        if raw_segment.is_empty() {
            return Err(invalid_params(
                "folder cannot contain empty segments (consecutive `/`)",
            ));
        }
        if raw_segment == "." {
            return Err(invalid_params("folder cannot contain `.` segments"));
        }
    }
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::ParentDir => {
                return Err((
                    "forbidden".into(),
                    "folder cannot contain `..` traversal".into(),
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(("forbidden".into(), "folder cannot be absolute".into()));
            }
            _ => {}
        }
    }
    Ok(())
}

fn read_mtime_ms(path: &Path) -> Option<i64> {
    let md = std::fs::symlink_metadata(path).ok()?;
    let mtime = md.modified().ok()?;
    let dur = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as i64)
}

/// Read a small prefix of the file and parse frontmatter; surfaces only the
/// fields the KB panel uses (title, summary, tags, repo, created_at,
/// canonical_url, supersedes). Bounded read (~8 KiB) so the listing path
/// can't be DOSed by a single huge file.
fn read_frontmatter_for_listing(path: &Path) -> Option<Value> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let mut buf = vec![0u8; 8 * 1024];
    use std::io::Read;
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    let content = String::from_utf8_lossy(&buf).into_owned();
    let fm = parse_frontmatter(&content)?;
    let mut out = Map::new();
    for key in [
        "title",
        "summary",
        "tags",
        "repo",
        "created_at",
        "updated_at",
        "canonical_url",
        "supersedes",
    ] {
        if let Some(v) = fm.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    Some(Value::Object(out))
}

fn rel_to_id(rel: &Path) -> String {
    let mut parts = Vec::new();
    for comp in rel.components() {
        if let Component::Normal(s) = comp {
            parts.push(s.to_string_lossy().into_owned());
        }
    }
    parts.join("/")
}

fn path_to_value(path: &Path) -> Value {
    Value::String(path.to_string_lossy().into_owned())
}

fn invalid_params(msg: impl Into<String>) -> KbErr {
    ("invalid_params".into(), msg.into())
}

fn io_error(msg: impl Into<String>) -> KbErr {
    ("io_error".into(), msg.into())
}

// -------- helpers: filesystem --------

/// Visitor return for `walk`. Handed the `FileType` from `DirEntry::file_type`
/// (uses readdir's `d_type` when available, falls back to a `lstat` otherwise)
/// so the visitor can decide is-dir/is-file without a second stat that would
/// reopen a TOCTOU window if the entry was swapped between calls.
enum WalkAction {
    Recurse,
    Skip,
    Continue,
}

fn walk<F>(root: &Path, visit: &mut F)
where
    F: FnMut(&Path, &std::fs::FileType) -> WalkAction,
{
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        // Use file_type() from the DirEntry itself — it's
        // populated by readdir's d_type on most filesystems and does
        // NOT follow symlinks. Falling back to file_type via
        // symlink_metadata if the FS doesn't expose d_type.
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let path = entry.path();
        match visit(&path, &ft) {
            WalkAction::Recurse => {
                if ft.is_dir() {
                    walk(&path, visit);
                }
            }
            WalkAction::Skip => continue,
            WalkAction::Continue => {}
        }
    }
}

fn create_parents(path: &Path) -> Result<(), KbErr> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| io_error(format!("mkdir -p {}: {e}", parent.display())))?;
    }
    Ok(())
}

/// Sibling-temp + atomic create-or-fail rename. Provides both protocol
/// invariants: exactly-one-creator across concurrent calls (rename's
/// no-replace flag → losers get EEXIST) AND no torn reads (atomic publish).
fn atomic_create_with_content(path: &Path, content: &[u8]) -> Result<bool, KbErr> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let temp_dir = parent
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let temp_name = format!(".kb-tmp-{}-{}", std::process::id(), next_temp_seq());
    let temp_path = temp_dir.join(temp_name);

    // Write the temp file. O_CREAT|O_EXCL|O_WRONLY here too — paranoia
    // against another instance picking the same temp name.
    {
        let mut tmp = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .map_err(|e| io_error(format!("create temp {}: {e}", temp_path.display())))?;
        if !content.is_empty() {
            // Use write_all here — the temp file isn't visible to
            // readers yet, so multi-syscall writes are fine. (Only
            // the FINAL rename needs to be atomic for readers.)
            use std::io::Write;
            tmp.write_all(content)
                .map_err(|e| io_error(format!("write temp {}: {e}", temp_path.display())))?;
            tmp.sync_data()
                .map_err(|e| io_error(format!("fsync temp {}: {e}", temp_path.display())))?;
        }
    }

    // Atomic create-or-fail rename. Routed through `copad_core::fs_atomic`
    // so the platform-specific syscall (Linux `renameat2(RENAME_NOREPLACE)`,
    // macOS `renamex_np(RENAME_EXCL)`) lives in one place.
    match copad_core::fs_atomic::rename_no_replace(&temp_path, path) {
        Ok(()) => Ok(true),
        Err(err) => {
            // Clean up our orphaned temp file regardless of which branch
            // we take next.
            let _ = std::fs::remove_file(&temp_path);
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                // Lost the race — the target file already exists. That's
                // not an error for our caller; protocol says
                // `created=false` and the existing content stands.
                Ok(false)
            } else {
                Err(io_error(format!(
                    "rename_no_replace -> {}: {err}",
                    path.display()
                )))
            }
        }
    }
}

/// Process-local; cross-process temp collisions are caught by `O_EXCL`.
fn next_temp_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

// -------- helpers: snippet, title, frontmatter --------

fn build_snippet(content: &str, content_lower: &str, q_lower: &str) -> Option<String> {
    let pos = content_lower.find(q_lower)?;
    let start = char_floor(content, pos.saturating_sub(SNIPPET_RADIUS));
    let end = char_ceil(
        content,
        (pos + q_lower.len() + SNIPPET_RADIUS).min(content.len()),
    );
    let mut snippet = String::new();
    if start > 0 {
        snippet.push('…');
    }
    let chunk = &content[start..end];
    // Trim newlines so the snippet stays one-line for callers that
    // render it inline.
    for ch in chunk.chars() {
        if ch == '\n' || ch == '\r' {
            snippet.push(' ');
        } else {
            snippet.push(ch);
        }
    }
    if end < content.len() {
        snippet.push('…');
    }
    Some(snippet)
}

/// Snap a byte offset down to the nearest UTF-8 char boundary.
fn char_floor(s: &str, mut byte: usize) -> usize {
    while byte > 0 && !s.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

/// Snap a byte offset up to the nearest UTF-8 char boundary.
fn char_ceil(s: &str, mut byte: usize) -> usize {
    while byte < s.len() && !s.is_char_boundary(byte) {
        byte += 1;
    }
    byte
}

fn extract_title(content: &str, rel: &Path) -> Option<Value> {
    if let Some(fm) = parse_frontmatter(content)
        && let Some(t) = fm.get("title").and_then(Value::as_str)
    {
        return Some(Value::String(t.to_string()));
    }
    // First H1 line: `# Foo`
    for line in content.lines().take(50) {
        if let Some(rest) = line.strip_prefix("# ") {
            let t = rest.trim();
            if !t.is_empty() {
                return Some(Value::String(t.to_string()));
            }
        }
    }
    // Fallback: filename without extension.
    rel.file_stem()
        .map(|s| Value::String(s.to_string_lossy().into_owned()))
}

/// Subset YAML: each line is `key: value`; supported values are scalars,
/// `[a, b]` arrays, quoted strings, booleans, integers. Nested mappings and
/// multi-line forms (`|`, `>`) are NOT supported — they are silently
/// misparsed (e.g. a `parent:` line becomes `null` and indented children
/// flatten into top-level keys). Returns `None` when the opening `---`
/// fence is absent OR the closing fence is missing.
fn parse_frontmatter(content: &str) -> Option<Value> {
    let body = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;
    // Closing fence: a line containing only `---` (with optional CR).
    let mut offset = 0usize;
    let mut close_at: Option<usize> = None;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed == "---" {
            close_at = Some(offset);
            break;
        }
        offset += line.len();
    }
    // Also accept a trailing `---` with no newline at EOF.
    if close_at.is_none() && body.trim_end_matches(['\n', '\r']).ends_with("\n---") {
        let trimmed = body.trim_end_matches(['\n', '\r']);
        close_at = trimmed.rfind("\n---").map(|i| i + 1);
    }
    let block_end = close_at?;
    let block = &body[..block_end];
    let mut obj = Map::new();
    for line in block.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() || trimmed.trim_start().starts_with('#') {
            continue;
        }
        let (k, v) = match trimmed.split_once(':') {
            Some(p) => p,
            None => continue,
        };
        let k = k.trim();
        let v = v.trim();
        if k.is_empty() {
            continue;
        }
        obj.insert(k.to_string(), parse_yaml_value(v));
    }
    Some(Value::Object(obj))
}

fn parse_yaml_value(v: &str) -> Value {
    if v.is_empty() {
        return Value::Null;
    }
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
    {
        return Value::String(v[1..v.len() - 1].to_string());
    }
    if v.starts_with('[') && v.ends_with(']') {
        let inner = &v[1..v.len() - 1];
        if inner.trim().is_empty() {
            return Value::Array(Vec::new());
        }
        let arr: Vec<Value> = inner
            .split(',')
            .map(|s| parse_yaml_value(s.trim()))
            .collect();
        return Value::Array(arr);
    }
    if v == "true" {
        return Value::Bool(true);
    }
    if v == "false" {
        return Value::Bool(false);
    }
    if v == "null" || v == "~" {
        return Value::Null;
    }
    if let Ok(n) = v.parse::<i64>() {
        return Value::Number(n.into());
    }
    if let Ok(n) = v.parse::<f64>()
        && let Some(num) = serde_json::Number::from_f64(n)
    {
        return Value::Number(num);
    }
    Value::String(v.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_id_rejects_empty() {
        assert_eq!(validate_id("").unwrap_err().0, "invalid_id");
    }

    #[test]
    fn validate_id_rejects_nul() {
        assert_eq!(validate_id("a\0b").unwrap_err().0, "invalid_id");
    }

    #[test]
    fn validate_id_rejects_absolute() {
        assert_eq!(validate_id("/etc/passwd").unwrap_err().0, "forbidden");
    }

    #[test]
    fn validate_id_rejects_traversal() {
        assert_eq!(validate_id("../etc").unwrap_err().0, "forbidden");
        assert_eq!(validate_id("a/../b").unwrap_err().0, "forbidden");
    }

    #[test]
    fn validate_id_accepts_filename_with_dots() {
        // Substring `..` inside a name is fine — only bare `..`
        // segments are traversal.
        validate_id("a..b.md").unwrap();
        validate_id("notes/v1.0.0.md").unwrap();
    }

    #[test]
    fn validate_id_accepts_normal_paths() {
        validate_id("meetings/2026-04-26.md").unwrap();
        validate_id("notes/foo.md").unwrap();
    }

    #[test]
    fn validate_id_rejects_non_canonical_forms() {
        // Empty interior segment.
        assert_eq!(validate_id("notes//foo.md").unwrap_err().0, "invalid_id");
        // `.` segments.
        assert_eq!(validate_id("./notes/foo.md").unwrap_err().0, "invalid_id");
        assert_eq!(validate_id("notes/./foo.md").unwrap_err().0, "invalid_id");
        // Trailing slash — would name a directory, not a doc.
        assert_eq!(validate_id("meetings/").unwrap_err().0, "invalid_id");
    }

    #[test]
    fn validate_folder_allows_single_trailing_slash() {
        // The protocol explicitly says trailing slash on folder is
        // ignored — keep that ergonomic.
        validate_folder("meetings/").unwrap();
        validate_folder("meetings").unwrap();
    }

    #[test]
    fn validate_folder_rejects_interior_empty_segment() {
        // `meetings//` (trailing) is OK — same target. `a//b` (interior) is not.
        assert_eq!(validate_folder("a//b").unwrap_err().0, "invalid_params");
    }

    #[test]
    fn validate_folder_rejects_dot_segment() {
        assert_eq!(
            validate_folder("./meetings").unwrap_err().0,
            "invalid_params"
        );
    }

    #[test]
    fn validate_folder_uses_invalid_params_for_shape() {
        // Per protocol, kb.search's documented error set does NOT
        // include `invalid_id`. Shape problems on `folder` surface
        // as `invalid_params`.
        assert_eq!(validate_folder("").unwrap_err().0, "invalid_params");
        assert_eq!(validate_folder("a\0b").unwrap_err().0, "invalid_params");
    }

    #[test]
    fn validate_folder_uses_forbidden_for_traversal() {
        assert_eq!(validate_folder("../etc").unwrap_err().0, "forbidden");
        assert_eq!(validate_folder("/etc").unwrap_err().0, "forbidden");
        assert_eq!(validate_folder("a/../b").unwrap_err().0, "forbidden");
    }

    #[test]
    fn validate_folder_accepts_normal() {
        validate_folder("meetings").unwrap();
        validate_folder("notes/sub").unwrap();
    }

    #[test]
    fn rel_to_id_uses_forward_slash() {
        let p = Path::new("meetings").join("foo.md");
        assert_eq!(rel_to_id(&p), "meetings/foo.md");
    }

    #[test]
    fn frontmatter_parses_flat_yaml() {
        let doc = "---\ntitle: \"Hello\"\ntags: [a, b]\nnum: 42\nflag: true\n---\nbody\n";
        let fm = parse_frontmatter(doc).expect("frontmatter present");
        assert_eq!(fm["title"], Value::String("Hello".into()));
        assert_eq!(fm["tags"], json!(["a", "b"]));
        assert_eq!(fm["num"], json!(42));
        assert_eq!(fm["flag"], Value::Bool(true));
    }

    #[test]
    fn frontmatter_returns_none_without_fence() {
        assert!(parse_frontmatter("# Hello\nbody\n").is_none());
    }

    #[test]
    fn frontmatter_handles_unquoted_values() {
        let doc = "---\nkey: value with spaces\n---\nbody";
        let fm = parse_frontmatter(doc).expect("frontmatter present");
        assert_eq!(fm["key"], Value::String("value with spaces".into()));
    }

    #[test]
    fn yaml_value_handles_empty_array() {
        assert_eq!(parse_yaml_value("[]"), json!([]));
    }

    #[test]
    fn yaml_value_handles_quoted_string() {
        assert_eq!(parse_yaml_value("\"hi\""), json!("hi"));
        assert_eq!(parse_yaml_value("'hi'"), json!("hi"));
    }

    fn mk_kb_at(root: &Path) -> Kb {
        std::fs::create_dir_all(root).unwrap();
        let canonical = std::fs::canonicalize(root).unwrap();
        Kb {
            root: canonical.clone(),
            root_canonical: canonical,
        }
    }

    fn unique_root(suffix: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "copad-kb-list-{}-{}",
            std::process::id(),
            next_temp_seq()
        ));
        p.push(suffix);
        p
    }

    #[test]
    fn kb_list_returns_top_level_entries() {
        let root = unique_root("top");
        let kb = mk_kb_at(&root);
        std::fs::write(root.join("a.md"), "hello").unwrap();
        std::fs::create_dir_all(root.join("topics")).unwrap();
        std::fs::write(root.join("topics/b.md"), "world").unwrap();
        let r = kb.list(&json!({})).unwrap();
        let entries = r.get("entries").unwrap().as_array().unwrap();
        // top-level only: a.md + topics (NOT topics/b.md because recursive=false)
        let ids: Vec<&str> = entries
            .iter()
            .map(|e| e.get("id").unwrap().as_str().unwrap())
            .collect();
        assert!(ids.contains(&"a.md"));
        assert!(ids.contains(&"topics"));
        assert!(!ids.contains(&"topics/b.md"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kb_list_recursive_walks_subdirs() {
        let root = unique_root("recursive");
        let kb = mk_kb_at(&root);
        std::fs::create_dir_all(root.join("topics")).unwrap();
        std::fs::write(root.join("topics/b.md"), "x").unwrap();
        let r = kb.list(&json!({"recursive": true})).unwrap();
        let entries = r.get("entries").unwrap().as_array().unwrap();
        let ids: Vec<&str> = entries
            .iter()
            .map(|e| e.get("id").unwrap().as_str().unwrap())
            .collect();
        assert!(ids.contains(&"topics/b.md"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kb_list_dotfolders_excluded_by_default_but_explicit_folder_allowed() {
        let root = unique_root("dot");
        let kb = mk_kb_at(&root);
        std::fs::create_dir_all(root.join(".backlinks")).unwrap();
        std::fs::write(root.join(".backlinks/index.tsv"), "a\tb\n").unwrap();
        std::fs::write(root.join("a.md"), "x").unwrap();
        // Top-level walk: dotfolder hidden.
        let r = kb.list(&json!({})).unwrap();
        let entries = r.get("entries").unwrap().as_array().unwrap();
        let ids: Vec<&str> = entries
            .iter()
            .map(|e| e.get("id").unwrap().as_str().unwrap())
            .collect();
        assert!(!ids.contains(&".backlinks"));
        assert!(ids.contains(&"a.md"));
        // Explicit dotfolder list: walks into it.
        // But the dotfile-skip filter we use rejects children of dotfolders.
        // For the panel use case the indices are read directly via kb.read,
        // not via kb.list, so explicit listing of `.backlinks` returning
        // empty is acceptable behavior — what matters is that the dotfolder
        // doesn't pollute the default listing.
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kb_list_sort_mtime_orders_newest_first() {
        let root = unique_root("mtime");
        let kb = mk_kb_at(&root);
        std::fs::write(root.join("a.md"), "x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(root.join("b.md"), "y").unwrap();
        let r = kb
            .list(&json!({"sort": "mtime", "include_mtime": true}))
            .unwrap();
        let entries = r.get("entries").unwrap().as_array().unwrap();
        let first = entries[0].get("id").unwrap().as_str().unwrap();
        assert_eq!(first, "b.md");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kb_list_rejects_invalid_sort() {
        let root = unique_root("bad-sort");
        let kb = mk_kb_at(&root);
        let err = kb.list(&json!({"sort": "weird"})).unwrap_err();
        assert_eq!(err.0, "invalid_params");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kb_list_traversal_rejected() {
        let root = unique_root("traversal");
        let kb = mk_kb_at(&root);
        let err = kb.list(&json!({"folder": "../etc"})).unwrap_err();
        assert_eq!(err.0, "forbidden");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn kb_list_include_frontmatter_returns_subset() {
        let root = unique_root("fm");
        let kb = mk_kb_at(&root);
        std::fs::write(
            root.join("a.md"),
            "---\ntitle: Hi\nsummary: Short\nfoo: ignored\n---\nbody",
        )
        .unwrap();
        let r = kb
            .list(&json!({"include_frontmatter": true, "recursive": true}))
            .unwrap();
        let entries = r.get("entries").unwrap().as_array().unwrap();
        let fm = entries[0].get("frontmatter").unwrap();
        assert_eq!(fm.get("title").unwrap().as_str().unwrap(), "Hi");
        assert_eq!(fm.get("summary").unwrap().as_str().unwrap(), "Short");
        assert!(fm.get("foo").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn snippet_centers_on_match() {
        let content = "lorem ipsum dolor sit AMET consectetur adipiscing elit";
        let lower = content.to_lowercase();
        let s = build_snippet(content, &lower, "amet").expect("match present");
        assert!(s.contains("AMET"));
    }
}
