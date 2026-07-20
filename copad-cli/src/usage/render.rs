//! Renderers for the usage aggregate: human table, tmux one-liner, JSON.

use super::model::{Aggregate, Tool, ToolAgg};
use serde_json::json;

/// `5_100_000 → "5.1M"`, `12_300 → "12.3K"`, `812 → "812"`.
pub fn humanize(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// `$6.10`, or `~$0.90` for estimated rates, or `—` when unpriced.
fn money(cost: f64, priced: bool, estimated: bool) -> String {
    if !priced {
        return "—".to_string();
    }
    let tilde = if estimated { "~" } else { "" };
    format!("{tilde}${cost:.2}")
}

/// A subtotal/total cost cell. `has_unpriced` = some tokens in scope had no
/// rate. When NOTHING was priced (`cost == 0` with unpriced tokens) render `—`
/// rather than `$0.00`, so "unknown" never reads as "free" (codex C2). When
/// some priced + some unpriced, append `+` ("at least this").
fn cost_cell(cost: f64, estimated: bool, has_unpriced: bool) -> String {
    if has_unpriced && cost == 0.0 {
        return "—".to_string();
    }
    let tilde = if estimated { "~" } else { "" };
    let plus = if has_unpriced { "+" } else { "" };
    format!("{tilde}${cost:.2}{plus}")
}

fn short_model(m: &str) -> String {
    // Trim the vendor prefix for a tidier column: `claude-opus-4-8` → `opus-4-8`.
    m.strip_prefix("claude-").unwrap_or(m).to_string()
}

pub fn human(agg: &Aggregate) -> String {
    let mut out = String::new();
    let mut any_row = false;
    for tool in [Tool::Claude, Tool::Codex] {
        let ta = agg.tool(tool);
        if ta.models.is_empty() {
            continue;
        }
        any_row = true;
        let mut first = true;
        for (model, ma) in &ta.models {
            let label = if first { tool.as_str() } else { "" };
            first = false;
            out.push_str(&format!(
                "{label:<7} {:<12} {:>8} tok  {:>8}\n",
                short_model(model),
                humanize(ma.usage.total()),
                money(ma.cost, ma.priced, ma.estimated),
            ));
        }
    }
    if !any_row {
        return format!("{}: no usage found\n", agg.window_label);
    }
    out.push_str(&"─".repeat(40));
    out.push('\n');
    let total_cost = cost_cell(agg.total_cost, agg.any_estimated, agg.has_unpriced);
    out.push_str(&format!(
        "{:<7} {:<12} {:>8} tok  {:>8}\n",
        agg.window_label,
        "",
        humanize(agg.total.total()),
        total_cost,
    ));
    if agg.any_estimated {
        out.push_str("~ = estimated rate (verify pricing)\n");
    }
    if agg.has_unpriced {
        out.push_str("+ = plus unpriced model(s) — tokens counted, cost unknown\n");
    }
    if !agg.warnings.is_empty() {
        out.push_str(&format!(
            "note: {} unreadable file(s), {} skipped line(s)\n",
            agg.warnings.unreadable_files, agg.warnings.skipped_lines
        ));
    }
    out
}

/// One line for a tmux `status-right`: `claude 5.1M $6.41 · codex 1.1M $0.90`.
pub fn oneline(agg: &Aggregate) -> String {
    let mut parts = Vec::new();
    for tool in [Tool::Claude, Tool::Codex] {
        let ta = agg.tool(tool);
        if ta.subtotal.total() == 0 {
            continue;
        }
        parts.push(format!(
            "{} {} {}",
            tool.as_str(),
            humanize(ta.subtotal.total()),
            tool_cost(ta),
        ));
    }
    if parts.is_empty() {
        return "no usage".to_string();
    }
    parts.join(" · ")
}

/// A tool's subtotal cost for the one-liner: `~`/`+`-flagged, or `—` when the
/// tool's tokens were entirely unpriced.
fn tool_cost(ta: &ToolAgg) -> String {
    let estimated = ta.models.values().any(|m| m.estimated && m.priced);
    let has_unpriced = ta.models.values().any(|m| !m.priced && m.usage.total() > 0);
    cost_cell(ta.cost, estimated, has_unpriced)
}

pub fn to_json(agg: &Aggregate) -> serde_json::Value {
    let tool_json = |ta: &ToolAgg| {
        let models: Vec<_> = ta
            .models
            .iter()
            .map(|(model, ma)| {
                json!({
                    "model": model,
                    "tokens": {
                        "input": ma.usage.input,
                        "cache_write": ma.usage.cache_write,
                        "cache_read": ma.usage.cache_read,
                        "output": ma.usage.output,
                        "total": ma.usage.total(),
                    },
                    "cost": ma.cost,
                    "priced": ma.priced,
                    "estimated": ma.estimated,
                })
            })
            .collect();
        json!({
            "models": models,
            "subtotal_tokens": ta.subtotal.total(),
            "cost": ta.cost,
        })
    };
    json!({
        "window": agg.window_label,
        "claude": tool_json(&agg.claude),
        "codex": tool_json(&agg.codex),
        "total_tokens": agg.total.total(),
        "total_cost": agg.total_cost,
        "any_estimated": agg.any_estimated,
        "has_unpriced": agg.has_unpriced,
        "warnings": {
            "unreadable_files": agg.warnings.unreadable_files,
            "skipped_lines": agg.warnings.skipped_lines,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_scales() {
        assert_eq!(humanize(812), "812");
        assert_eq!(humanize(12_300), "12.3K");
        assert_eq!(humanize(5_100_000), "5.1M");
    }

    #[test]
    fn money_variants() {
        assert_eq!(money(6.1, true, false), "$6.10");
        assert_eq!(money(0.9, true, true), "~$0.90");
        assert_eq!(money(0.0, false, false), "—");
    }

    #[test]
    fn cost_cell_semantics() {
        // fully priced
        assert_eq!(cost_cell(6.1, false, false), "$6.10");
        // priced + estimated
        assert_eq!(cost_cell(0.9, true, false), "~$0.90");
        // some priced, some unpriced → "at least this"
        assert_eq!(cost_cell(6.1, false, true), "$6.10+");
        // NOTHING priced → em dash, never "$0.00" (codex C2)
        assert_eq!(cost_cell(0.0, false, true), "—");
    }

    #[test]
    fn short_model_trims_vendor_prefix() {
        assert_eq!(short_model("claude-opus-4-8"), "opus-4-8");
        assert_eq!(short_model("gpt-5.6-sol"), "gpt-5.6-sol");
    }

    #[test]
    fn oneline_empty_aggregate() {
        let agg = Aggregate::default();
        assert_eq!(oneline(&agg), "no usage");
    }
}
