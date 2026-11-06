//! Shared usage aggregation types for `coctl usage`.
//!
//! `RawUsage` splits tokens into uncached input / cache-write / cache-read / output, which fits
//! both providers' billing so `pricing::cost` is provider-agnostic. Claude's `input_tokens` is
//! already uncached; Codex's includes the cached portion, so its parser subtracts
//! `cached_input_tokens` into `cache_read`. Codex `output` already includes reasoning tokens;
//! adding them again would double-count tokens and cost.

use std::collections::BTreeMap;

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawUsage {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    /// Output tokens (already includes reasoning tokens; do not add them).
    pub output: u64,
}

impl RawUsage {
    /// Saturating so a malformed transcript with an absurd `u64` field cannot panic a debug build
    /// or wrap a release build into a wrong total.
    pub fn add(&mut self, o: &RawUsage) {
        self.input = self.input.saturating_add(o.input);
        self.cache_write = self.cache_write.saturating_add(o.cache_write);
        self.cache_read = self.cache_read.saturating_add(o.cache_read);
        self.output = self.output.saturating_add(o.output);
    }

    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.cache_write)
            .saturating_add(self.cache_read)
            .saturating_add(self.output)
    }
}

/// One parsed, deduplicated record before aggregation; the scanner already applied the window.
pub struct Record {
    pub tool: Tool,
    pub model: String,
    pub usage: RawUsage,
}

/// Non-fatal scan problems, surfaced on stderr and in `--json` so format drift or permission
/// errors don't masquerade as zero usage.
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

#[derive(Debug, Clone, Default)]
pub struct ModelAgg {
    pub usage: RawUsage,
    pub cost: f64,
    /// Priced from a rate entry flagged "best-effort estimate" (currently all non-Anthropic
    /// models): rendered with a `~` and a footnote.
    pub estimated: bool,
    /// False when NO rate matched: tokens are real, cost is unknown (shown `—`).
    pub priced: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ToolAgg {
    pub models: BTreeMap<String, ModelAgg>,
    pub subtotal: RawUsage,
    pub cost: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Aggregate {
    pub window_label: String,
    pub claude: ToolAgg,
    pub codex: ToolAgg,
    pub total: RawUsage,
    pub total_cost: f64,
    pub any_estimated: bool,
    /// A model with real tokens had NO rate: its cost is left out of the totals, so cost figures
    /// render with a trailing `+` ("at least this") instead of implying those tokens were free.
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
