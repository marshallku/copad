//! Serve the workstation's terminal font to the PWA, so powerline / devicon glyphs do not render
//! as tofu on a phone that cannot install a Nerd Font. Served at runtime from the installed font
//! (resolved from `[terminal] font_family`), not bundled, to keep a multi-MB binary and a
//! redistributed font out of the repo.
//!
//! The endpoint is public (see the `public` allowlist in `main.rs`) because a CSS `@font-face`
//! fetch carries no `Authorization` header. That makes validation load-bearing: a misconfigured
//! override must not turn it into arbitrary-file disclosure, so `load()` accepts only a regular
//! file with real font magic under a size ceiling.

use std::path::{Path, PathBuf};

/// Refuse anything larger; real fonts are single-digit MB.
const MAX_FONT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone)]
pub struct Font {
    pub bytes: Vec<u8>,
    /// Strong ETag payload: a hash of `bytes`, not mtime/size, which can stay identical across a
    /// byte change and pin a stale font in every client.
    pub etag: String,
    pub content_type: &'static str,
    pub path: PathBuf,
}

impl std::fmt::Debug for Font {
    /// Hand-rolled: a derived Debug would dump megabytes of glyph data into logs and test failures.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Font")
            .field("path", &self.path)
            .field("bytes", &format_args!("{} B", self.bytes.len()))
            .field("etag", &self.etag)
            .field("content_type", &self.content_type)
            .finish()
    }
}

/// Font magic -> MIME. Also the validation gate: `/etc/passwd` or any non-font fails before a byte is served.
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

/// Normalize a family/PostScript name for comparison: `fc-match` answers with aliases and
/// PostScript spellings ("JetBrainsMono Nerd Font Mono" vs "JetBrainsMonoNF-Regular").
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// True if `candidate` plausibly is `wanted`. Needed because `fc-match` never fails: it
/// substitutes a default, and serving DejaVu Sans would show the tofu with nothing in the log.
pub fn family_matches(wanted: &str, candidate_family: &str, candidate_ps: &str) -> bool {
    let w = norm(wanted);
    if w.is_empty() {
        return false;
    }
    let f = norm(candidate_family);
    let p = norm(candidate_ps);
    // The candidate must be the wanted family or more specific ("...Mono" answers as
    // "...MonoRegular"), not the reverse: accepting a shorter one would take "JetBrains Mono" for
    // "JetBrainsMono Nerd Font Mono", the glyph-less family fontconfig substitutes, served under
    // our @font-face name so the browser reports success and the phone still shows tofu.
    f.starts_with(&w) || p.starts_with(&w)
}

/// The regular-file and size checks read path metadata before a separate read, so a swap in
/// between can pass them; the font magic is checked on the bytes actually served.
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
/// 1. `COPAD_WEB_BRIDGE_FONT`: absolute-path override.
/// 2. `fc-match` on `[terminal] font_family` (Linux; macOS if fontconfig is installed).
/// 3. macOS font directories, matched by filename.
///
/// `None` makes the endpoint 404 and the PWA keeps its fallback stack. Every failure logs once.
pub fn resolve(family: &str) -> Option<PathBuf> {
    resolve_with_override(family, "COPAD_WEB_BRIDGE_FONT")
}

/// Resolve `family`, honouring a path override from `override_var`. The variable is a parameter
/// because the terminal and symbol fonts must not share one: `COPAD_WEB_BRIDGE_FONT` pins an
/// exact file, and reading it for symbols too would serve the same glyph-incomplete file from
/// both endpoints.
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

/// Ask fontconfig, then verify it did not just return its substitute.
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

/// Best-effort filename match (macOS has no fc-match by default). Rank of a file for the
/// requested family, LOWER is better. `read_dir` order is arbitrary, and taking the first match
/// served `Jetendard-SemiBoldItalic` for every glyph, so the configured weight must win.
pub fn variant_rank(stem: &str, want: &str) -> u8 {
    let rest = norm(stem)
        .strip_prefix(want)
        .map(str::to_string)
        .unwrap_or_default();
    match rest.as_str() {
        // "Jetendard" itself, or a single-file family.
        "" => 0,
        "regular" => 1,
        // Any other named variant; italic is worse, since a whole terminal in italic is worse than
        // in the wrong weight.
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
            // Not `?`: one stem-less entry would abort the scan of every later directory.
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

    /// `read_dir` order once picked `Jetendard-SemiBoldItalic`, rendering every phone glyph
    /// semibold italic.
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
        // The endpoint is public; this gate stops an override from disclosing arbitrary readable
        // bytes.
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
        assert!(family_matches(
            "JetBrainsMono Nerd Font Mono",
            "JetBrainsMono Nerd Font Mono Regular",
            ""
        ));
        assert!(family_matches(
            "jetbrainsmono nerd font mono",
            "JetBrainsMonoNerdFontMono",
            ""
        ));
    }

    #[test]
    fn family_matches_rejects_the_fontconfig_substitute() {
        // fc-match never fails, it substitutes; serving DejaVu here would reproduce the tofu with
        // no diagnostic.
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
        // fontconfig answers "JetBrains Mono" when the Nerd variant is missing. It is a prefix of
        // what we asked for, so a two-way prefix check would accept it and serve a font with no
        // powerline glyphs.
        assert!(!family_matches(
            "JetBrainsMono Nerd Font Mono",
            "JetBrains Mono",
            "JetBrainsMono-Regular"
        ));
        assert!(!family_matches("Hack Nerd Font", "Hack", "Hack-Regular"));
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
        // Same length, likely same mtime: an mtime+size ETag would collide here.
        assert_ne!(fa.etag, fb.etag);
        assert_eq!(fa.content_type, "font/ttf");
        std::fs::remove_dir_all(&dir).ok();
    }
}
