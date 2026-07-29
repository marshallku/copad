//! `coctl background cache` — rebuild the wallpaper list file by scanning a
//! directory. Purely local (reads `config.toml`, writes the list file), so it
//! works over SSH and with no copad running, like `usage` and `agent status`.
//!
//! This exists for the curated-list workflow: point `[background] image` at a
//! directory and copad scans it live — no cache needed. Generate a list file
//! when you want the source to be an explicit set (hand-pruned, filtered by a
//! separate tool, or spanning several directories).

use std::path::PathBuf;

use copad_core::background::{DirSource, scan_dir, write_list};
use copad_core::config::{CopadConfig, expand_tilde};

pub fn run(
    path: Option<&str>,
    output: Option<&str>,
    recursive: bool,
    force: bool,
    json: bool,
) -> i32 {
    // A broken config shouldn't block an explicit `--path`/`--output` run, so
    // fall back to defaults instead of failing outright.
    let config = CopadConfig::load().unwrap_or_default();

    let Some(root) = path
        .map(expand_tilde)
        .or_else(|| config.background.source_path().filter(|p| p.is_dir()))
    else {
        eprintln!(
            "No directory to scan: pass --path <dir>, or set `[background] image` to a directory \
             in {}",
            CopadConfig::config_path().display()
        );
        return 1;
    };

    if !root.is_dir() {
        eprintln!("Not a directory: {}", root.display());
        return 1;
    }

    let out = output
        .map(expand_tilde)
        .or_else(|| config.background.list_path())
        .unwrap_or_else(default_list_path);

    // Overwriting silently would destroy a hand-curated list (the legacy
    // `terminal-wallpapers.txt` is a *filtered* subset of its source
    // directory, so a naive rescan would quietly re-add everything).
    let existing = existing_line_count(&out);
    if let Some(count) = existing
        && !force
    {
        eprintln!(
            "{} already exists ({count} entries). Re-run with --force to replace it.",
            out.display()
        );
        return 1;
    }

    let source = DirSource::new(
        root.clone(),
        recursive || config.background.recursive,
        &config.background.extensions,
    );
    let entries = match scan_dir(&source) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("Failed to scan {}: {e}", root.display());
            return 1;
        }
    };
    let written = match write_list(&out, &entries) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("Failed to write {}: {e}", out.display());
            return 1;
        }
    };

    // `write_list` drops non-UTF-8 paths, which the line-based list format
    // cannot represent — report them rather than letting the counts differ
    // with no explanation.
    let skipped = entries.len().saturating_sub(written);

    if json {
        println!(
            "{}",
            serde_json::json!({
                "source": root.to_string_lossy(),
                "output": out.to_string_lossy(),
                "recursive": source.recursive,
                "extensions": source.extensions,
                "scanned": entries.len(),
                "written": written,
                "skipped": skipped,
                "replaced": existing,
            })
        );
    } else {
        println!("scanned {} → wrote {written} entries", entries.len());
        println!("  source: {}", root.display());
        println!("  output: {}", out.display());
        if skipped > 0 {
            println!("  skipped {skipped} path(s) that are not valid UTF-8");
        }
    }
    0
}

/// Same default the Linux GUI uses when `[background] list` is unset.
fn default_list_path() -> PathBuf {
    expand_tilde("~/.cache/terminal-wallpapers.txt")
}

fn existing_line_count(path: &std::path::Path) -> Option<usize> {
    let contents = std::fs::read_to_string(path).ok()?;
    Some(contents.lines().filter(|l| !l.is_empty()).count())
}
