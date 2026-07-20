//! Model → per-million-token rate table for `coctl usage`.
//!
//! ⚠️  PRICES ARE A MOVING TARGET. This table is a point-in-time snapshot;
//! verify against the providers' pricing pages and update here. Anthropic
//! rates are public and stable enough to treat as authoritative; OpenAI /
//! Codex rates are flagged `estimated` (the user may run codex under a flat
//! ChatGPT subscription where per-token cost is only notional), so the
//! renderer shows them with a `~` and a footnote. A future
//! `~/.config/copad/pricing.toml` override is a documented TODO, not built.
//!
//! Matching is by model-family PREFIX so version bumps (opus-4-8 → opus-4-9,
//! gpt-5.6-sol → gpt-5.x) keep pricing without a table edit.

use super::model::RawUsage;

/// USD per 1,000,000 tokens, split by billing category.
#[derive(Debug, Clone, Copy)]
pub struct Price {
    pub input: f64,
    pub cache_write: f64,
    pub cache_read: f64,
    pub output: f64,
    /// Best-effort estimate (rendered with `~` + footnote).
    pub estimated: bool,
}

/// Resolve a model string to a rate, or `None` when nothing matches (tokens
/// still count; cost renders `—`). Lowercased substring/prefix match so both
/// bare (`opus`) and fully-qualified (`claude-opus-4-8`) names hit.
pub fn price(model: &str) -> Option<Price> {
    let m = model.to_ascii_lowercase();

    // --- Anthropic (public rates; treated as authoritative) ---
    if m.contains("opus") {
        return Some(Price {
            input: 15.0,
            cache_write: 18.75,
            cache_read: 1.50,
            output: 75.0,
            estimated: false,
        });
    }
    if m.contains("sonnet") {
        return Some(Price {
            input: 3.0,
            cache_write: 3.75,
            cache_read: 0.30,
            output: 15.0,
            estimated: false,
        });
    }
    if m.contains("haiku") {
        return Some(Price {
            input: 0.80,
            cache_write: 1.00,
            cache_read: 0.08,
            output: 4.0,
            estimated: false,
        });
    }

    // --- OpenAI / Codex (flagged estimate) ---
    // gpt-5 family published API rates; codex model ids look like `gpt-5.6-sol`.
    if m.contains("gpt-5") || m.starts_with("gpt5") {
        return Some(Price {
            input: 1.25,
            cache_write: 1.25,
            cache_read: 0.125,
            output: 10.0,
            estimated: true,
        });
    }
    if m.starts_with("o3") || m.starts_with("o4") {
        return Some(Price {
            input: 2.0,
            cache_write: 2.0,
            cache_read: 0.50,
            output: 8.0,
            estimated: true,
        });
    }

    None
}

/// Dollar cost of `u` at rate `p`. Category rates are per-million tokens.
pub fn cost(u: &RawUsage, p: &Price) -> f64 {
    (u.input as f64 * p.input
        + u.cache_write as f64 * p.cache_write
        + u.cache_read as f64 * p.cache_read
        + u.output as f64 * p.output)
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_matches_families() {
        assert!(!price("claude-opus-4-8").unwrap().estimated);
        assert!(!price("claude-sonnet-5").unwrap().estimated);
        assert!(!price("claude-haiku-4-5").unwrap().estimated);
        assert!(price("gpt-5.6-sol").unwrap().estimated);
        assert!(price("mystery").is_none());
    }

    #[test]
    fn cost_splits_by_category() {
        let p = price("claude-opus-4-8").unwrap();
        // 1M output at $75/Mtok = $75.00
        let u = RawUsage {
            input: 0,
            cache_write: 0,
            cache_read: 0,
            output: 1_000_000,
        };
        assert!((cost(&u, &p) - 75.0).abs() < 1e-9);
        // cache_read at $1.50/Mtok
        let u2 = RawUsage {
            input: 0,
            cache_write: 0,
            cache_read: 1_000_000,
            output: 0,
        };
        assert!((cost(&u2, &p) - 1.50).abs() < 1e-9);
    }
}
