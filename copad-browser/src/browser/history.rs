//! Opaque per-tab history blobs: the platform's serialized back/forward list and scroll
//! position (macOS `WKWebView.interactionState`, WebKitGTK `WebKitWebViewSessionState`). Neither
//! is documented and both contain full URLs, so a blob is sensitive persistence, written only
//! under `RestorePolicy::Full`, and every rule about where it lands lives here.
//!
//! - A session document is never filesystem authority: the path is rebuilt from a validated tab
//!   id and generation, and reads refuse to follow a symlink planted at the expected name.
//! - Generations are immutable: overwriting `<id>.bin` would destroy the blob the previous
//!   committed session references. `gc` reclaims old ones after a session commits.
//! - Oversize is refused, never truncated: a truncated `interactionState` fails to decode in
//!   WebKit, silently.
//! - `gc` removes only files matching the exact `<id>-<generation>.bin` grammar that no live tab
//!   references.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use super::tabs::{history_dir, history_path, is_valid_tab_id};

/// Ceiling on one blob: a real `interactionState` is single-digit KB, and a corrupt length must
/// not force a huge allocation.
pub const MAX_BLOB_BYTES: usize = 4 * 1024 * 1024;

const DIR_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;

/// Write a blob as a NEW generation: temp, fsync, rename, parent fsync, so a reader sees the
/// previous generation or this one, never a half-written file. Write `session.json` only after
/// this returns.
pub fn write(tab_id: &str, generation: u64, data: &[u8]) -> Result<(), String> {
    if data.len() > MAX_BLOB_BYTES {
        return Err(format!(
            "history blob is {} bytes, over the {MAX_BLOB_BYTES}-byte cap",
            data.len()
        ));
    }
    let path =
        history_path(tab_id, generation).ok_or_else(|| format!("invalid tab id: {tab_id:?}"))?;
    let dir = history_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    // Best-effort: an existing dir keeps its mode; narrow it rather than leave it world-readable.
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(DIR_MODE));

    // The temp name is predictable, so creation must be hostile-proof: `O_EXCL | O_NOFOLLOW`
    // refuses anything already there, including a planted symlink that a plain create+truncate
    // would follow and the rename would then install as the history file. The read-side
    // `O_NOFOLLOW` does not cover this.
    let tmp = dir.join(format!(".{tab_id}-{generation}.bin.tmp"));
    let _ = fs::remove_file(&tmp);
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(libc::O_NOFOLLOW)
            .mode(FILE_MODE)
            .open(&tmp)
            .map_err(|e| format!("open {}: {e}", tmp.display()))?;
        f.write_all(data)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.sync_all()
            .map_err(|e| format!("fsync {}: {e}", tmp.display()))?;
    }
    // NON-REPLACING rename: core must enforce immutable generations itself, since a caller that
    // reused a generation (a bug, or a second process) would otherwise destroy the blob a
    // committed session still points at.
    crate::fs_atomic::rename_no_replace(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!(
                "history generation {generation} for {tab_id:?} already exists — generations are immutable"
            )
        } else {
            format!("rename into {}: {e}", path.display())
        }
    })?;
    if let Ok(d) = fs::File::open(&dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Read a blob back. `O_NOFOLLOW` plus a parent check: the file may have been replaced with a
/// symlink pointing anywhere, which would turn "restore my tabs" into an arbitrary-file read.
pub fn read(tab_id: &str, generation: u64) -> Result<Vec<u8>, String> {
    let path =
        history_path(tab_id, generation).ok_or_else(|| format!("invalid tab id: {tab_id:?}"))?;
    if path.parent() != Some(history_dir().as_path()) {
        return Err("history path escaped its directory".to_string());
    }
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;

    let meta = f
        .metadata()
        .map_err(|e| format!("stat {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err("history blob is not a regular file".to_string());
    }
    if meta.len() as usize > MAX_BLOB_BYTES {
        return Err(format!(
            "history blob is {} bytes, over the {MAX_BLOB_BYTES}-byte cap",
            meta.len()
        ));
    }
    // Read one byte past the cap so a file that grew between stat and read is caught, not
    // silently truncated.
    let mut buf = Vec::with_capacity(meta.len() as usize);
    f.take(MAX_BLOB_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    if buf.len() > MAX_BLOB_BYTES {
        return Err("history blob grew past the cap while being read".to_string());
    }
    Ok(buf)
}

/// Remove blobs no live tab references; returns how many. Only files matching
/// `<id>-<generation>.bin` are candidates, so anything else in the directory is left alone.
pub fn gc(live: &[(String, u64)]) -> usize {
    let dir = history_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some((id, generation)) = parse_blob_name(name) else {
            continue;
        };
        if live.iter().any(|(l, g)| l == &id && *g == generation) {
            continue;
        }
        if fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// `<id>-<generation>.bin` to `(id, generation)`, or `None`. The id is re-validated so a hostile
/// name like `../evil-1.bin` is not treated as a blob.
fn parse_blob_name(name: &str) -> Option<(String, u64)> {
    let stem = name.strip_suffix(".bin")?;
    let (id, gen_str) = stem.rsplit_once('-')?;
    if !is_valid_tab_id(id) {
        return None;
    }
    let generation: u64 = gen_str.parse().ok()?;
    Some((id.to_string(), generation))
}

pub fn exists(tab_id: &str, generation: u64) -> bool {
    history_path(tab_id, generation)
        .map(|p| p.is_file())
        .unwrap_or(false)
}

// Hex rather than base64 because the blob crosses a JSON FFI boundary: `copad-core` has no base64
// dependency, and a second half-written codec to save a few KB is the wrong trade.

pub fn hex_encode(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

pub fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("hex string has an odd length".to_string());
    }
    if s.len() / 2 > MAX_BLOB_BYTES {
        return Err(format!(
            "hex payload decodes to {} bytes, over the {MAX_BLOB_BYTES}-byte cap",
            s.len() / 2
        ));
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    // `as_chunks::<2>()` rather than `chunks_exact(2)`: the chunk size is in the type, so
    // `pair[0]`/`pair[1]` are compile-time-checked (clippy::chunks_exact_to_as_chunks). The
    // odd-length guard above makes the remainder always empty.
    let (pairs, _) = bytes.as_chunks::<2>();
    for pair in pairs {
        let hi = hex_val(pair[0])?;
        let lo = hex_val(pair[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_val(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("invalid hex digit: {:?}", b as char)),
    }
}

/// Test seam: `history_dir()` reads the state root, which tests move with `replace_state_root`.
pub fn dir() -> PathBuf {
    history_dir()
}

/// Is `path` inside the history directory? For asserting confinement before acting on a path.
pub fn is_confined(path: &Path) -> bool {
    path.parent() == Some(history_dir().as_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    struct Sandbox {
        _guard: MutexGuard<'static, ()>,
        root: PathBuf,
        prev_root: Option<PathBuf>,
    }

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let guard = super::super::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let root = std::env::temp_dir()
                .join(format!("copad-history-test-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            let prev_root = super::super::replace_state_root(Some(root.join("state")));
            Self {
                _guard: guard,
                root,
                prev_root,
            }
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            super::super::replace_state_root(self.prev_root.take());
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_blob_round_trips() {
        let _s = Sandbox::new("roundtrip");
        let data = b"\x00\x01\xfe\xff opaque interaction state".to_vec();
        write("tab-a", 1, &data).expect("write");
        assert_eq!(read("tab-a", 1).expect("read"), data);
        assert!(exists("tab-a", 1));
        assert!(!exists("tab-a", 2));
    }

    #[test]
    fn a_blob_is_written_0600_in_a_0700_directory() {
        let _s = Sandbox::new("modes");
        write("tab-a", 1, b"x").expect("write");
        let f = fs::metadata(history_path("tab-a", 1).unwrap()).unwrap();
        assert_eq!(f.permissions().mode() & 0o777, FILE_MODE);
        let d = fs::metadata(dir()).unwrap();
        assert_eq!(d.permissions().mode() & 0o777, DIR_MODE);
    }

    #[test]
    fn a_traversal_id_cannot_write_or_read_anywhere() {
        let _s = Sandbox::new("traversal");
        assert!(write("../../etc/passwd", 1, b"x").is_err());
        assert!(read("../../etc/passwd", 1).is_err());
        assert!(!exists("../../etc/passwd", 1));
    }

    #[test]
    fn a_symlink_planted_at_the_blob_name_is_not_followed() {
        let _s = Sandbox::new("symlink");
        write("tab-a", 1, b"real").expect("write");
        let secret = dir().parent().unwrap().join("secret.txt");
        fs::write(&secret, b"TOP SECRET").unwrap();
        let target = history_path("tab-a", 1).unwrap();
        fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink(&secret, &target).unwrap();

        let err = read("tab-a", 1).unwrap_err();
        assert!(err.contains("open"), "{err}");
        assert!(!err.contains("TOP SECRET"));
    }

    #[test]
    fn an_oversize_blob_is_refused_rather_than_truncated() {
        let _s = Sandbox::new("oversize");
        let big = vec![0u8; MAX_BLOB_BYTES + 1];
        let err = write("tab-a", 1, &big).unwrap_err();
        assert!(err.contains("over the"), "{err}");
        assert!(!exists("tab-a", 1));
    }

    #[test]
    fn a_missing_blob_reads_as_an_error_not_an_empty_history() {
        let _s = Sandbox::new("missing");
        assert!(read("tab-a", 9).is_err());
    }

    #[test]
    fn rewriting_an_existing_generation_is_refused_not_silently_overwritten() {
        let _s = Sandbox::new("immutable");
        write("tab-a", 1, b"first").unwrap();
        let err = write("tab-a", 1, b"second").unwrap_err();
        assert!(err.contains("immutable"), "{err}");
        assert_eq!(read("tab-a", 1).unwrap(), b"first");
    }

    #[test]
    fn a_new_generation_leaves_the_previous_one_intact() {
        let _s = Sandbox::new("generations");
        write("tab-a", 1, b"one").unwrap();
        write("tab-a", 2, b"two").unwrap();
        assert_eq!(read("tab-a", 1).unwrap(), b"one");
        assert_eq!(read("tab-a", 2).unwrap(), b"two");
    }

    #[test]
    fn gc_removes_only_unreferenced_blobs() {
        let _s = Sandbox::new("gc");
        write("tab-a", 1, b"old").unwrap();
        write("tab-a", 2, b"new").unwrap();
        write("tab-b", 1, b"other").unwrap();
        let removed = gc(&[("tab-a".into(), 2), ("tab-b".into(), 1)]);
        assert_eq!(removed, 1);
        assert!(!exists("tab-a", 1));
        assert!(exists("tab-a", 2));
        assert!(exists("tab-b", 1));
    }

    #[test]
    fn gc_leaves_anything_that_is_not_a_blob_alone() {
        let _s = Sandbox::new("gc-foreign");
        write("tab-a", 1, b"x").unwrap();
        let stray = dir().join("notes.txt");
        fs::write(&stray, b"keep me").unwrap();
        let odd = dir().join("tab-a-notanumber.bin");
        fs::write(&odd, b"keep me too").unwrap();

        assert_eq!(gc(&[]), 1);
        assert!(stray.is_file());
        assert!(odd.is_file());
    }

    #[test]
    fn gc_on_a_missing_directory_is_a_no_op() {
        let _s = Sandbox::new("gc-nodir");
        assert_eq!(gc(&[]), 0);
    }

    #[test]
    fn blob_names_parse_only_in_the_exact_grammar() {
        assert_eq!(parse_blob_name("tab-a-3.bin"), Some(("tab-a".into(), 3)));
        assert_eq!(parse_blob_name("tab-a-3.txt"), None);
        assert_eq!(parse_blob_name("tab-a.bin"), None);
        assert_eq!(parse_blob_name("tab-a-x.bin"), None);
        assert_eq!(parse_blob_name(".tab-a-1.bin.tmp"), None);
    }

    #[test]
    fn hex_round_trips_every_byte_value() {
        let all: Vec<u8> = (0u8..=255).collect();
        assert_eq!(hex_decode(&hex_encode(&all)).unwrap(), all);
        assert_eq!(hex_encode(&[]), "");
        assert_eq!(hex_decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hex_rejects_malformed_input_rather_than_guessing() {
        assert!(hex_decode("abc").is_err());
        assert!(hex_decode("zz").is_err());
        assert!(hex_decode("00 11").is_err());
        assert_eq!(hex_decode("FF").unwrap(), vec![0xff]);
    }

    #[test]
    fn hex_decode_refuses_a_payload_over_the_blob_cap() {
        let huge = "ab".repeat(MAX_BLOB_BYTES + 1);
        assert!(hex_decode(&huge).is_err());
    }

    #[test]
    fn confinement_helper_agrees_with_the_path_builder() {
        let _s = Sandbox::new("confined");
        assert!(is_confined(&history_path("tab-a", 1).unwrap()));
        assert!(!is_confined(Path::new("/etc/passwd")));
    }
}
