//! A tiny inline fuzzy picker for the `comux` CLI: a control verb given no target lists the
//! live candidates and lets you type to narrow them.
//!
//! It draws INLINE on **stderr** rather than in the alternate screen, which keeps the shell's
//! scrollback intact, and stdout stays clean for `--json` / pipes. Callers gate on
//! [`interactive`] first: a non-terminal stderr or `--json` keeps the usage error, so scripts
//! fail fast instead of blocking on a prompt nobody can see.

use std::io::{self, IsTerminal, Write};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType, disable_raw_mode, enable_raw_mode};
use crossterm::{cursor, queue};
use unicode_width::UnicodeWidthChar;

/// One selectable row. `label` is the identity the user thinks in; `detail` is dim context.
/// Both are fuzzy-matched.
#[derive(Debug, Clone)]
pub struct Item {
    pub label: String,
    pub detail: String,
}

impl Item {
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
        }
    }

    fn haystack(&self) -> String {
        if self.detail.is_empty() {
            self.label.clone()
        } else {
            format!("{} {}", self.label, self.detail)
        }
    }
}

/// Most rows shown at once; the inline block must stay prompt-sized.
const MAX_ROWS: usize = 12;

/// Case-insensitive subsequence match (fzf-style), shared with the TUI's `Ctrl-f` switcher so
/// both filters behave identically.
pub fn fuzzy_match(needle: &str, hay: &str) -> bool {
    let hay = hay.to_ascii_lowercase();
    let mut chars = hay.chars();
    needle
        .to_ascii_lowercase()
        .chars()
        .all(|nc| chars.any(|hc| hc == nc))
}

/// True when stderr (where the picker draws) is a terminal. Callers also refuse in `--json`
/// mode: a machine-readable invocation must never turn into a prompt.
pub fn interactive() -> bool {
    io::stderr().is_terminal()
}

/// Returns the chosen index into `items`, or `None` if cancelled (`Esc` / `Ctrl-c`). `Err`
/// only for a terminal I/O failure, which the caller treats as "could not prompt".
///
/// The screen guard erases the block and restores cooked mode on any exit, panics included.
pub fn pick(title: &str, items: &[Item]) -> Result<Option<usize>, String> {
    if items.is_empty() {
        return Ok(None);
    }
    let mut screen = Screen::enter().map_err(|e| format!("terminal: {e}"))?;
    let mut query = String::new();
    let mut sel = 0usize;
    let mut offset = 0usize;
    loop {
        let matches = filter(items, &query);
        sel = sel.min(matches.len().saturating_sub(1));
        let view = view_rows();
        offset = scroll_offset(sel, offset, view);
        screen
            .draw(title, &query, items, &matches, sel, offset, view)
            .map_err(|e| format!("terminal: {e}"))?;

        let ev = event::read().map_err(|e| format!("terminal: {e}"))?;
        let Event::Key(key) = ev else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let last = matches.len().saturating_sub(1);
        match key_action(&key) {
            Action::Accept => {
                if let Some(&i) = matches.get(sel) {
                    return Ok(Some(i));
                }
            }
            Action::Cancel => return Ok(None),
            Action::Up => sel = sel.saturating_sub(1),
            Action::Down => sel = (sel + 1).min(last),
            Action::PageUp => sel = sel.saturating_sub(view),
            Action::PageDown => sel = (sel + view).min(last),
            Action::Insert(c) => {
                query.push(c);
                sel = 0;
                offset = 0;
            }
            Action::Backspace => {
                query.pop();
                sel = 0;
                offset = 0;
            }
            Action::ClearQuery => {
                query.clear();
                sel = 0;
                offset = 0;
            }
            Action::Ignore => {}
        }
    }
}

/// Indices of `items` matching `query`: rows whose LABEL matches first, then rows that only
/// matched on their detail, each group in listing order (the indices are meaningful).
///
/// Detail text is boilerplate-heavy (`1 tabs · 1 panes`) and a subsequence filter matches it
/// readily: `alp` for session `alpha` also matched `local  1 tabs · 1 panes` and, being earlier
/// in the list, selected the wrong row.
fn filter(items: &[Item], query: &str) -> Vec<usize> {
    let mut by_label = Vec::new();
    let mut by_detail = Vec::new();
    for (i, it) in items.iter().enumerate() {
        if fuzzy_match(query, &it.label) {
            by_label.push(i);
        } else if fuzzy_match(query, &it.haystack()) {
            by_detail.push(i);
        }
    }
    by_label.extend(by_detail);
    by_label
}

fn scroll_offset(sel: usize, offset: usize, view: usize) -> usize {
    let view = view.max(1);
    if sel < offset {
        sel
    } else if sel >= offset + view {
        sel + 1 - view
    } else {
        offset
    }
}

fn view_rows() -> usize {
    let rows = terminal::size().map(|(_, r)| r as usize).unwrap_or(24);
    MAX_ROWS.min(rows.saturating_sub(3)).max(1)
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Accept,
    Cancel,
    Up,
    Down,
    PageUp,
    PageDown,
    Insert(char),
    Backspace,
    ClearQuery,
    Ignore,
}

/// fzf/readline bindings. Bare `j`/`k` can NOT move the selection here (unlike the TUI
/// switcher) — every printable key belongs to the filter — so vertical motion is
/// arrows or `Ctrl-n`/`Ctrl-p`.
fn key_action(k: &KeyEvent) -> Action {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Enter => Action::Accept,
        KeyCode::Esc => Action::Cancel,
        KeyCode::Up => Action::Up,
        KeyCode::Down => Action::Down,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Char(c) if ctrl => match c.to_ascii_lowercase() {
            'c' | 'd' | 'g' | 'q' => Action::Cancel,
            'n' | 'j' => Action::Down,
            'p' | 'k' => Action::Up,
            'u' => Action::ClearQuery,
            'h' => Action::Backspace,
            _ => Action::Ignore,
        },
        // Any other printable char extends the filter. ALT-modified chars are terminal
        // shortcuts, not input.
        KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::ALT) => Action::Insert(c),
        _ => Action::Ignore,
    }
}

/// Truncate to at most `width - 1` display columns (wide glyphs count as 2), appending
/// `…` when it had to cut — so a long path never wraps and breaks the block's line
/// accounting. The last column is left empty ON PURPOSE, ellipsis included: writing into
/// it arms deferred auto-wrap on most terminals, which would silently add a line the
/// erase math doesn't know about.
fn truncate(s: &str, width: usize) -> String {
    let col = |c: char| UnicodeWidthChar::width(c).unwrap_or(0);
    let limit = width.saturating_sub(1);
    if limit == 0 {
        return String::new();
    }
    if s.chars().map(col).sum::<usize>() <= limit {
        return s.to_string();
    }
    let content = limit - 1;
    let mut used = 0usize;
    let mut out = String::new();
    for c in s.chars() {
        let w = col(c);
        if used + w > content {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// Owns the inline block on stderr: raw mode + a hidden cursor while it lives, and on
/// drop it erases every line it drew and restores the terminal. Drop runs on unwind
/// too, so a panic mid-pick can't leave the shell in raw mode.
struct Screen {
    drawn: u16,
}

impl Screen {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // Bind the guard BEFORE anything else that can fail: a `?` below then drops it
        // and restores cooked mode. Constructing it last would leave the terminal raw
        // with nothing left to undo it.
        let screen = Self { drawn: 0 };
        let mut out = io::stderr();
        queue!(out, cursor::Hide)?;
        out.flush()?;
        Ok(screen)
    }

    /// Move back to the top of the block and wipe it. Safe to call repeatedly; the
    /// cursor ends where the block started. The block is capped to the terminal height
    /// (see [`view_rows`]), so its top never scrolls off — which is what makes the
    /// relative `MoveUp` accounting hold even when printing scrolled the screen.
    fn erase(&mut self) -> io::Result<()> {
        if self.drawn == 0 {
            return Ok(());
        }
        let mut out = io::stderr();
        queue!(
            out,
            cursor::MoveToColumn(0),
            cursor::MoveUp(self.drawn),
            Clear(ClearType::FromCursorDown)
        )?;
        out.flush()?;
        self.drawn = 0;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(
        &mut self,
        title: &str,
        query: &str,
        items: &[Item],
        matches: &[usize],
        sel: usize,
        offset: usize,
        view: usize,
    ) -> io::Result<()> {
        self.erase()?;
        // A reported 0 means the terminal never told us its size (an unsized PTY), not a
        // zero-column screen — fall back rather than render nothing. A genuinely narrow
        // terminal is used AS-IS: clamping the width up would guarantee exactly the
        // wrapping `truncate` exists to prevent.
        let width = match terminal::size() {
            Ok((cols, _)) if cols > 0 => cols as usize,
            _ => 80,
        };
        let mut out = io::stderr();
        let mut lines = 0u16;

        let header = format!(
            "{title}  [{}/{}]  ↑↓ move · Enter select · Esc cancel",
            matches.len(),
            items.len()
        );
        queue!(
            out,
            SetAttribute(Attribute::Dim),
            Print(truncate(&header, width)),
            SetAttribute(Attribute::Reset),
            Print("\r\n"),
            Print(truncate(&format!("> {query}█"), width)),
            Print("\r\n")
        )?;
        lines += 2;

        if matches.is_empty() {
            queue!(
                out,
                SetAttribute(Attribute::Dim),
                // Truncated like every other row: an untruncated literal wraps on a very narrow terminal and
                // adds a physical line `drawn` doesn't count.
                Print(truncate("  (no match)", width)),
                SetAttribute(Attribute::Reset),
                Print("\r\n")
            )?;
            lines += 1;
        }
        for (row, &i) in matches.iter().enumerate().skip(offset).take(view) {
            let it = &items[i];
            let selected = row == sel;
            let arrow = if selected { "▸" } else { " " };
            let text = if it.detail.is_empty() {
                format!("{arrow} {}", it.label)
            } else {
                format!("{arrow} {}  {}", it.label, it.detail)
            };
            if selected {
                queue!(out, SetAttribute(Attribute::Reverse))?;
            }
            queue!(
                out,
                Print(truncate(&text, width)),
                SetAttribute(Attribute::Reset),
                Print("\r\n")
            )?;
            lines += 1;
        }

        out.flush()?;
        self.drawn = lines;
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self.erase();
        let mut out = io::stderr();
        let _ = queue!(out, cursor::Show);
        let _ = out.flush();
        let _ = disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, detail: &str) -> Item {
        Item::new(label, detail)
    }

    #[test]
    fn fuzzy_match_is_case_insensitive_subsequence() {
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("api", "api-server"));
        assert!(fuzzy_match("api", "API-Server"));
        assert!(fuzzy_match("aps", "api-server"));
        assert!(fuzzy_match("cld", "claude"));
        assert!(!fuzzy_match("xyz", "claude"));
        assert!(!fuzzy_match("sa", "api-server"));
    }

    #[test]
    fn filter_matches_label_and_detail_and_keeps_order() {
        let items = vec![
            item("feat/login", "/home/u/repo-feat-login"),
            item("fix/crash", "/home/u/repo-fix-crash · live"),
            item("main", "/home/u/repo"),
        ];
        assert_eq!(filter(&items, ""), vec![0, 1, 2]);
        assert_eq!(filter(&items, "login"), vec![0]);
        assert_eq!(filter(&items, "live"), vec![1]);
        assert!(filter(&items, "zzz").is_empty());
    }

    #[test]
    fn filter_ranks_label_matches_above_detail_only_matches() {
        // The real regression: `alp` subsequence-matches "loc(al)  1 tabs · 1 (p)anes"
        // through its boilerplate detail, so listing order alone selected the wrong row.
        let items = vec![
            item("0: local", "1 tabs · 1 panes"),
            item("1: alpha", "1 tabs · 1 panes"),
        ];
        assert_eq!(filter(&items, "alp"), vec![1, 0]);
    }

    #[test]
    fn scroll_offset_moves_only_when_selection_leaves_the_window() {
        assert_eq!(scroll_offset(0, 0, 3), 0);
        assert_eq!(scroll_offset(2, 0, 3), 0);
        assert_eq!(scroll_offset(3, 0, 3), 1);
        assert_eq!(scroll_offset(1, 4, 3), 1);
        assert_eq!(scroll_offset(5, 0, 0), 5);
    }

    #[test]
    fn key_bindings_cover_filter_and_motion() {
        let key = |code, mods| KeyEvent::new(code, mods);
        assert_eq!(
            key_action(&key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Accept
        );
        assert_eq!(
            key_action(&key(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Cancel
        );
        assert_eq!(
            key_action(&key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Cancel
        );
        assert_eq!(
            key_action(&key(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            Action::Down
        );
        assert_eq!(
            key_action(&key(KeyCode::Char('j'), KeyModifiers::NONE)),
            Action::Insert('j')
        );
        assert_eq!(
            key_action(&key(KeyCode::Char('J'), KeyModifiers::SHIFT)),
            Action::Insert('J')
        );
        assert_eq!(
            key_action(&key(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            Action::ClearQuery
        );
    }

    #[test]
    fn truncate_respects_display_width() {
        let cols = |s: &str| {
            s.chars()
                .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                .sum::<usize>()
        };
        assert_eq!(truncate("abc", 10), "abc");
        // The last column stays empty, ellipsis included: writing into it arms deferred
        // auto-wrap, which breaks the erase math.
        assert_eq!(truncate("abcdef", 4), "ab…");
        assert!(cols(&truncate("abcdef", 4)) < 4);
        assert_eq!(truncate("한글", 4), "한…");
        assert!(cols(&truncate("한글자", 6)) < 6);
        assert_eq!(truncate("abc", 1), "");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn empty_item_list_is_never_a_selection() {
        assert_eq!(pick("nothing", &[]).unwrap(), None);
    }
}
