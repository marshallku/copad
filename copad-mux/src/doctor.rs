//! `comux doctor` — diagnose copad configuration problems.
//!
//! Surfaces WHERE and WHAT is wrong across the config surface so a broken setup
//! is a one-command find instead of a mystery. Runs entirely locally — it never
//! contacts the server for the config checks (only a cheap connect probe reports
//! whether one is running), so it works on a cold machine.
//!
//! The headline check is the cross-platform float lint: copad's config schema
//! has a handful of float fields (`[background] tint`/`opacity`, `[window]
//! opacity`). Linux/serde silently coerces an integer literal (`tint = 0`) into
//! a float, but the macOS app parses via TOMLKit, whose strict decoder REJECTS
//! the integer and — before the parser was hardened — discarded the ENTIRE
//! config, falling back to a non-Nerd default font so every glyph icon rendered
//! as tofu. A config that worked on Linux broke only on macOS, with the error
//! buried in a GUI-app stderr nobody reads. `doctor` names the exact line.

use std::path::Path;

use serde::Serialize;

use crate::config::MuxConfig;

/// Severity of a single diagnostic. `Note` is purely informational (never
/// affects the exit code); `Warn` flags a tolerated-but-suspect condition;
/// `Error` is something known to break a platform.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Level {
    Ok,
    Note,
    Warn,
    Error,
}

impl Level {
    /// The status glyph shown in human output. ASCII-safe fallbacks are not
    /// needed — comux already assumes a UTF-8 terminal everywhere else.
    fn glyph(self) -> &'static str {
        match self {
            Level::Ok => "✓",
            Level::Note => "·",
            Level::Warn => "⚠",
            Level::Error => "✗",
        }
    }
}

#[derive(Serialize)]
struct Finding {
    level: Level,
    message: String,
    /// A concrete fix, printed indented under the message when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

impl Finding {
    fn new(level: Level, message: impl Into<String>) -> Self {
        Finding {
            level,
            message: message.into(),
            hint: None,
        }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// One report section (a config file or a subsystem) with its findings.
#[derive(Serialize)]
struct Section {
    title: String,
    /// The file/socket the section is about, shown next to the title. `None` for
    /// sections that aren't tied to a single path.
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    findings: Vec<Finding>,
}

/// The float fields in copad's `config.toml` schema. Mirrors the `f64` fields in
/// `copad_core::config` — kept as a small explicit list rather than reflecting
/// the schema because the whole point is to catch a value the *macOS* parser
/// rejects, which the Rust schema itself would happily coerce and hide.
const FLOAT_FIELDS: &[(&str, &str)] = &[
    ("background", "tint"),
    ("background", "opacity"),
    ("window", "opacity"),
];

/// `comux doctor [--json]` entry point. Exit code follows the `brew doctor`
/// convention: 0 only when the config surface is completely clean, 1 when there
/// is anything worth a look — an `Error` (broken on every platform right now) OR
/// a `Warn` (e.g. an integer for a float field: tolerated by the current parser
/// but a hazard on older/released builds and other platforms). `Note`-level
/// findings are purely informational and never affect the exit code, so the
/// command stays usable as a CI/pre-commit gate.
pub fn run(args: &[String]) -> i32 {
    let json_out = args.iter().any(|a| a == "--json");

    let config_path = MuxConfig::config_path().with_file_name("config.toml");
    let mux_path = MuxConfig::config_path();

    let sections = vec![
        check_config_toml(&config_path),
        check_mux_toml(&mux_path),
        check_server(),
        check_state(),
    ];

    let (errors, warnings) = tally(&sections);

    if json_out {
        print_json(&sections, errors, warnings);
    } else {
        print_human(&sections, errors, warnings);
    }

    i32::from(errors > 0 || warnings > 0)
}

/// Count `Error`/`Warn` findings across every section.
fn tally(sections: &[Section]) -> (usize, usize) {
    let mut errors = 0;
    let mut warnings = 0;
    for s in sections {
        for f in &s.findings {
            match f.level {
                Level::Error => errors += 1,
                Level::Warn => warnings += 1,
                _ => {}
            }
        }
    }
    (errors, warnings)
}

/// `config.toml` — copad's shared GUI config. Checks that it parses and lints
/// the cross-platform float footgun; echoes the configured font for context.
fn check_config_toml(path: &Path) -> Section {
    let mut findings = Vec::new();

    if !path.exists() {
        findings.push(
            Finding::new(Level::Note, "not present — copad uses built-in defaults")
                .with_hint("this is normal on a machine that has never customized copad"),
        );
        return section("config.toml", Some(path), findings);
    }

    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            findings.push(Finding::new(Level::Error, format!("cannot read: {e}")));
            return section("config.toml", Some(path), findings);
        }
    };

    let value = match contents.parse::<toml::Value>() {
        Ok(v) => v,
        Err(e) => {
            findings.push(
                Finding::new(Level::Error, format!("TOML syntax error: {e}"))
                    .with_hint("fix the syntax; every platform fails to parse this file"),
            );
            return section("config.toml", Some(path), findings);
        }
    };

    findings.push(Finding::new(Level::Ok, "parses"));
    findings.extend(lint_config_value(&value));
    section("config.toml", Some(path), findings)
}

/// Pure lint over a parsed `config.toml` — split out so it's unit-testable
/// without touching the filesystem.
fn lint_config_value(value: &toml::Value) -> Vec<Finding> {
    let mut findings = Vec::new();

    for (table, key) in FLOAT_FIELDS {
        let Some(field) = value.get(table).and_then(|t| t.get(key)) else {
            continue;
        };
        if let Some(n) = field.as_integer() {
            // A Warn, not an Error: the current macOS parser coerces int→float,
            // so this no longer breaks a copad built from this source, and failing
            // the exit code on a config the current binary accepts would be wrong.
            // It stays worth flagging because a macOS build from BEFORE the parser
            // hardening (any released binary until the next release) still rejects
            // an integer here and discards the whole config → non-Nerd fallback
            // font → tofu glyphs. Writing the float is the portable, safe form.
            findings.push(
                Finding::new(
                    Level::Warn,
                    format!("[{table}] {key} = {n} is an integer; this field is a float"),
                )
                .with_hint(format!(
                    "write `{key} = {n}.0`. The current macOS parser coerces it, but a macOS \
                     copad built before the parser hardening rejects an integer here and \
                     discards the ENTIRE config (→ non-Nerd fallback font, tofu glyphs). The \
                     float form is portable across versions and platforms."
                )),
            );
        }
    }

    // Informational: surface the configured font so a "my icons are broken"
    // report can be eyeballed against what's actually installed.
    if let Some(font) = value
        .get("terminal")
        .and_then(|t| t.get("font_family"))
        .and_then(|f| f.as_str())
    {
        findings.push(Finding::new(
            Level::Note,
            format!("[terminal] font_family = \"{font}\""),
        ));
    }

    findings
}

/// `mux.toml` — comux's own config. Read + syntax errors are checked here first
/// (as `Error`s, symmetric with `config.toml`) because `MuxConfig::load_from`
/// flattens *everything* — an unreadable file, a TOML syntax error, and a merely
/// suspect value — into one severity-less `Vec<String>`. Only once the file
/// genuinely parses do we defer to `load_from` for the value-level warnings (bad
/// bindings, out-of-range numbers, collisions) it already knows how to collect,
/// keeping the definition of a "bad value" in one place.
fn check_mux_toml(path: &Path) -> Section {
    let mut findings = Vec::new();

    if !path.exists() {
        findings.push(Finding::new(
            Level::Note,
            "not present — comux uses built-in defaults",
        ));
        return section("mux.toml", Some(path), findings);
    }

    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            findings.push(Finding::new(Level::Error, format!("cannot read: {e}")));
            return section("mux.toml", Some(path), findings);
        }
    };
    if let Err(e) = contents.parse::<toml::Value>() {
        findings.push(
            Finding::new(Level::Error, format!("TOML syntax error: {e}"))
                .with_hint("comux falls back to built-in defaults until this parses"),
        );
        return section("mux.toml", Some(path), findings);
    }

    // Parses as TOML; surface only the value-level warnings from load_from.
    let (_cfg, warnings) = MuxConfig::load_from(path);
    if warnings.is_empty() {
        findings.push(Finding::new(Level::Ok, "parses (no warnings)"));
    } else {
        for w in warnings {
            findings.push(Finding::new(Level::Warn, w));
        }
    }
    section("mux.toml", Some(path), findings)
}

/// Report whether a server is running, via a cheap connect probe on the control
/// socket (the same signal `comux server status` uses).
fn check_server() -> Section {
    let sock = crate::control::socket_path();
    let running = std::os::unix::net::UnixStream::connect(&sock).is_ok();
    let finding = if running {
        Finding::new(Level::Ok, "running")
    } else {
        Finding::new(Level::Note, "not running")
            .with_hint("start it with `comux` (attach) or `comux server start`")
    };
    section("server", Some(&sock), vec![finding])
}

/// Report the persisted-session file. A missing file is the normal "no saved
/// layout yet" `Note`; a file that exists but can't be stat'd (e.g. a permission
/// error) is a real `Warn` rather than being misreported as absent.
fn check_state() -> Section {
    let path = crate::persist::state_path();
    let finding = match std::fs::metadata(&path) {
        Ok(m) => Finding::new(
            Level::Note,
            format!("saved session layout present ({})", human_size(m.len())),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Finding::new(Level::Note, "no saved session layout yet")
        }
        Err(e) => Finding::new(Level::Warn, format!("cannot stat saved-session file: {e}")),
    };
    section("state", Some(&path), vec![finding])
}

fn section(title: &str, path: Option<&Path>, findings: Vec<Finding>) -> Section {
    Section {
        title: title.to_string(),
        path: path.map(|p| p.display().to_string()),
        findings,
    }
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn print_human(sections: &[Section], errors: usize, warnings: usize) {
    println!("comux doctor\n");
    for s in sections {
        match &s.path {
            Some(p) => println!("  {}  {p}", s.title),
            None => println!("  {}", s.title),
        }
        for f in &s.findings {
            println!("    {} {}", f.level.glyph(), f.message);
            if let Some(hint) = &f.hint {
                // Indent the hint under its finding; wrap-agnostic (the terminal
                // soft-wraps) so we keep it one logical line.
                println!("        → {hint}");
            }
        }
        println!();
    }
    println!("{}", summary_line(errors, warnings));
}

fn summary_line(errors: usize, warnings: usize) -> String {
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    format!(
        "{}, {}",
        plural(errors, "error"),
        plural(warnings, "warning")
    )
}

#[derive(Serialize)]
struct Report<'a> {
    errors: usize,
    warnings: usize,
    sections: &'a [Section],
}

fn print_json(sections: &[Section], errors: usize, warnings: usize) {
    let report = Report {
        errors,
        warnings,
        sections,
    };
    match serde_json::to_string_pretty(&report) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("comux doctor: could not serialize report: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn levels(findings: &[Finding], level: Level) -> usize {
        findings.iter().filter(|f| f.level == level).count()
    }

    #[test]
    fn integer_float_field_is_a_warning() {
        // The exact bug: `tint = 0` (integer) where a float is expected. It's a
        // Warn, not an Error — the current parser coerces it, but a pre-hardening
        // build breaks, so it's flagged with the fix without failing the exit code.
        let v: toml::Value = "[background]\ntint = 0\nopacity = 0.5\n".parse().unwrap();
        let findings = lint_config_value(&v);
        assert_eq!(levels(&findings, Level::Warn), 1);
        assert_eq!(levels(&findings, Level::Error), 0);
        let warn = findings.iter().find(|f| f.level == Level::Warn).unwrap();
        assert!(warn.message.contains("tint"));
        assert!(warn.hint.as_deref().unwrap().contains("0.0"));
    }

    #[test]
    fn float_literal_is_clean() {
        let v: toml::Value =
            "[background]\ntint = 0.0\nopacity = 0.5\n\n[window]\nopacity = 0.82\n"
                .parse()
                .unwrap();
        let findings = lint_config_value(&v);
        assert_eq!(levels(&findings, Level::Warn), 0);
        assert_eq!(levels(&findings, Level::Error), 0);
    }

    #[test]
    fn every_float_field_is_linted() {
        // An integer in each known float field yields one warning apiece.
        let v: toml::Value = "[background]\ntint = 1\nopacity = 1\n\n[window]\nopacity = 1\n"
            .parse()
            .unwrap();
        let findings = lint_config_value(&v);
        assert_eq!(levels(&findings, Level::Warn), FLOAT_FIELDS.len());
    }

    #[test]
    fn font_family_is_surfaced_as_a_note() {
        let v: toml::Value = "[terminal]\nfont_family = \"Jetendard\"\n".parse().unwrap();
        let findings = lint_config_value(&v);
        let note = findings
            .iter()
            .find(|f| f.level == Level::Note)
            .expect("font note");
        assert!(note.message.contains("Jetendard"));
    }

    #[test]
    fn missing_float_field_is_not_flagged() {
        let v: toml::Value = "[terminal]\nshell = \"/bin/zsh\"\n".parse().unwrap();
        let findings = lint_config_value(&v);
        assert_eq!(levels(&findings, Level::Error), 0);
    }

    #[test]
    fn summary_pluralizes() {
        assert_eq!(summary_line(1, 0), "1 error, 0 warnings");
        assert_eq!(summary_line(0, 2), "0 errors, 2 warnings");
    }
}
