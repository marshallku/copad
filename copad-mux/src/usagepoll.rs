//! Background poller for the status-bar usage/limits readout.
//!
//! Resolves the subscription rate-limit windows in-process via the shared
//! `copad_usage::limits` crate — Claude 5h + weekly (a live OAuth call) and
//! Codex weekly (newest rollout snapshot) — into a [`UsageSnapshot`], and shares
//! it with the render loop. Numbers (not a pre-formatted string) so the status
//! bar can render either text or a progress bar per config + width. Calling it
//! does network + disk I/O, so it runs on a dedicated thread, never the render
//! loop. Reading the limits in-process (not by shelling out to `coctl`) is what
//! lets comux install standalone. `COPAD_MUX_USAGE=0` disables it.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

/// How often to re-poll. The 5h / weekly windows move slowly and each poll is a
/// process spawn + network round-trip, so a minute is plenty.
const POLL: Duration = Duration::from_secs(60);

const BAR_FILLED: char = '━';
const BAR_EMPTY: char = '╌';

/// Which rate-limit windows the status bar is allowed to show (config
/// `usage_windows`). A window is rendered only if it is BOTH available in the
/// snapshot AND enabled here — so a user can hide, e.g., Claude's 5h window and
/// keep only the weekly ones. Default = all on (the historical readout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageWindows {
    pub claude_5h: bool,
    pub claude_wk: bool,
    pub codex_wk: bool,
}

impl Default for UsageWindows {
    fn default() -> Self {
        Self::all()
    }
}

impl UsageWindows {
    /// Every window enabled (the zero-config readout: Claude 5h + weekly, Codex weekly).
    pub fn all() -> Self {
        UsageWindows {
            claude_5h: true,
            claude_wk: true,
            codex_wk: true,
        }
    }

    /// No window enabled (starting point when a config list is given).
    pub fn none() -> Self {
        UsageWindows {
            claude_5h: false,
            claude_wk: false,
            codex_wk: false,
        }
    }
}

/// Parsed rate-limit percentages. `None` window = unavailable (omitted); `stale`
/// = the provider was served from the on-disk cache (rendered with a `~`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsageSnapshot {
    pub claude_5h: Option<f64>,
    pub claude_wk: Option<f64>,
    pub claude_stale: bool,
    pub codex_wk: Option<f64>,
    pub codex_stale: bool,
    /// UNIX epoch SECONDS at which each window resets (for the carousel's
    /// countdown/clock). `None` = the backend didn't report it → no reset shown.
    pub claude_5h_reset: Option<i64>,
    pub claude_wk_reset: Option<i64>,
    pub codex_wk_reset: Option<i64>,
}

impl UsageSnapshot {
    pub fn is_empty(&self) -> bool {
        self.claude_5h.is_none() && self.claude_wk.is_none() && self.codex_wk.is_none()
    }

    /// Whether any window would render given the config selection `sel` — a window
    /// shows only when it is both available AND enabled. The status bar uses this
    /// (rather than [`Self::is_empty`]) so hiding every enabled window hides the readout.
    pub fn has_visible(&self, sel: UsageWindows) -> bool {
        (self.claude_5h.is_some() && sel.claude_5h)
            || (self.claude_wk.is_some() && sel.claude_wk)
            || (self.codex_wk.is_some() && sel.codex_wk)
    }

    /// Percentages: `claude 5h 5% wk 34% · codex wk 60%` (stale provider `~`-prefixed).
    pub fn text(&self) -> String {
        self.parts(None, UsageWindows::all())
            .iter()
            .map(UsagePart::text)
            .collect()
    }

    /// A progress bar per window: `claude 5h ━━╌╌╌╌╌╌ 5% wk ━━━╌╌╌╌╌ 34% · codex wk …`.
    pub fn bar(&self, width: u16) -> String {
        self.parts(Some(width), UsageWindows::all())
            .iter()
            .map(UsagePart::text)
            .collect()
    }

    /// The readout broken into render parts so the status bar can color each
    /// window by its utilization (threshold coloring) while leaving labels and
    /// separators neutral. `bar_width = None` = text; `Some(w)` = a `w`-cell bar
    /// before each percent. Only windows enabled in `sel` are emitted, and a
    /// provider label is dropped when none of its windows survive. The concatenation
    /// equals [`Self::text`]/[`Self::bar`] when `sel` is [`UsageWindows::all`].
    pub fn parts(&self, bar_width: Option<u16>, sel: UsageWindows) -> Vec<UsagePart> {
        let cell = |pct: f64| match bar_width {
            Some(w) => format!("{} ", bar_glyphs(pct, w)),
            None => String::new(),
        };
        // A window = neutral " <label> " + a gauge (`bar %`) colored by threshold.
        let window = |out: &mut Vec<UsagePart>, label: &str, pct: f64| {
            out.push(UsagePart::Neutral(format!(" {label} ")));
            out.push(UsagePart::window(format!("{}{pct:.0}%", cell(pct)), pct));
        };
        // A window renders only when both available and enabled in `sel`.
        let claude_5h = self.claude_5h.filter(|_| sel.claude_5h);
        let claude_wk = self.claude_wk.filter(|_| sel.claude_wk);
        let codex_wk = self.codex_wk.filter(|_| sel.codex_wk);
        let mut out = Vec::new();
        let has_claude = claude_5h.is_some() || claude_wk.is_some();
        if has_claude {
            out.push(UsagePart::Neutral(
                if self.claude_stale {
                    "~claude"
                } else {
                    "claude"
                }
                .to_string(),
            ));
            if let Some(p) = claude_5h {
                window(&mut out, "5h", p);
            }
            if let Some(p) = claude_wk {
                window(&mut out, "wk", p);
            }
        }
        if let Some(p) = codex_wk {
            if has_claude {
                out.push(UsagePart::Neutral(" · ".to_string()));
            }
            out.push(UsagePart::Neutral(
                if self.codex_stale { "~codex" } else { "codex" }.to_string(),
            ));
            window(&mut out, "wk", p);
        }
        out
    }

    /// Break the visible windows into carousel pages per `unit`, in the same order
    /// as the inline readout (claude 5h, claude wk, codex wk). Empty when nothing
    /// is visible — the caller then hides the readout. A page groups one window
    /// (`Window`) or all of a provider's windows (`Provider`); each entry carries
    /// its reset so the tui layer can render a countdown/clock beside the gauge.
    pub fn pages(
        &self,
        sel: UsageWindows,
        unit: PageUnit,
        reset_style: ResetStyle,
    ) -> Vec<UsagePage> {
        let mut entries = Vec::new();
        if let Some(p) = self.claude_5h.filter(|_| sel.claude_5h) {
            entries.push(UsageEntry::new(
                "claude",
                "5h",
                p,
                self.claude_5h_reset,
                self.claude_stale,
            ));
        }
        if let Some(p) = self.claude_wk.filter(|_| sel.claude_wk) {
            entries.push(UsageEntry::new(
                "claude",
                "wk",
                p,
                self.claude_wk_reset,
                self.claude_stale,
            ));
        }
        if let Some(p) = self.codex_wk.filter(|_| sel.codex_wk) {
            entries.push(UsageEntry::new(
                "codex",
                "wk",
                p,
                self.codex_wk_reset,
                self.codex_stale,
            ));
        }
        match unit {
            PageUnit::Window => entries
                .into_iter()
                .map(|e| UsagePage {
                    kind: PageKind::Full,
                    entries: vec![e],
                })
                .collect(),
            // Coalesce consecutive same-provider entries onto one page (order-preserving).
            PageUnit::Provider => {
                let mut pages: Vec<UsagePage> = Vec::new();
                for e in entries {
                    match pages.last_mut() {
                        Some(last) if last.entries[0].provider == e.provider => {
                            last.entries.push(e)
                        }
                        _ => pages.push(UsagePage {
                            kind: PageKind::Full,
                            entries: vec![e],
                        }),
                    }
                }
                pages
            }
            // Split by metric: page 1 = every window's utilization, page 2 = every
            // window's reset. The reset page is omitted when no window reports a reset
            // OR resets are turned off (`usage_reset = off`) — else it'd be a blank
            // second page with a misleading `2/2`.
            PageUnit::Metric => {
                if entries.is_empty() {
                    return Vec::new();
                }
                let mut pages = vec![UsagePage {
                    kind: PageKind::Usage,
                    entries: entries.clone(),
                }];
                if reset_style != ResetStyle::Off && entries.iter().any(|e| e.reset.is_some()) {
                    pages.push(UsagePage {
                        kind: PageKind::Reset,
                        entries,
                    });
                }
                pages
            }
        }
    }
}

/// Carousel page granularity (config `usage_page_unit`). `Window` = one rate-limit
/// window per page (most room for a reset countdown); `Provider` = all of a
/// provider's windows on a single page; `Metric` = split by METRIC — page 1 is
/// every window's utilization, page 2 every window's reset time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PageUnit {
    #[default]
    Window,
    Provider,
    Metric,
}

/// What a carousel page shows for each of its entries. `Full` = gauge + reset
/// together (Window/Provider units); `Usage`/`Reset` are the two halves of the
/// `Metric` unit (utilization-only / reset-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    Full,
    Usage,
    Reset,
}

/// How a window's reset time is shown in the carousel (config `usage_reset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResetStyle {
    /// Countdown to reset, e.g. `⟳2h13m`.
    #[default]
    Relative,
    /// Local wall-clock of the reset instant, e.g. `⟳14:32` / `⟳Wed 09:00`.
    Absolute,
    /// Hide the reset.
    Off,
}

/// One window on a carousel page.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageEntry {
    pub provider: &'static str, // "claude" | "codex"
    pub window: &'static str,   // "5h" | "wk"
    pub pct: f64,
    pub reset: Option<i64>, // epoch seconds
    pub stale: bool,
}

impl UsageEntry {
    fn new(
        provider: &'static str,
        window: &'static str,
        pct: f64,
        reset: Option<i64>,
        stale: bool,
    ) -> Self {
        UsageEntry {
            provider,
            window,
            pct,
            reset,
            stale,
        }
    }
}

/// One carousel page: its render `kind` plus the window entries it covers (one for
/// `Window`, a provider's windows for `Provider`, every window for a `Metric` half).
#[derive(Debug, Clone, PartialEq)]
pub struct UsagePage {
    pub kind: PageKind,
    pub entries: Vec<UsageEntry>,
}

/// Render ONE carousel page into colored chunks — threshold-colored gauges,
/// neutral provider/window labels and reset text — so the tui layer can pad the
/// result to the widest page and append the `n/N` indicator. `now` is Unix
/// seconds from the caller's clock; the countdown refreshes as the render loop
/// repaints on each `HH:MM` rollover.
pub fn page_parts(
    page: &UsagePage,
    bar_width: Option<u16>,
    now: i64,
    reset_style: ResetStyle,
) -> Vec<UsagePart> {
    // What each entry contributes: `Full` = gauge + reset; the `Metric` halves show
    // one or the other. Entries are grouped by provider inline (a ` · ` separator +
    // one heading per provider), so this covers a single-provider page (Window/
    // Provider units → one group, no separator) and a cross-provider metric page alike.
    let (show_gauge, show_reset) = match page.kind {
        PageKind::Full => (true, true),
        PageKind::Usage => (true, false),
        PageKind::Reset => (false, true),
    };
    // On a reset-ONLY page a window with no resolvable reset carries no value, so
    // drop it (and any provider heading that would be left dangling) rather than
    // print a bare `5h ` label. Full/Usage pages always have a gauge, so keep all.
    let entries: Vec<&UsageEntry> = if page.kind == PageKind::Reset {
        page.entries
            .iter()
            .filter(|e| {
                e.reset
                    .and_then(|r| format_reset(r, now, reset_style))
                    .is_some()
            })
            .collect()
    } else {
        page.entries.iter().collect()
    };
    let mut out = Vec::new();
    let mut cur_provider: Option<&str> = None;
    for e in entries {
        if cur_provider != Some(e.provider) {
            if cur_provider.is_some() {
                out.push(UsagePart::Neutral(" · ".to_string()));
            }
            out.push(UsagePart::Neutral(if e.stale {
                format!("~{}", e.provider)
            } else {
                e.provider.to_string()
            }));
            cur_provider = Some(e.provider);
        }
        out.push(UsagePart::Neutral(format!(" {} ", e.window)));
        if show_gauge {
            let gauge = match bar_width {
                Some(w) => format!("{} {:.0}%", bar_glyphs(e.pct, w), e.pct),
                None => format!("{:.0}%", e.pct),
            };
            out.push(UsagePart::window(gauge, e.pct));
        }
        if show_reset
            && let Some(r) = e.reset
            && let Some(s) = format_reset(r, now, reset_style)
        {
            // A leading space only when a gauge precedes it (`… 4% ⟳ 5d19h`); on a
            // reset-only page the window label already carries the trailing space.
            // The space AFTER `⟳` is load-bearing: `⟳` (U+27F3) is ambiguous-width
            // and some fonts draw it wider than its one cell, so butting a digit
            // against it overlaps — same reason the `●`/`⬆`/`⚑` chips are spaced.
            let sep = if show_gauge { " " } else { "" };
            out.push(UsagePart::Neutral(format!("{sep}⟳ {s}")));
        }
    }
    out
}

/// Human reset text per `usage_reset`. `None` = hidden (`Off`). Pure for
/// `Relative`; `Absolute` reads the local zone via libc (same path as the
/// status-bar clock — no chrono dep).
pub fn format_reset(reset_epoch: i64, now: i64, style: ResetStyle) -> Option<String> {
    match style {
        ResetStyle::Off => None,
        ResetStyle::Relative => Some(fmt_relative(reset_epoch - now)),
        ResetStyle::Absolute => Some(fmt_absolute(reset_epoch, now)),
    }
}

/// `delta` seconds until reset → `2h13m` / `3d04h` / `05m` / `<1m` / `now`.
/// Minutes/hours are zero-padded so a page's width doesn't jitter as the
/// countdown ticks (the tui layer additionally pads to the widest page).
fn fmt_relative(delta: i64) -> String {
    if delta <= 0 {
        return "now".to_string();
    }
    if delta < 60 {
        return "<1m".to_string();
    }
    let mins = delta / 60;
    let (d, h, m) = (mins / 1440, (mins % 1440) / 60, mins % 60);
    if d > 0 {
        format!("{d}d{h:02}h")
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m:02}m")
    }
}

/// Local wall-clock of the reset instant: `14:32` within a day, else `Wed 09:00`.
/// libc `localtime_r`, mirroring the status-bar clock (no chrono dep).
fn fmt_absolute(reset_epoch: i64, now: i64) -> String {
    let t = reset_epoch as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `t` is a valid time_t; `tm` is a zeroed, correctly-sized out-param.
    let r = unsafe { libc::localtime_r(&t as *const libc::time_t, &mut tm) };
    if r.is_null() {
        return "--:--".to_string();
    }
    let hm = format!("{:02}:{:02}", tm.tm_hour, tm.tm_min);
    // Within the next day → just the clock; further out → prefix the weekday.
    if reset_epoch - now < 24 * 3600 {
        hm
    } else {
        const WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let wd = WD
            .get(tm.tm_wday.clamp(0, 6) as usize)
            .copied()
            .unwrap_or("");
        format!("{wd} {hm}")
    }
}

/// One piece of the rendered readout. `Window` chunks carry their utilization so
/// the caller can color them by threshold; `Neutral` is labels/separators.
#[derive(Debug, Clone, PartialEq)]
pub enum UsagePart {
    Window { text: String, pct: f64 },
    Neutral(String),
}

impl UsagePart {
    fn window(text: String, pct: f64) -> Self {
        UsagePart::Window { text, pct }
    }

    pub fn text(&self) -> &str {
        match self {
            UsagePart::Window { text, .. } => text,
            UsagePart::Neutral(s) => s,
        }
    }
}

/// `filled`/`empty` glyphs proportional to `pct` (0–100) over `width` cells.
fn bar_glyphs(pct: f64, width: u16) -> String {
    let w = width as usize;
    let filled = ((pct / 100.0) * width as f64)
        .round()
        .clamp(0.0, width as f64) as usize;
    let mut s = String::with_capacity(w * 3);
    s.extend(std::iter::repeat_n(BAR_FILLED, filled));
    s.extend(std::iter::repeat_n(BAR_EMPTY, w - filled));
    s
}

/// Display width of the bar rendering (only the windows enabled in `sel`) — used by
/// the status bar to decide whether the terminal is wide enough for bars before
/// falling back to text.
pub fn bar_display_width(u: &UsageSnapshot, width: u16, sel: UsageWindows) -> usize {
    u.parts(Some(width), sel)
        .iter()
        .map(|p| p.text().width())
        .sum()
}

/// Latest snapshot shared with the render loop. `None` = nothing fetched yet /
/// disabled; `Some(empty)` = fetched but nothing to show.
pub type Shared = Arc<Mutex<Option<UsageSnapshot>>>;

/// An empty handle with no poller behind it (default before `spawn`, and in
/// tests that construct an `App` without a server).
pub fn idle() -> Shared {
    Arc::new(Mutex::new(None))
}

/// Spawn the detached poller thread and return the handle the status bar reads.
/// `COPAD_MUX_USAGE=0` returns an idle handle that stays empty forever.
pub fn spawn() -> Shared {
    let shared = idle();
    if std::env::var("COPAD_MUX_USAGE").is_ok_and(|v| v == "0") {
        return shared;
    }
    let out = shared.clone();
    let _ = std::thread::Builder::new()
        .name("usage-poll".into())
        .spawn(move || {
            loop {
                if let Some(s) = fetch()
                    && let Ok(mut g) = out.lock()
                {
                    *g = Some(s);
                }
                std::thread::sleep(POLL);
            }
        });
    shared
}

/// Resolve both providers' rate-limit windows in-process. `None` = `HOME` is
/// unset (can't locate credentials/rollouts) → keep the previous snapshot;
/// `Some(snapshot)` (possibly empty) = a fresh reading. The shared crate never
/// fails the call itself — an unavailable provider is simply an empty window
/// (backfilled from the short-lived cache and flagged stale where possible).
fn fetch() -> Option<UsageSnapshot> {
    let home = std::env::var("HOME").ok()?;
    // Diagnostics (why a provider is missing) are for `coctl`'s stderr; the
    // status bar only shows values, so ignore them here.
    let (limits, stale, _diags) = copad_usage::limits::resolve(&home, true, true);
    let mut snap = UsageSnapshot::default();
    if let Some(c) = &limits.claude {
        snap.claude_5h = c.five_hour;
        snap.claude_wk = c.seven_day;
        snap.claude_5h_reset = c.five_hour_reset;
        snap.claude_wk_reset = c.seven_day_reset;
        snap.claude_stale = stale.claude;
    }
    if let Some(x) = &limits.codex {
        snap.codex_wk = x.weekly;
        snap.codex_wk_reset = x.weekly_reset;
        snap.codex_stale = stale.codex;
    }
    Some(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> UsageSnapshot {
        UsageSnapshot {
            claude_5h: Some(5.0),
            claude_wk: Some(34.0),
            claude_stale: false,
            codex_wk: Some(60.0),
            codex_stale: false,
            ..Default::default()
        }
    }

    #[test]
    fn text_matches_percent_format() {
        assert_eq!(full().text(), "claude 5h 5% wk 34% · codex wk 60%");
    }

    #[test]
    fn parts_carry_pct_and_concat_to_text() {
        let parts = full().parts(None, UsageWindows::all());
        // Each gauge chunk carries its utilization (for threshold coloring); the
        // order is claude 5h, claude wk, codex wk.
        let pcts: Vec<f64> = parts
            .iter()
            .filter_map(|p| match p {
                UsagePart::Window { pct, .. } => Some(*pct),
                UsagePart::Neutral(_) => None,
            })
            .collect();
        assert_eq!(pcts, vec![5.0, 34.0, 60.0]);
        // Concatenation is byte-identical to the flat text form.
        let concat: String = parts.iter().map(UsagePart::text).collect();
        assert_eq!(concat, full().text());
    }

    #[test]
    fn window_selection_filters_and_drops_empty_labels() {
        let u = full();
        // Claude weekly only + Codex weekly → 5h is dropped, claude label stays.
        let sel = UsageWindows {
            claude_5h: false,
            claude_wk: true,
            codex_wk: true,
        };
        let s: String = u.parts(None, sel).iter().map(UsagePart::text).collect();
        assert_eq!(s, "claude wk 34% · codex wk 60%");
        assert!(u.has_visible(sel));

        // Disabling every Claude window drops the whole `claude` label (no orphan).
        let codex_only = UsageWindows {
            claude_5h: false,
            claude_wk: false,
            codex_wk: true,
        };
        let s: String = u
            .parts(None, codex_only)
            .iter()
            .map(UsagePart::text)
            .collect();
        assert_eq!(s, "codex wk 60%");

        // Nothing enabled → nothing rendered, and `has_visible` is false so the bar hides.
        assert!(u.parts(None, UsageWindows::none()).is_empty());
        assert!(!u.has_visible(UsageWindows::none()));

        // An enabled-but-unavailable window doesn't make it visible.
        let missing = UsageSnapshot::default();
        assert!(!missing.has_visible(UsageWindows::all()));
    }

    #[test]
    fn text_marks_stale_provider() {
        let mut u = full();
        u.claude_stale = true;
        assert_eq!(u.text(), "~claude 5h 5% wk 34% · codex wk 60%");
    }

    #[test]
    fn bar_glyphs_are_proportional() {
        assert_eq!(bar_glyphs(0.0, 8), "╌╌╌╌╌╌╌╌");
        assert_eq!(bar_glyphs(100.0, 8), "━━━━━━━━");
        assert_eq!(bar_glyphs(50.0, 8), "━━━━╌╌╌╌");
        // rounds to nearest cell; clamps out-of-range
        assert_eq!(bar_glyphs(12.5, 8), "━╌╌╌╌╌╌╌");
        assert_eq!(bar_glyphs(150.0, 4), "━━━━");
    }

    #[test]
    fn bar_render_has_a_bar_per_window() {
        let s = full().bar(8);
        // 5% of 8 rounds to 0 filled cells; the "5%" label carries the value.
        assert!(s.contains("5h ╌╌╌╌╌╌╌╌ 5%"), "got: {s}");
        assert!(s.contains("wk ━━━╌╌╌╌╌ 34%"), "got: {s}"); // 34% → 3/8
        assert!(s.contains("codex wk ━━━━━╌╌╌ 60%"), "got: {s}"); // 60% → 5/8
    }

    // ── carousel (paged) rendering ─────────────────────────────────────────

    #[test]
    fn fmt_relative_buckets() {
        assert_eq!(fmt_relative(-5), "now");
        assert_eq!(fmt_relative(0), "now");
        assert_eq!(fmt_relative(30), "<1m");
        assert_eq!(fmt_relative(60), "01m"); // zero-padded so width stays stable
        assert_eq!(fmt_relative(45 * 60), "45m");
        assert_eq!(fmt_relative(2 * 3600 + 13 * 60), "2h13m");
        assert_eq!(fmt_relative(2 * 3600 + 3 * 60), "2h03m");
        assert_eq!(fmt_relative(3 * 86400 + 4 * 3600), "3d04h");
    }

    #[test]
    fn format_reset_off_hides_relative_counts() {
        assert_eq!(format_reset(1000, 0, ResetStyle::Off), None);
        // 1000s = 16m40s → 16m.
        assert_eq!(
            format_reset(1000, 0, ResetStyle::Relative),
            Some("16m".to_string())
        );
    }

    #[test]
    fn pages_window_unit_is_one_window_each() {
        let pages = full().pages(UsageWindows::all(), PageUnit::Window, ResetStyle::Relative);
        assert_eq!(pages.len(), 3);
        assert!(pages.iter().all(|p| p.entries.len() == 1));
        assert_eq!(
            (pages[0].entries[0].provider, pages[0].entries[0].window),
            ("claude", "5h")
        );
        assert_eq!(
            (pages[1].entries[0].provider, pages[1].entries[0].window),
            ("claude", "wk")
        );
        assert_eq!(
            (pages[2].entries[0].provider, pages[2].entries[0].window),
            ("codex", "wk")
        );
    }

    #[test]
    fn pages_provider_unit_groups_claude_windows() {
        let pages = full().pages(
            UsageWindows::all(),
            PageUnit::Provider,
            ResetStyle::Relative,
        );
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].entries.len(), 2); // claude 5h + wk on one page
        assert_eq!(pages[0].entries[0].provider, "claude");
        assert_eq!(pages[1].entries.len(), 1); // codex wk
        assert_eq!(pages[1].entries[0].provider, "codex");
    }

    #[test]
    fn pages_respect_window_selection() {
        let sel = UsageWindows {
            claude_5h: false,
            claude_wk: true,
            codex_wk: true,
        };
        let pages = full().pages(sel, PageUnit::Window, ResetStyle::Relative);
        assert_eq!(pages.len(), 2); // 5h dropped
        assert_eq!(pages[0].entries[0].window, "wk");
        // Nothing enabled → no pages (caller hides the readout).
        assert!(
            full()
                .pages(UsageWindows::none(), PageUnit::Window, ResetStyle::Relative)
                .is_empty()
        );
    }

    #[test]
    fn page_parts_render_gauge_and_reset() {
        let mut u = full();
        u.claude_5h_reset = Some(2 * 3600 + 13 * 60); // 2h13m out from now=0
        let pages = u.pages(UsageWindows::all(), PageUnit::Window, ResetStyle::Relative);
        let parts = page_parts(&pages[0], Some(8), 0, ResetStyle::Relative);
        let s: String = parts.iter().map(UsagePart::text).collect();
        assert!(s.starts_with("claude 5h "), "got: {s}");
        assert!(s.contains("5%"), "got: {s}");
        assert!(s.contains("⟳ 2h13m"), "got: {s}");
        // The gauge chunk carries its pct (threshold coloring by the tui layer).
        assert!(
            parts
                .iter()
                .any(|p| matches!(p, UsagePart::Window { pct, .. } if *pct == 5.0))
        );
    }

    #[test]
    fn page_parts_provider_page_marks_stale_and_omits_reset_when_off() {
        let mut u = full();
        u.claude_stale = true;
        let pages = u.pages(
            UsageWindows::all(),
            PageUnit::Provider,
            ResetStyle::Relative,
        );
        let parts = page_parts(&pages[0], None, 0, ResetStyle::Off);
        let s: String = parts.iter().map(UsagePart::text).collect();
        assert!(s.starts_with("~claude"), "got: {s}"); // stale marker on the heading
        assert!(s.contains(" 5h "), "got: {s}");
        assert!(s.contains(" wk "), "got: {s}"); // both windows on the provider page
        assert!(!s.contains('⟳'), "reset hidden under Off: {s}");
    }

    #[test]
    fn pages_metric_splits_usage_and_reset() {
        let mut u = full();
        u.claude_5h_reset = Some(3600);
        u.codex_wk_reset = Some(7200);
        let pages = u.pages(UsageWindows::all(), PageUnit::Metric, ResetStyle::Relative);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].kind, PageKind::Usage);
        assert_eq!(pages[1].kind, PageKind::Reset);
        // Both halves cover every visible window.
        assert_eq!(pages[0].entries.len(), 3);
        assert_eq!(pages[1].entries.len(), 3);
    }

    #[test]
    fn pages_metric_omits_reset_page_when_reset_off() {
        // usage_reset = off → no reset page (else it'd be a blank `2/2`). Even with
        // real reset epochs present.
        let mut u = full();
        u.claude_wk_reset = Some(3600);
        u.codex_wk_reset = Some(7200);
        let pages = u.pages(UsageWindows::all(), PageUnit::Metric, ResetStyle::Off);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].kind, PageKind::Usage);
    }

    #[test]
    fn pages_metric_omits_reset_page_when_no_resets() {
        // `full()` carries no reset epochs → only the usage page (no empty reset page).
        let pages = full().pages(UsageWindows::all(), PageUnit::Metric, ResetStyle::Relative);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].kind, PageKind::Usage);
    }

    #[test]
    fn page_parts_metric_groups_providers_inline() {
        // The owner's config: weekly-only, metric split → page 1 usages, page 2 resets.
        let mut u = full();
        u.claude_wk_reset = Some(5 * 86400 + 19 * 3600); // 5d19h
        u.codex_wk_reset = Some(6 * 86400 + 21 * 3600); // 6d21h
        let sel = UsageWindows {
            claude_5h: false,
            claude_wk: true,
            codex_wk: true,
        };
        let pages = u.pages(sel, PageUnit::Metric, ResetStyle::Relative);
        assert_eq!(pages.len(), 2);
        let usage: String = page_parts(&pages[0], None, 0, ResetStyle::Relative)
            .iter()
            .map(UsagePart::text)
            .collect();
        assert_eq!(usage, "claude wk 34% · codex wk 60%");
        let reset: String = page_parts(&pages[1], None, 0, ResetStyle::Relative)
            .iter()
            .map(UsagePart::text)
            .collect();
        assert_eq!(reset, "claude wk ⟳ 5d19h · codex wk ⟳ 6d21h");
    }

    #[test]
    fn page_parts_metric_reset_page_drops_windows_without_a_reset() {
        // Claude's reset is unknown (e.g. token lapsed), Codex's is known → the reset
        // page shows ONLY codex, with no dangling `claude 5h  wk ` labels.
        let mut u = full();
        u.claude_5h_reset = None;
        u.claude_wk_reset = None;
        u.codex_wk_reset = Some(6 * 86400 + 21 * 3600);
        let pages = u.pages(UsageWindows::all(), PageUnit::Metric, ResetStyle::Relative);
        assert_eq!(pages.len(), 2); // reset page exists (codex has one)
        let reset: String = page_parts(&pages[1], None, 0, ResetStyle::Relative)
            .iter()
            .map(UsagePart::text)
            .collect();
        assert_eq!(reset, "codex wk ⟳ 6d21h");
    }

    /// Live smoke: resolve the REAL rate-limit windows (Codex works offline; Claude
    /// needs a valid token) and print each carousel page exactly as the status bar
    /// would render it, for both page units and all reset styles. `#[ignore]` so it
    /// never runs in CI (network / machine-specific) — run explicitly to eyeball:
    ///   cargo test -p copad-mux --lib usagepoll::tests::live_carousel_smoke -- --ignored --nocapture
    #[test]
    #[ignore]
    fn live_carousel_smoke() {
        let Some(snap) = fetch() else {
            eprintln!("HOME unset — cannot resolve");
            return;
        };
        eprintln!("snapshot: {snap:?}");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;
        for unit in [PageUnit::Window, PageUnit::Provider, PageUnit::Metric] {
            let pages = snap.pages(UsageWindows::all(), unit, ResetStyle::Relative);
            eprintln!("\n== page_unit={unit:?} ({} pages) ==", pages.len());
            for (i, p) in pages.iter().enumerate() {
                for style in [ResetStyle::Relative, ResetStyle::Absolute, ResetStyle::Off] {
                    let s: String = page_parts(p, Some(8), now, style)
                        .iter()
                        .map(UsagePart::text)
                        .collect();
                    eprintln!("  [{}/{}] {style:?}: {s}", i + 1, pages.len());
                }
            }
        }
    }
}
