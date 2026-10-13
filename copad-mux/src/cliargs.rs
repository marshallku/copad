//! The argument grammar every flat `comux <verb>` shares.
//!
//! Before this existed, most verbs read their arguments ad hoc, so `--help` and a mistyped
//! flag were silently ignored — or, worse, read as a VALUE: `comux new-session --help` made
//! a session named `--help` and switched the user's view to it. Every verb now goes through
//! [`parse`], which fails BEFORE a request is built, so a bad invocation never reaches the
//! server:
//!
//! - `-h` / `--help` print the verb's usage to stdout and exit 0.
//! - An unknown `-flag` is a usage error (exit 2).
//! - Everything after `--` is positional, which is how a body or a name that starts with
//!   `-` is passed (`comux send <pane> -- --json`).
//! - More positionals than the verb takes is a usage error (exit 2).

/// One flag a verb accepts. `names[0]` is the canonical name [`Parsed`] reports it under.
pub struct Flag {
    pub names: &'static [&'static str],
    /// The flag consumes the next argument as its value.
    pub value: bool,
}

/// Shorthand for a flag table entry.
pub const fn flag(names: &'static [&'static str]) -> Flag {
    Flag {
        names,
        value: false,
    }
}

/// Shorthand for a value-taking flag table entry.
pub const fn opt(names: &'static [&'static str]) -> Flag {
    Flag { names, value: true }
}

/// How many positional arguments a verb takes.
#[derive(Clone, Copy)]
pub enum Positionals {
    AtMost(usize),
    /// Unbounded — the verb joins them (a name, a body).
    Any,
}

/// A verb's grammar.
pub struct Spec {
    pub usage: &'static str,
    pub flags: &'static [Flag],
    pub positionals: Positionals,
}

/// A verb that takes nothing at all.
pub const fn bare(usage: &'static str) -> Spec {
    Spec {
        usage,
        flags: &[],
        positionals: Positionals::AtMost(0),
    }
}

/// What a successful parse produced.
#[derive(Debug, Default, PartialEq)]
pub struct Parsed {
    /// `(canonical name, value)` in command-line order; a repeated flag appears repeatedly.
    pub flags: Vec<(&'static str, Option<String>)>,
    pub positionals: Vec<String>,
}

impl Parsed {
    pub fn has(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| *n == name)
    }

    /// The LAST value given for `name` — a later flag overrides an earlier one.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(n, _)| *n == name)
            .and_then(|(_, v)| v.as_deref())
    }
}

/// Why a parse stopped short of producing a [`Parsed`].
#[derive(Debug, PartialEq)]
pub enum Stop {
    /// `--help` was asked for: print the usage, exit 0.
    Help,
    /// The invocation is malformed: print this reason and the usage, exit 2.
    Usage(String),
}

/// Parse `args` (the verb's arguments, WITHOUT the verb itself) against `spec`.
pub fn parse(spec: &Spec, args: &[&str]) -> Result<Parsed, Stop> {
    let mut out = Parsed::default();
    let mut i = 0;
    let mut literal = false;
    while i < args.len() {
        let a = args[i];
        i += 1;
        if literal || !is_flag_shaped(a) {
            out.positionals.push(a.to_string());
            continue;
        }
        if a == "--" {
            literal = true;
            continue;
        }
        // A verb's own flag wins over help, so `split -h` keeps meaning "horizontal".
        let Some(f) = spec.flags.iter().find(|f| f.names.contains(&a)) else {
            if a == "-h" || a == "--help" {
                return Err(Stop::Help);
            }
            return Err(Stop::Usage(format!("unknown flag {a}")));
        };
        let value = if f.value {
            // A flag-shaped "value" (`--help`, `--`, `--bogus`) is a missing value, never data:
            // swallowing it would let `split --from --help` build a request instead of helping.
            let Some(v) = args.get(i).filter(|v| !is_flag_shaped(v)) else {
                // ...except that help still wins (unless the verb owns `-h` itself).
                let next = args.get(i).copied();
                let owns = |n: &str| spec.flags.iter().any(|f| f.names.contains(&n));
                if next == Some("--help") || (next == Some("-h") && !owns("-h")) {
                    return Err(Stop::Help);
                }
                return Err(Stop::Usage(format!("{a} needs a value")));
            };
            i += 1;
            Some((*v).to_string())
        } else {
            None
        };
        out.flags.push((f.names[0], value));
    }
    if let Positionals::AtMost(n) = spec.positionals
        && out.positionals.len() > n
    {
        return Err(Stop::Usage(match n {
            0 => format!("unexpected argument '{}'", out.positionals[0]),
            _ => format!("unexpected argument '{}'", out.positionals[n]),
        }));
    }
    Ok(out)
}

/// `-x`, `--xyz`, and `--` are flags; a bare `-` (stdin by convention) and the empty string
/// are not. Negative numbers ARE flag-shaped on purpose: no verb takes one, and accepting
/// `-1` as an index would silently address something.
fn is_flag_shaped(a: &str) -> bool {
    a.len() > 1 && a.starts_with('-')
}

/// Split the global `--json` switch off the argument list. Only arguments BEFORE a `--` are
/// considered, so `comux send <pane> -- --json` sends the literal text `--json`.
pub fn take_json(args: &[String]) -> (bool, Vec<String>) {
    let mut json = false;
    let mut out = Vec::with_capacity(args.len());
    let mut literal = false;
    for a in args {
        if !literal && a == "--json" {
            json = true;
            continue;
        }
        if a == "--" {
            literal = true;
        }
        out.push(a.clone());
    }
    (json, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: Spec = Spec {
        usage: "usage: x",
        flags: &[flag(&["--enter", "-e"]), opt(&["--file"])],
        positionals: Positionals::AtMost(2),
    };

    #[test]
    fn help_wins_wherever_it_appears() {
        assert_eq!(parse(&SPEC, &["a", "--help"]), Err(Stop::Help));
        assert_eq!(parse(&SPEC, &["-h"]), Err(Stop::Help));
    }

    #[test]
    fn help_after_the_separator_is_just_text() {
        let p = parse(&SPEC, &["--", "--help"]).unwrap();
        assert_eq!(p.positionals, vec!["--help"]);
    }

    #[test]
    fn unknown_flags_are_usage_errors() {
        assert!(matches!(parse(&SPEC, &["--bogus"]), Err(Stop::Usage(_))));
        assert!(matches!(parse(&SPEC, &["-1"]), Err(Stop::Usage(_))));
    }

    #[test]
    fn aliases_report_the_canonical_name_and_values_are_taken() {
        let p = parse(&SPEC, &["-e", "--file", "f.txt", "tok"]).unwrap();
        assert!(p.has("--enter"));
        assert_eq!(p.value("--file"), Some("f.txt"));
        assert_eq!(p.positionals, vec!["tok"]);
    }

    #[test]
    fn a_value_flag_at_the_end_is_an_error() {
        assert!(matches!(parse(&SPEC, &["--file"]), Err(Stop::Usage(_))));
    }

    #[test]
    fn a_value_flag_never_swallows_a_flag() {
        assert_eq!(parse(&SPEC, &["--file", "--help"]), Err(Stop::Help));
        for next in ["--", "--bogus", "-e"] {
            assert!(
                matches!(parse(&SPEC, &["--file", next]), Err(Stop::Usage(_))),
                "--file {next}"
            );
        }
    }

    #[test]
    fn too_many_positionals_is_an_error() {
        assert!(matches!(
            parse(&SPEC, &["a", "b", "c"]),
            Err(Stop::Usage(m)) if m.contains("'c'")
        ));
        assert!(matches!(parse(&bare("u"), &["x"]), Err(Stop::Usage(_))));
    }

    #[test]
    fn a_verb_that_owns_dash_h_keeps_it() {
        const OWNS_H: Spec = Spec {
            usage: "u",
            flags: &[flag(&["-h", "--horizontal"])],
            positionals: Positionals::AtMost(0),
        };
        assert!(parse(&OWNS_H, &["-h"]).unwrap().has("-h"));
        assert_eq!(parse(&OWNS_H, &["--help"]), Err(Stop::Help));
    }

    #[test]
    fn a_lone_dash_is_positional() {
        assert_eq!(parse(&SPEC, &["-"]).unwrap().positionals, vec!["-"]);
    }

    #[test]
    fn json_is_taken_only_before_the_separator() {
        let args: Vec<String> = ["send", "--json", "t", "--", "--json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (json, rest) = take_json(&args);
        assert!(json);
        assert_eq!(rest, vec!["send", "t", "--", "--json"]);
    }
}
