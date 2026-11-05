//! Background poller for the status-bar usage/limits readout. Resolves rate-limit windows
//! in-process via `copad_usage::limits` (network + disk I/O, so on a dedicated thread, never
//! the render loop) into numbers, so the bar can render text or a gauge. `COPAD_MUX_USAGE=0`
//! disables it.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

const POLL: Duration = Duration::from_secs(60);

const BAR_FILLED: char = '━';
const BAR_EMPTY: char = '╌';

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
    pub fn all() -> Self {
        UsageWindows {
            claude_5h: true,
            claude_wk: true,
            codex_wk: true,
        }
    }

    pub fn none() -> Self {
        UsageWindows {
            claude_5h: false,
            claude_wk: false,
            codex_wk: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct UsageSnapshot {
    pub claude_5h: Option<f64>,
    pub claude_wk: Option<f64>,
    pub claude_stale: bool,
    pub codex_wk: Option<f64>,
    pub codex_stale: bool,
    pub claude_5h_reset: Option<i64>,
    pub claude_wk_reset: Option<i64>,
    pub codex_wk_reset: Option<i64>,
}

impl UsageSnapshot {
    pub fn is_empty(&self) -> bool {
        self.claude_5h.is_none() && self.claude_wk.is_none() && self.codex_wk.is_none()
    }

    pub fn has_visible(&self, sel: UsageWindows) -> bool {
        (self.claude_5h.is_some() && sel.claude_5h)
            || (self.claude_wk.is_some() && sel.claude_wk)
            || (self.codex_wk.is_some() && sel.codex_wk)
    }

    pub fn text(&self) -> String {
        self.parts(None, UsageWindows::all())
            .iter()
            .map(UsagePart::text)
            .collect()
    }

    pub fn bar(&self, width: u16) -> String {
        self.parts(Some(width), UsageWindows::all())
            .iter()
            .map(UsagePart::text)
            .collect()
    }

    pub fn parts(&self, bar_width: Option<u16>, sel: UsageWindows) -> Vec<UsagePart> {
        let cell = |pct: f64| match bar_width {
            Some(w) => format!("{} ", bar_glyphs(pct, w)),
            None => String::new(),
        };
        let window = |out: &mut Vec<UsagePart>, label: &str, pct: f64| {
            out.push(UsagePart::Neutral(format!(" {label} ")));
            out.push(UsagePart::window(format!("{}{pct:.0}%", cell(pct)), pct));
        };
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PageUnit {
    #[default]
    Window,
    Provider,
    Metric,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    Full,
    Usage,
    Reset,
}

/// How a window's reset time is shown in the carousel (config `usage_reset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResetStyle {
    #[default]
    Relative,
    Absolute,
    /// Hide the reset.
    Off,
}

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

#[derive(Debug, Clone, PartialEq)]
pub struct UsagePage {
    pub kind: PageKind,
    pub entries: Vec<UsageEntry>,
}

pub fn page_parts(
    page: &UsagePage,
    bar_width: Option<u16>,
    now: i64,
    reset_style: ResetStyle,
) -> Vec<UsagePart> {
    let (show_gauge, show_reset) = match page.kind {
        PageKind::Full => (true, true),
        PageKind::Usage => (true, false),
        PageKind::Reset => (false, true),
    };
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
            // The space after `⟳` is load-bearing: U+27F3 is ambiguous-width and some fonts draw it
            // wider than one cell, so a digit butted against it overlaps.
            let sep = if show_gauge { " " } else { "" };
            out.push(UsagePart::Neutral(format!("{sep}⟳ {s}")));
        }
    }
    out
}

pub fn format_reset(reset_epoch: i64, now: i64, style: ResetStyle) -> Option<String> {
    match style {
        ResetStyle::Off => None,
        ResetStyle::Relative => Some(fmt_relative(reset_epoch - now)),
        ResetStyle::Absolute => Some(fmt_absolute(reset_epoch, now)),
    }
}

/// `delta` seconds until reset → `2h13m` / `3d04h` / `05m` / `<1m` / `now`. Minutes and hours are
/// zero-padded so a page's width does not jitter as the countdown ticks.
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

fn fmt_absolute(reset_epoch: i64, now: i64) -> String {
    let t = reset_epoch as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::localtime_r(&t as *const libc::time_t, &mut tm) };
    if r.is_null() {
        return "--:--".to_string();
    }
    let hm = format!("{:02}:{:02}", tm.tm_hour, tm.tm_min);
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

pub(crate) fn bar_glyphs(pct: f64, width: u16) -> String {
    let w = width as usize;
    let filled = ((pct / 100.0) * width as f64)
        .round()
        .clamp(0.0, width as f64) as usize;
    let mut s = String::with_capacity(w * 3);
    s.extend(std::iter::repeat_n(BAR_FILLED, filled));
    s.extend(std::iter::repeat_n(BAR_EMPTY, w - filled));
    s
}

pub fn bar_display_width(u: &UsageSnapshot, width: u16, sel: UsageWindows) -> usize {
    u.parts(Some(width), sel)
        .iter()
        .map(|p| p.text().width())
        .sum()
}

pub type Shared = Arc<Mutex<Option<UsageSnapshot>>>;

pub fn idle() -> Shared {
    Arc::new(Mutex::new(None))
}

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

/// Resolve both providers' rate-limit windows in-process. `None` = `HOME` is unset, so keep the
/// previous snapshot; `Some` (possibly empty) = a fresh reading.
fn fetch() -> Option<UsageSnapshot> {
    let home = std::env::var("HOME").ok()?;
    // Diagnostics are for `coctl`'s stderr; the status bar only shows values.
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
        let pcts: Vec<f64> = parts
            .iter()
            .filter_map(|p| match p {
                UsagePart::Window { pct, .. } => Some(*pct),
                UsagePart::Neutral(_) => None,
            })
            .collect();
        assert_eq!(pcts, vec![5.0, 34.0, 60.0]);
        let concat: String = parts.iter().map(UsagePart::text).collect();
        assert_eq!(concat, full().text());
    }

    #[test]
    fn window_selection_filters_and_drops_empty_labels() {
        let u = full();
        let sel = UsageWindows {
            claude_5h: false,
            claude_wk: true,
            codex_wk: true,
        };
        let s: String = u.parts(None, sel).iter().map(UsagePart::text).collect();
        assert_eq!(s, "claude wk 34% · codex wk 60%");
        assert!(u.has_visible(sel));

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

        assert!(u.parts(None, UsageWindows::none()).is_empty());
        assert!(!u.has_visible(UsageWindows::none()));

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
        assert_eq!(bar_glyphs(12.5, 8), "━╌╌╌╌╌╌╌");
        assert_eq!(bar_glyphs(150.0, 4), "━━━━");
    }

    #[test]
    fn bar_render_has_a_bar_per_window() {
        let s = full().bar(8);
        assert!(s.contains("5h ╌╌╌╌╌╌╌╌ 5%"), "got: {s}");
        assert!(s.contains("wk ━━━╌╌╌╌╌ 34%"), "got: {s}"); // 34% → 3/8
        assert!(s.contains("codex wk ━━━━━╌╌╌ 60%"), "got: {s}"); // 60% → 5/8
    }

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
