//! Serve the workstation's terminal font to the PWA.
//!
//! A phone has no way to install a Nerd Font, so without this every powerline /
//! devicon glyph renders as tofu. The font is served at runtime from whatever the
//! workstation already has installed (resolved from `[terminal] font_family`)
//! rather than bundled into the repo — that keeps a multi-MB binary out of git and
//! avoids vendoring a font whose license we'd then be redistributing. The bytes
//! still travel from the user's machine to the user's browser; runtime serving
//! doesn't launder that, it just doesn't put it in our tree.
//!
//! **The endpoint is public** (see the `public` allowlist in `main.rs`) because a
//! CSS `@font-face` fetch carries no `Authorization` header — an authed font URL
//! would 401 on the bearer path and leave the glyphs broken, i.e. the bug this
//! exists to fix. That makes validation load-bearing rather than cosmetic: the
//! path comes from local config, but a misconfigured override must not turn a
//! public endpoint into an arbitrary-file disclosure. Hence `load()` accepts only
//! a regular file, carrying real font magic, under a sane size ceiling.

use std::path::{Path, PathBuf};

/// Refuse anything larger. Real fonts are single-digit MB; this stops a
/// misconfigured override from streaming something huge.
const MAX_FONT_BYTES: u64 = 32 * 1024 * 1024;

/// A validated font, held in memory with its content hash.
#[derive(Clone)]
pub struct Font {
    pub bytes: Vec<u8>,
    /// Strong ETag payload — a hash of `bytes`, NOT mtime/size. mtime+size can
    /// stay identical across a byte change, which would pin a stale font in every
    /// client forever; the hash cannot.
    pub etag: String,
    pub content_type: &'static str,
    pub path: PathBuf,
}

impl std::fmt::Debug for Font {
    /// Hand-rolled: a derived Debug would dump megabytes of glyph data into any
    /// test failure or log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Font")
            .field("path", &self.path)
            .field("bytes", &format_args!("{} B", self.bytes.len()))
            .field("etag", &self.etag)
            .field("content_type", &self.content_type)
            .finish()
    }
}

/// Font magic → MIME. Also the validation gate: anything not on this list is not a
/// font, so `/etc/passwd` (or any other readable file) fails before a byte is served.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    match bytes.get(..4)? {
        b"wOF2" => Some("font/woff2"),
        b"wOFF" => Some("font/woff"),
        b"OTTO" => Some("font/otf"),
        b"ttcf" => Some("font/collection"),
        b"true" | b"\x00\x01\x00\x00" => Some("font/ttf"),
        _ => None,
    }
}

/// Normalize a font family/PostScript name for comparison: `fc-match` legitimately
/// answers with aliases and PostScript spellings ("JetBrainsMono Nerd Font Mono"
/// vs "JetBrainsMonoNF-Regular"), so an exact compare would reject valid hits.
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// True if `candidate` plausibly IS `wanted`.
///
/// The guard exists because `fc-match` NEVER fails — it substitutes a default. Without
/// this we'd cheerfully serve DejaVu Sans and the user would see the exact tofu they
/// reported, with nothing in the log to explain it.
pub fn family_matches(wanted: &str, candidate_family: &str, candidate_ps: &str) -> bool {
    let w = norm(wanted);
    if w.is_empty() {
        return false;
    }
    let f = norm(candidate_family);
    let p = norm(candidate_ps);
    // The candidate must be the wanted family or MORE specific — "…Mono" legitimately
    // answers as "…MonoRegular". Deliberately NOT the reverse: accepting a shorter
    // candidate would accept "JetBrains Mono" for "JetBrainsMono Nerd Font Mono", i.e.
    // the plain family fontconfig substitutes when the Nerd variant is missing. That
    // serves a glyph-less font under our own @font-face name, so the browser reports
    // success and the phone still shows tofu — the exact bug this module exists to fix,
    // minus the diagnostic.
    f.starts_with(&w) || p.starts_with(&w)
}

/// Read + validate a font file. Opens ONCE and validates the bytes it will actually
/// serve, so a symlink or file swap between check and read can't slip past.
pub fn load(path: &Path) -> Result<Font, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.len() > MAX_FONT_BYTES {
        return Err(format!(
            "{} is {} bytes, over the {MAX_FONT_BYTES} ceiling",
            path.display(),
            meta.len()
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let content_type =
        sniff(&bytes).ok_or_else(|| format!("{} is not a font (bad magic)", path.display()))?;

    use sha2::{Digest, Sha256};
    let etag = format!("\"{:x}\"", Sha256::digest(&bytes));

    Ok(Font {
        bytes,
        etag,
        content_type,
        path: path.to_path_buf(),
    })
}

/// Resolve the terminal font's file, in precedence order:
/// 1. `COPAD_WEB_BRIDGE_FONT` — absolute-path override / escape hatch.
/// 2. `fc-match` on the configured `[terminal] font_family` (Linux; macOS if
///    fontconfig happens to be installed).
/// 3. macOS font directories, matched by filename (no fontconfig by default there).
///
/// `None` → the endpoint 404s and the PWA keeps its existing fallback stack, i.e.
/// today's behaviour. Every failure logs once so it's diagnosable.
pub fn resolve(family: &str) -> Option<PathBuf> {
    resolve_with_override(family, "COPAD_WEB_BRIDGE_FONT")
}

/// Resolve `family`, honouring a path override from `override_var`.
///
/// The override variable is a PARAMETER because the terminal font and the symbol font must not
/// share one. `COPAD_WEB_BRIDGE_FONT` is a supported escape hatch that pins an exact file; if
/// symbol resolution read it too, anyone using it would be served the same glyph-incomplete
/// file from both endpoints and the icons would stay broken with no indication why.
pub fn resolve_with_override(family: &str, override_var: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(override_var) {
        let path = PathBuf::from(&p);
        if !path.is_absolute() {
            eprintln!("[web-bridge] {override_var} must be absolute: {p}");
            return None;
        }
        return Some(path);
    }
    if let Some(p) = fc_match(family) {
        return Some(p);
    }
    macos_scan(family)
}

/// Ask fontconfig, then verify it didn't just hand back its substitute.
fn fc_match(family: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("fc-match")
        .arg("-f")
        .arg("%{file}\t%{family}\t%{postscriptname}")
        .arg(family)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut it = s.split('\t');
    let file = it.next()?.trim();
    let fam = it.next().unwrap_or("").trim();
    let ps = it.next().unwrap_or("").trim();
    if file.is_empty() {
        return None;
    }
    if !family_matches(family, fam, ps) {
        eprintln!(
            "[web-bridge] fc-match substituted {fam:?} for {family:?} — not serving it; \
             install the font or set COPAD_WEB_BRIDGE_FONT"
        );
        return None;
    }
    Some(PathBuf::from(file))
}

/// macOS has no fc-match by default. Best-effort filename match.
/// How much we want a given file, given the family we asked for. LOWER is better.
///
/// A family like `Jetendard` has a dozen files on disk and `read_dir` hands them back in
/// whatever order the filesystem likes. Taking the first match served
/// `Jetendard-SemiBoldItalic` to the phone — every glyph in semibold italic — which is how this
/// was found. Rank instead, so the weight the user actually configured wins regardless of
/// directory order.
pub fn variant_rank(stem: &str, want: &str) -> u8 {
    let rest = norm(stem)
        .strip_prefix(want)
        .map(str::to_string)
        .unwrap_or_default();
    match rest.as_str() {
        // "Jetendard" itself, or a single-file family.
        "" => 0,
        "regular" => 1,
        // Anything else is a named variant. Italic is worse than a plain weight, because a
        // whole terminal in italic is worse than a whole terminal in the wrong weight.
        r if r.contains("italic") => 3,
        _ => 2,
    }
}

fn macos_scan(family: &str) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let want = norm(family);
    let home = std::env::var("HOME").ok()?;
    let dirs = [
        PathBuf::from(&home).join("Library/Fonts"),
        PathBuf::from("/Library/Fonts"),
        PathBuf::from("/System/Library/Fonts"),
    ];
    let mut best: Option<(u8, PathBuf)> = None;
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            // NOT `?`: a single stem-less entry used to abort the entire scan, taking every
            // later directory with it.
            let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().to_string()) else {
                continue;
            };
            if !norm(&stem).starts_with(&want) {
                continue;
            }
            let rank = variant_rank(&stem, &want);
            if best.as_ref().is_none_or(|(b, _)| rank < *b) {
                best = Some((rank, p));
            }
            if rank == 0 {
                break;
            }
        }
    }
    best.map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this encodes: `read_dir` order decided the variant, and it picked
    /// `Jetendard-SemiBoldItalic` — so the phone rendered every glyph in semibold italic.
    #[test]
    fn regular_beats_every_other_variant_regardless_of_directory_order() {
        let want = norm("Jetendard");
        let mut ranked = [
            "Jetendard-SemiBoldItalic",
            "Jetendard-Bold",
            "Jetendard-Regular",
            "Jetendard-Light",
        ];
        ranked.sort_by_key(|s| variant_rank(s, &want));
        assert_eq!(ranked[0], "Jetendard-Regular");
        // Italic ranks below a plain weight: a whole terminal in italic is worse than one in
        // the wrong weight.
        assert!(
            variant_rank("Jetendard-Bold", &want) < variant_rank("Jetendard-BoldItalic", &want)
        );
    }

    #[test]
    fn a_single_file_family_with_no_suffix_wins_outright() {
        let want = norm("Monaco");
        assert_eq!(variant_rank("Monaco", &want), 0);
        assert!(variant_rank("Monaco", &want) < variant_rank("MonacoBold", &want));
    }

    #[test]
    fn sniff_accepts_real_font_magic() {
        assert_eq!(sniff(b"wOF2....."), Some("font/woff2"));
        assert_eq!(sniff(b"OTTO...."), Some("font/otf"));
        assert_eq!(sniff(b"\x00\x01\x00\x00...."), Some("font/ttf"));
        assert_eq!(sniff(b"true...."), Some("font/ttf"));
        assert_eq!(sniff(b"ttcf...."), Some("font/collection"));
    }

    #[test]
    fn sniff_rejects_non_fonts() {
        // The endpoint is public, so this is the gate that stops a misconfigured
        // override from disclosing arbitrary readable bytes.
        assert_eq!(sniff(b"root:x:0:0:root:/root:/bin/bash"), None);
        assert_eq!(sniff(b"#!/bin/sh\necho hi"), None);
        assert_eq!(sniff(b"\x7fELF\x02\x01\x01"), None);
        assert_eq!(sniff(b"<html>"), None);
        assert_eq!(sniff(b""), None);
        assert_eq!(sniff(b"ab"), None); // shorter than the magic
    }

    #[test]
    fn family_matches_accepts_aliases_and_ps_names() {
        assert!(family_matches(
            "JetBrainsMono Nerd Font Mono",
            "JetBrainsMono Nerd Font Mono",
            "JetBrainsMonoNFM-Regular"
        ));
        // fc-match commonly answers with the concrete face
        assert!(family_matches(
            "JetBrainsMono Nerd Font Mono",
            "JetBrainsMono Nerd Font Mono Regular",
            ""
        ));
        // case + separators are noise
        assert!(family_matches(
            "jetbrainsmono nerd font mono",
            "JetBrainsMonoNerdFontMono",
            ""
        ));
    }

    #[test]
    fn family_matches_rejects_the_fontconfig_substitute() {
        // The whole point: fc-match never fails, it substitutes. Serving DejaVu here
        // would reproduce the exact tofu bug with no diagnostic.
        assert!(!family_matches(
            "JetBrainsMono Nerd Font Mono",
            "DejaVu Sans",
            "DejaVuSans"
        ));
        assert!(!family_matches(
            "JetBrainsMono Nerd Font Mono",
            "Noto Color Emoji",
            ""
        ));
        assert!(!family_matches("", "DejaVu Sans", ""));
    }

    #[test]
    fn family_matches_rejects_the_non_nerd_base_family() {
        // The subtle one: fontconfig answers "JetBrains Mono" when the Nerd variant
        // isn't installed. It's a prefix of what we asked for, so a naive two-way
        // prefix check accepts it — and then we serve a font with no powerline glyphs
        // under the CopadTerminal name and the phone still renders tofu.
        assert!(!family_matches(
            "JetBrainsMono Nerd Font Mono",
            "JetBrains Mono",
            "JetBrainsMono-Regular"
        ));
        assert!(!family_matches("Hack Nerd Font", "Hack", "Hack-Regular"));
        // ...while the genuinely-more-specific face still passes.
        assert!(family_matches(
            "JetBrainsMono Nerd Font Mono",
            "JetBrainsMono Nerd Font Mono Regular",
            ""
        ));
    }

    #[test]
    fn load_rejects_a_non_font_file() {
        let dir = std::env::temp_dir().join(format!("copad-font-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("passwd");
        std::fs::write(&p, b"root:x:0:0:root:/root:/bin/bash\n").unwrap();
        let err = load(&p).unwrap_err();
        assert!(err.contains("not a font"), "got: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_rejects_a_directory() {
        let err = load(&std::env::temp_dir()).unwrap_err();
        assert!(err.contains("not a regular file"), "got: {err}");
    }

    #[test]
    fn load_hashes_content_for_the_etag() {
        let dir = std::env::temp_dir().join(format!("copad-font-etag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.ttf");
        let b = dir.join("b.ttf");
        std::fs::write(&a, b"\x00\x01\x00\x00AAAA").unwrap();
        std::fs::write(&b, b"\x00\x01\x00\x00BBBB").unwrap();
        let fa = load(&a).unwrap();
        let fb = load(&b).unwrap();
        // Same length, likely same mtime — an mtime+size ETag would collide here and
        // pin a stale font in every client. The content hash must not.
        assert_ne!(fa.etag, fb.etag);
        assert_eq!(fa.content_type, "font/ttf");
        std::fs::remove_dir_all(&dir).ok();
    }
}
