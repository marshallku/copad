//! Shared usage aggregation types for `coctl usage`.
//!
//! `RawUsage` uses a category split (uncached-input / cache-write /
//! cache-read / output) that maps cleanly onto BOTH providers' billing so
//! `pricing::cost` is provider-agnostic:
//!   * Claude `message.usage`: input_tokens is already the *uncached* input;
//!     cache_creation → cache_write, cache_read_input → cache_read.
//!   * Codex `last_token_usage`: input_tokens is the *full* input INCLUDING
//!     the cached portion, so the parser subtracts `cached_input_tokens`
//!     into `cache_read` (codex has no separate cache-write meter).
//!
//! `output` already INCLUDES reasoning tokens for codex — reasoning is a
//! subset of `output_tokens`, not an addend (verified: input+output == total).
//! Adding it again would double-count both tokens and cost.

use std::collections::BTreeMap;

/// Which provider a record came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Claude,
    Codex,
}

impl Tool {
    pub fn as_str(self) -> &'static str {
        match self {
            Tool::Claude => "claude",
            Tool::Codex => "codex",
        }
    }
}

/// Token counts for one already-deduplicated turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawUsage {
    /// Uncached input tokens (billed at the input rate).
    pub input: u64,
    /// Cache-write / cache-creation tokens (billed at the write rate).
    pub cache_write: u64,
    /// Cache-read / cached-input tokens (billed at the read rate).
    pub cache_read: u64,
    /// Output tokens (already includes reasoning tokens; do not add them).
    pub output: u64,
}

impl RawUsage {
    /// Saturating so a malformed transcript with an absurd `u64` field can't
    /// panic a debug build or wrap a release build into a wrong total (codex C4).
    /// Real usage is nowhere near `u64::MAX`, so saturation only bites garbage.
    pub fn add(&mut self, o: &RawUsage) {
        self.input = self.input.saturating_add(o.input);
        self.cache_write = self.cache_write.saturating_add(o.cache_write);
        self.cache_read = self.cache_read.saturating_add(o.cache_read);
        self.output = self.output.saturating_add(o.output);
    }

    /// All billable tokens, for the human "tok" column.
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.cache_write)
            .saturating_add(self.cache_read)
            .saturating_add(self.output)
    }
}

/// One parsed, deduplicated record before aggregation. Window-filtering already
/// happened in the scanner, so no timestamp is carried here.
pub struct Record {
    pub tool: Tool,
    pub model: String,
    pub usage: RawUsage,
}

/// Non-fatal problems encountered while scanning — surfaced on stderr and in
/// `--json` so silent format drift or permission errors don't masquerade as
/// legitimate zero usage (codex review I2).
#[derive(Debug, Clone, Default)]
pub struct Warnings {
    pub unreadable_files: u64,
    pub skipped_lines: u64,
}

impl Warnings {
    pub fn merge(&mut self, o: &Warnings) {
        self.unreadable_files += o.unreadable_files;
        self.skipped_lines += o.skipped_lines;
    }

    pub fn is_empty(&self) -> bool {
        self.unreadable_files == 0 && self.skipped_lines == 0
    }
}

/// Per-model rollup within a tool.
#[derive(Debug, Clone, Default)]
pub struct ModelAgg {
    pub usage: RawUsage,
    pub cost: f64,
    /// True when priced from a rate table entry flagged "best-effort estimate"
    /// (currently all non-Anthropic models) — rendered with a `~` and footnote.
    pub estimated: bool,
    /// True when NO rate matched — tokens are real, cost is unknown (shown `—`).
    pub priced: bool,
}

/// Per-tool rollup: models sorted for stable output, plus a subtotal.
#[derive(Debug, Clone, Default)]
pub struct ToolAgg {
    pub models: BTreeMap<String, ModelAgg>,
    pub subtotal: RawUsage,
    pub cost: f64,
}

/// The full aggregate handed to the renderers.
#[derive(Debug, Clone, Default)]
pub struct Aggregate {
    pub window_label: String,
    pub claude: ToolAgg,
    pub codex: ToolAgg,
    pub total: RawUsage,
    pub total_cost: f64,
    /// Any tool contributed an estimated price → render the footnote.
    pub any_estimated: bool,
    /// A model with real tokens had NO rate → its cost is excluded from totals,
    /// so cost figures render with a trailing `+` ("at least this") instead of
    /// implying the unpriced tokens were free (codex C2).
    pub has_unpriced: bool,
    pub warnings: Warnings,
}

impl Aggregate {
    pub fn tool(&self, t: Tool) -> &ToolAgg {
        match t {
            Tool::Claude => &self.claude,
            Tool::Codex => &self.codex,
        }
    }
}
