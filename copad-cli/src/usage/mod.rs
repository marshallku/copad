//! `coctl usage` — local Claude/Codex token + cost aggregation.
//!
//! Runs entirely from local transcript files (no daemon, no socket) so it works
//! over SSH and inside a tmux `status-right`. See submodules for the per-turn
//! dedup (claude) and cumulative-vs-delta (codex) subtleties.

mod claude;
mod codex;
mod model;
mod pricing;
mod render;

use crate::commands::UsageArgs;
use chrono::{DateTime, Duration, Local, Timelike};
use model::{Aggregate, Record, Tool, Warnings};
use std::path::Path;

/// Parse a `5h` / `30m` / `2d` / `90s` rolling-window duration.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let split = s
        .find(|c: char| c.is_ascii_alphabetic())
        .ok_or_else(|| format!("bad duration '{s}' (expected e.g. 5h)"))?;
    let (num, unit) = s.split_at(split);
    let n: i64 = num
        .parse()
        .map_err(|_| format!("bad duration number '{num}'"))?;
    if n < 0 {
        return Err(format!("duration must be non-negative: '{s}'"));
    }
    // `try_*` (not `Duration::hours` etc.) so a huge-but-valid i64 like
    // `9223372036854775807d` returns an error instead of panicking (codex C3).
    let dur = match unit {
        "s" => Duration::try_seconds(n),
        "m" => Duration::try_minutes(n),
        "h" => Duration::try_hours(n),
        "d" => Duration::try_days(n),
        _ => return Err(format!("unknown duration unit '{unit}' (use s/m/h/d)")),
    };
    dur.ok_or_else(|| format!("duration out of range: '{s}'"))
}

/// Resolve `(since, window_label)` from the flags. `--since` wins over
/// `--window`; `all` → no lower bound.
fn resolve_window(
    args: &UsageArgs,
    now: DateTime<Local>,
) -> Result<(Option<DateTime<Local>>, String), String> {
    if let Some(since) = &args.since {
        let dur = parse_duration(since)?;
        // checked_sub_signed so a giant duration can't overflow the datetime.
        let start = now
            .checked_sub_signed(dur)
            .ok_or_else(|| format!("--since window underflows the calendar: '{since}'"))?;
        return Ok((Some(start), format!("last {since}")));
    }
    match args.window.as_str() {
        "all" => Ok((None, "all".to_string())),
        "today" => {
            let start = now
                .with_hour(0)
                .and_then(|d| d.with_minute(0))
                .and_then(|d| d.with_second(0))
                .and_then(|d| d.with_nanosecond(0))
                .ok_or_else(|| "could not compute local midnight".to_string())?;
            Ok((Some(start), "today".to_string()))
        }
        other => Err(format!("--window must be today|all (got '{other}')")),
    }
}

/// Fold records into per-tool / per-model rollups with cost.
fn aggregate(records: Vec<Record>, warns: Warnings, label: String) -> Aggregate {
    let mut agg = Aggregate {
        window_label: label,
        warnings: warns,
        ..Default::default()
    };
    for rec in records {
        let (c, priced, estimated) = match pricing::price(&rec.model) {
            Some(p) => (pricing::cost(&rec.usage, &p), true, p.estimated),
            None => (0.0, false, false),
        };
        {
            let ta = match rec.tool {
                Tool::Claude => &mut agg.claude,
                Tool::Codex => &mut agg.codex,
            };
            let entry = ta.models.entry(rec.model.clone()).or_default();
            entry.usage.add(&rec.usage);
            entry.cost += c;
            entry.priced = priced;
            entry.estimated = estimated;
            ta.subtotal.add(&rec.usage);
            ta.cost += c;
        }
        agg.total.add(&rec.usage);
        agg.total_cost += c;
        if estimated {
            agg.any_estimated = true;
        }
        if !priced && rec.usage.total() > 0 {
            agg.has_unpriced = true;
        }
    }
    agg
}

/// Entry point for `main.rs`'s local dispatch. Returns a process exit code.
pub fn run(args: &UsageArgs, json: bool) -> i32 {
    let now = Local::now();
    let (since, label) = match resolve_window(args, now) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("coctl usage: {e}");
            return 2;
        }
    };

    let tool = args.tool.as_deref();
    if let Some(t) = tool
        && t != "claude"
        && t != "codex"
    {
        eprintln!("coctl usage: --tool must be claude|codex (got '{t}')");
        return 2;
    }
    let want_claude = tool.is_none_or(|t| t == "claude");
    let want_codex = tool.is_none_or(|t| t == "codex");

    let Ok(home) = std::env::var("HOME") else {
        eprintln!("coctl usage: HOME is not set");
        return 2;
    };

    let mut records = Vec::new();
    let mut warns = Warnings::default();
    if want_claude {
        let (r, w) = claude::scan(&Path::new(&home).join(".claude/projects"), since);
        records.extend(r);
        warns.merge(&w);
    }
    if want_codex {
        let (r, w) = codex::scan(&Path::new(&home).join(".codex/sessions"), since);
        records.extend(r);
        warns.merge(&w);
    }

    let agg = aggregate(records, warns, label);

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&render::to_json(&agg)).unwrap()
        );
    } else if args.oneline {
        println!("{}", render::oneline(&agg));
        if !agg.warnings.is_empty() {
            eprintln!(
                "coctl usage: {} unreadable file(s), {} skipped line(s)",
                agg.warnings.unreadable_files, agg.warnings.skipped_lines
            );
        }
    } else {
        print!("{}", render::human(&agg));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::seconds(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::minutes(30));
        assert_eq!(parse_duration("5h").unwrap(), Duration::hours(5));
        assert_eq!(parse_duration("2d").unwrap(), Duration::days(2));
        assert!(parse_duration("5").is_err()); // no unit
        assert!(parse_duration("5x").is_err()); // bad unit
        assert!(parse_duration("-1h").is_err()); // negative
        assert!(parse_duration("abc").is_err());
        // huge-but-valid i64 must error, not panic (codex C3)
        assert!(parse_duration("9223372036854775807d").is_err());
    }

    #[test]
    fn resolve_window_today_is_local_midnight() {
        let now = Local::now();
        let args = UsageArgs {
            window: "today".into(),
            since: None,
            tool: None,
            oneline: false,
        };
        let (since, label) = resolve_window(&args, now).unwrap();
        let start = since.unwrap();
        assert_eq!(label, "today");
        assert_eq!(start.hour(), 0);
        assert_eq!(start.minute(), 0);
        assert!(start <= now);
    }

    #[test]
    fn resolve_window_since_overrides() {
        let now = Local::now();
        let args = UsageArgs {
            window: "today".into(),
            since: Some("5h".into()),
            tool: None,
            oneline: false,
        };
        let (since, label) = resolve_window(&args, now).unwrap();
        assert_eq!(label, "last 5h");
        assert_eq!(since.unwrap(), now - Duration::hours(5));
    }

    #[test]
    fn resolve_window_all_has_no_bound() {
        let args = UsageArgs {
            window: "all".into(),
            since: None,
            tool: None,
            oneline: false,
        };
        let (since, label) = resolve_window(&args, Local::now()).unwrap();
        assert!(since.is_none());
        assert_eq!(label, "all");
    }

    #[test]
    fn aggregate_splits_by_tool_and_prices() {
        use model::RawUsage;
        let records = vec![
            Record {
                tool: Tool::Claude,
                model: "claude-opus-4-8".into(),
                usage: RawUsage {
                    input: 1_000_000,
                    cache_write: 0,
                    cache_read: 0,
                    output: 0,
                },
            },
            Record {
                tool: Tool::Codex,
                model: "gpt-5.6-sol".into(),
                usage: RawUsage {
                    input: 1_000_000,
                    cache_write: 0,
                    cache_read: 0,
                    output: 0,
                },
            },
        ];
        let agg = aggregate(records, Warnings::default(), "today".into());
        // opus input rate $15/Mtok → $15.00
        assert!((agg.claude.cost - 15.0).abs() < 1e-9);
        // gpt-5 input rate $1.25/Mtok → $1.25, and flagged estimated
        assert!((agg.codex.cost - 1.25).abs() < 1e-9);
        assert!(agg.any_estimated);
        assert_eq!(agg.total.total(), 2_000_000);
    }

    #[test]
    fn aggregate_unknown_model_unpriced_but_counts_tokens() {
        use model::RawUsage;
        let records = vec![Record {
            tool: Tool::Codex,
            model: "mystery-model".into(),
            usage: RawUsage {
                input: 500,
                cache_write: 0,
                cache_read: 0,
                output: 100,
            },
        }];
        let agg = aggregate(records, Warnings::default(), "all".into());
        let m = agg.codex.models.get("mystery-model").unwrap();
        assert!(!m.priced);
        assert_eq!(m.usage.total(), 600);
        assert_eq!(agg.total_cost, 0.0);
        assert!(agg.has_unpriced); // total must flag "+" so $0.00 ≠ "free" (C2)
    }
}
