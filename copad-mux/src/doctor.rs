//! `comux doctor` — diagnose copad configuration problems.
//!
//! Runs locally: only a cheap connect probe reports whether a server is running.
//!
//! The headline check is the cross-platform float lint. Linux/serde coerces an integer literal
//! (`tint = 0`) into a float, but the macOS app parses via TOMLKit, whose strict decoder
//! REJECTS it (in older builds discarding the whole config). `doctor` names the exact line.

use std::path::Path;

use serde::Serialize;

use crate::config::MuxConfig;

/// `Note` is informational and never affects the exit code; `Warn` is tolerated but suspect;
/// `Error` is known to break a platform.
#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Level {
    Ok,
    Note,
    Warn,
    Error,
}

impl Level {
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

#[derive(Serialize)]
struct Section {
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    findings: Vec<Finding>,
}

/// Pane headroom below which `doctor` warns about the descriptor budget, early enough that the
/// user can still act on it.
const LOW_FD_PANES: usize = 8;

/// Mirrors the `f64` fields in `copad_core::config`. An explicit list, because the point is to
/// catch a value the macOS parser rejects, which the Rust schema would coerce and hide.
const FLOAT_FIELDS: &[(&str, &str)] = &[
    ("background", "tint"),
    ("background", "opacity"),
    ("window", "opacity"),
];

/// `comux doctor [--json]` entry point. Exit code is 0 only when nothing is `Error` or
/// `Warn`; `Note` findings never affect it, so the command works as a CI/pre-commit gate.
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

fn lint_config_value(value: &toml::Value) -> Vec<Finding> {
    let mut findings = Vec::new();

    for (table, key) in FLOAT_FIELDS {
        let Some(field) = value.get(table).and_then(|t| t.get(key)) else {
            continue;
        };
        if let Some(n) = field.as_integer() {
            // A Warn, not an Error: the current parser coerces int to float. It is still
            // worth flagging because a macOS build from before the parser was hardened
            // rejects an integer here and discards the whole config (non-Nerd font, tofu).
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

    // Surface the configured font so "my icons are broken" can be checked against it.
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

/// `mux.toml` — comux's own config. Read and syntax errors are checked here as `Error`s
/// because `MuxConfig::load_from` flattens everything into a severity-less `Vec<String>`; only
/// once the file parses do we defer to it for value-level warnings.
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

/// Report whether a server is running (connect probe on the control socket) plus, when one
/// is, its label/sweep counters: the only trace of a failed process sweep, whose visible
/// symptom (tab names and sidebar agents blinking out) is transient.
fn check_server() -> Section {
    let sock = crate::control::socket_path();
    let running = std::os::unix::net::UnixStream::connect(&sock).is_ok();
    if !running {
        let finding = Finding::new(Level::Note, "not running")
            .with_hint("start it with `comux` (attach) or `comux server start`");
        return section("server", Some(&sock), vec![finding]);
    }

    let mut findings = vec![Finding::new(Level::Ok, "running")];
    // A server that answers `list` but not `health` is an OLDER binary than this client;
    // name it, since zeroed counters would look like a healthy server.
    match crate::control::round_trip(&crate::control::Req::Health) {
        Ok(resp) => match resp.health {
            Some(h) => {
                findings.push(Finding::new(
                    Level::Note,
                    format!("{} panes, {} labeled", h.panes, h.labeled),
                ));
                findings.push(if h.label_sweeps_failed == 0 {
                    Finding::new(Level::Ok, "process sweeps: no failures")
                } else {
                    Finding::new(
                        Level::Warn,
                        format!("process sweeps: {} failed", h.label_sweeps_failed),
                    )
                    .with_hint(
                        "labels were carried forward instead of refreshed; \
                         tab names/agent list may lag. Please report this with the count.",
                    )
                });
                // Warn well before zero: at zero new tabs silently fail and the only cure is a
                // server restart, which costs the user their scrollback.
                if let (Some(soft), Some(open), Some(room)) =
                    (h.fd_soft, h.fd_open, h.panes_remaining())
                {
                    findings.push(if room >= LOW_FD_PANES {
                        Finding::new(
                            Level::Ok,
                            format!("fds: {open}/{soft} — room for {room} more panes"),
                        )
                    } else {
                        Finding::new(
                            Level::Warn,
                            format!("fds: {open}/{soft} — room for only {room} more panes"),
                        )
                        .with_hint(
                            "new tabs/sessions/splits will start failing to spawn a shell; \
                             `comux server restart` from a shell re-raises the limit",
                        )
                    });
                }
            }
            None => findings.push(
                Finding::new(Level::Note, "health counters unavailable").with_hint(
                    "the running server predates `comux health` — `comux server restart`",
                ),
            ),
        },
        Err(e) => findings.push(Finding::new(
            Level::Warn,
            format!("health probe failed: {e}"),
        )),
    }
    section("server", Some(&sock), findings)
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
        // `tint = 0` (integer) where a float is expected is a Warn, not an Error.
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
