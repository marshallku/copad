//! User configuration for copad-mux: `~/.config/copad/mux.toml`.
//!
//! Mirrors the owner's own `tmx` config conventions (copad-mux already ports tmx's
//! agent-status parser): TOML, overlay-merge onto built-in defaults (a partial config
//! keeps every unspecified default), warn-once on an invalid file/binding, and
//! action→chord key tables where an override REPLACES that action's default chord set.
//!
//! Zero-config users get behavior IDENTICAL to the previous hardcoded bindings — every
//! default here reproduces what `feed_key` used to match literally.
//!
//! Design notes (from the codex plan review, decisions #67):
//! - Bindings are **action → many chords** so aliases survive (`detach = d | q`,
//!   `focus-left = h | Left`, prefix `1..9` + global `M-1..9`).
//! - A live `KeyEvent` and a parsed config token are canonicalized the SAME way
//!   ([`chord_of`] / [`parse_chord`]) so they compare equal. Raw control bytes
//!   (`\u{2}` = `C-b`, `\u{6}` = `C-f`) are mapped to `ctrl`+letter FIRST.
//! - Collisions resolve by declaration order (deterministic) and emit a warning; a
//!   global binding equal to the prefix chord is dropped so prefix entry always wins.
//! - `load()` returns structured diagnostics (`Vec<String>`) rather than only touching
//!   stderr, so warnings are testable and the foreground client can print them.

use std::collections::HashMap;
use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

// ---- default constants (the config clamps toward / falls back to these) ----
pub const DEFAULT_SIDEBAR_WIDTH: u16 = 24;
pub const DEFAULT_SIDEBAR_MIN_COLS: u16 = 80;
pub const DEFAULT_SCROLL_STEP: i32 = 3;
/// Default periodic autosave interval (seconds) for session persistence.
pub const DEFAULT_AUTOSAVE_SECS: u32 = 15;
/// Default cells per progress bar for `usage = "bar"`.
pub const DEFAULT_USAGE_BAR_WIDTH: u16 = 8;
/// Minimum pane-content width kept to the right of the sidebar; `sidebar_min_cols`
/// is forced to at least `sidebar_width + this` so a visible sidebar can never eat
/// the whole viewport.
const MIN_CONTENT_COLS: u16 = 20;

/// The default `restore_processes` whitelist: the built-in AI-agent basenames.
fn default_restore_processes() -> Vec<String> {
    crate::procinfo::agent_basenames()
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// The default `update_environment` list — the volatile per-login-session variables
/// that a persistent server must refresh from the attaching client (tmux
/// `update-environment` plus the modern-desktop set). Deliberately excludes anything
/// load-bearing for a shell (see [`ENV_UPDATE_BLOCKLIST`]); these names are both
/// scrubbed from the daemon at startup and re-injected per-pane from the latest client,
/// so a pane never inherits a stale SSH/display session from wherever the server was born.
pub fn default_update_environment() -> Vec<String> {
    [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "XDG_SESSION_TYPE",
        "DBUS_SESSION_BUS_ADDRESS",
        "SSH_ASKPASS",
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "SSH_CONNECTION",
        "SSH_CLIENT",
        "SSH_TTY",
        "WINDOWID",
        "KRB5CCNAME",
        "TERM_PROGRAM",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Names refused in `update_environment`: load-bearing shell/process variables whose
/// removal (daemon scrub) or client-driven override would break PATH lookup, the login
/// shell, the home dir, or comux's own pane markers. A configured entry matching this
/// list is dropped with a warning rather than silently honored.
const ENV_UPDATE_BLOCKLIST: &[&str] = &[
    "PATH",
    "HOME",
    "SHELL",
    "USER",
    "LOGNAME",
    "PWD",
    "OLDPWD",
    "TERM",
    "SHLVL",
    "_",
    "COPAD_MUX",
    "COPAD_MUX_SOCK",
];

/// How sessions are ordered in the sidebar + `Ctrl-f` switcher + `)`/`(` cycling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortBy {
    /// Creation order (default; new sessions append).
    Created,
    /// By name, case-insensitive (like `tmx`).
    Alphabetical,
    /// Most-recently-switched-to first (MRU).
    Recent,
    /// Sessions with an active (working/blocked) agent first.
    Activity,
}

/// How many rows one sidebar entry (a session, an agent) occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    /// Two rows: name + subtitle. The original layout.
    Comfortable,
    /// One row: name with its subtitle trailing, right-aligned. Doubles how many entries
    /// fit, which past a certain count beats reading the subtitle of each.
    Compact,
    /// Comfortable per half, dropping to compact only for a half that would overflow —
    /// so a workspace pays the density cost exactly where it has too much to show.
    Auto,
}

impl Density {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "comfortable" | "comfy" | "normal" => Density::Comfortable,
            "compact" | "dense" => Density::Compact,
            "auto" => Density::Auto,
            _ => return None,
        })
    }

    /// Rows per entry, given whether this half would overflow at the comfortable size.
    pub fn entry_rows(self, would_overflow: bool) -> u16 {
        match self {
            Density::Comfortable => 2,
            Density::Compact => 1,
            Density::Auto => {
                if would_overflow {
                    1
                } else {
                    2
                }
            }
        }
    }
}

impl SortBy {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "created" | "creation" => SortBy::Created,
            "alphabetical" | "alpha" | "name" => SortBy::Alphabetical,
            "recent" | "mru" => SortBy::Recent,
            "activity" | "active" => SortBy::Activity,
            _ => return None,
        })
    }
}

/// How the status-bar usage/limits readout is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageStyle {
    /// Hidden entirely (equivalent to `COPAD_MUX_USAGE=0`).
    Off,
    /// Percentages only: `claude 5h 5% wk 34% · codex wk 60%`.
    Text,
    /// A progress bar per window on wide terminals (`5h ━━━╌╌╌╌╌ 34%`),
    /// falling back to `Text` when the terminal is too narrow for the bars.
    Bar,
}

impl UsageStyle {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "hidden" => UsageStyle::Off,
            "text" | "percent" | "pct" => UsageStyle::Text,
            "bar" | "bars" | "progress" => UsageStyle::Bar,
            _ => return None,
        })
    }
}

/// Whether the usage/limits readout is a paged carousel or the legacy inline row.
/// `UsageStyle` above still selects the gauge (bar/text/off) WITHIN either layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLayout {
    /// One window (or provider) per page with a reset countdown, wheel-scrollable
    /// and click-paged. The default — the only layout that shows reset times.
    Paged,
    /// The historical single row with every window inline (no room for resets).
    Inline,
}

impl UsageLayout {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "paged" | "page" | "carousel" => UsageLayout::Paged,
            "inline" | "row" | "all" => UsageLayout::Inline,
            _ => return None,
        })
    }
}

/// Config-string → [`crate::usagepoll::PageUnit`] (carousel page granularity).
fn parse_page_unit(s: &str) -> Option<crate::usagepoll::PageUnit> {
    use crate::usagepoll::PageUnit;
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "window" | "windows" => PageUnit::Window,
        "provider" | "providers" => PageUnit::Provider,
        "metric" | "metrics" => PageUnit::Metric,
        _ => return None,
    })
}

/// Config-string → [`crate::usagepoll::ResetStyle`] (how the reset time is shown).
fn parse_reset_style(s: &str) -> Option<crate::usagepoll::ResetStyle> {
    use crate::usagepoll::ResetStyle;
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "relative" | "rel" | "countdown" => ResetStyle::Relative,
        "absolute" | "abs" | "clock" => ResetStyle::Absolute,
        "off" | "none" | "hidden" => ResetStyle::Off,
        _ => return None,
    })
}

/// What each status-bar tab chip shows. Zero-config default is [`TabLabels::Number`]
/// (identical to the historical `1`/`2`/`3` chips); the other styles surface the tab's
/// focused-pane foreground command (e.g. `claude` / `nvim`) so tabs read at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabLabels {
    /// Just the 1-based index: ` 1 ` (the original behavior).
    Number,
    /// Just the process name: ` claude ` (index dropped).
    Name,
    /// Index + process name: ` 1:claude ` (keeps the `Ctrl-b <n>` mapping visible).
    Both,
}

impl TabLabels {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "number" | "index" | "num" => TabLabels::Number,
            "name" | "process" | "command" | "cmd" => TabLabels::Name,
            "both" | "number-name" | "index-name" => TabLabels::Both,
            _ => return None,
        })
    }
}

/// Every user-bindable action (each current binding, plus `KillSession`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    SplitRight,
    SplitDown,
    NewTab,
    NextTab,
    PrevTab,
    CloseTab,
    /// Jump to tab index `0..=8` (`Ctrl-b 1`..`9`, `Alt-1`..`9`).
    SelectTab(u8),
    /// `Ctrl-b ,` (tmux rename-window): give the active tab a custom name that takes
    /// display precedence over its foreground-process label (empty → clear).
    RenameTab,
    NewSession,
    /// `Ctrl-b W`: create a git worktree + a session in it (name prompt).
    NewWorktree,
    RenameSession,
    NextSession,
    PrevSession,
    KillSession,
    NotificationCenter,
    JumpAttention,
    Detach,
    ClosePane,
    ToggleSidebar,
    Scrollback,
    FocusNext,
    FocusLeft,
    FocusDown,
    FocusUp,
    FocusRight,
    ResizeLeft,
    ResizeDown,
    ResizeUp,
    ResizeRight,
    Popup,
    /// Force a full client repaint (`Ctrl-b r`, tmux `refresh-client`). The server re-sends
    /// a `full` frame and the client clears its terminal, wiping any drift/ghosting left by a
    /// resize, an alt-screen transition, or a nested emulator that lost a cell.
    Redraw,
    /// Focus the always-on left sidebar for keyboard navigation (nvim-explorer-style).
    FocusSidebar,
    /// Arm the prefix (`Ctrl-b`). A global-table action like any other, but prefix
    /// entry always wins over a colliding user binding.
    EnterPrefix,
}

/// A canonical key on the pane keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Left,
    Right,
    Up,
    Down,
    Enter,
    Tab,
    Space,
    Esc,
    Backspace,
}

/// A fully-canonicalized chord: modifier bits + one key. Two chords are equal iff a
/// live `KeyEvent` and a config token canonicalize to the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: Key,
}

impl Chord {
    /// A short label for showing the chord to the user: `C-b` → `^b`, `M-1` → `M-1`,
    /// `C-S-Left` → `^S-Left`. Terminal caret convention rather than the config
    /// spelling, because this is read at a glance in the status bar — and the prefix
    /// indicator there must show the ACTUAL prefix, which `prefix = "C-a"` in
    /// `mux.toml` can change.
    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.alt {
            s.push_str("M-");
        }
        if self.ctrl {
            s.push('^');
        }
        match self.key {
            // An alphabetic key folds its shift into the letter's case (that is how
            // `chord_of`/`parse_chord` canonicalize it), so `S-` would be redundant.
            Key::Char(c) if c.is_ascii_alphabetic() => {
                if self.shift {
                    s.push(c.to_ascii_uppercase());
                } else {
                    s.push(c);
                }
            }
            Key::Char(c) => s.push(c),
            named => {
                if self.shift {
                    s.push_str("S-");
                }
                s.push_str(match named {
                    Key::Left => "Left",
                    Key::Right => "Right",
                    Key::Up => "Up",
                    Key::Down => "Down",
                    Key::Enter => "Enter",
                    Key::Tab => "Tab",
                    Key::Space => "Space",
                    Key::Esc => "Esc",
                    Key::Backspace => "BSpace",
                    Key::Char(_) => unreachable!("handled above"),
                });
            }
        }
        s
    }
}

/// Which table a binding lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// Pressed AFTER the prefix (`Ctrl-b %`).
    Prefix,
    /// Prefix-less (tmux `bind -n`): `Alt-1`, `Ctrl+Shift+h`, `Ctrl-f`.
    Global,
}

/// Canonicalize a live key event into a [`Chord`], or `None` for keys we never bind
/// (function keys, etc.). Applied identically to config tokens by [`parse_chord`].
pub fn chord_of(k: &KeyEvent) -> Option<Chord> {
    let mods = k.modifiers;
    let mut ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    let mut shift = mods.contains(KeyModifiers::SHIFT);
    let key = match k.code {
        KeyCode::Char(c) => {
            let cp = c as u32;
            // Raw legacy control byte (e.g. `\u{2}` for Ctrl-b) with no CONTROL flag —
            // fold to ctrl+letter. Skip the ones that have their own KeyCode
            // (BS 8, Tab 9, LF 10, CR 13) so they aren't misread as Ctrl-h/i/j/m.
            if !ctrl && (1..=26).contains(&cp) && !matches!(cp, 8 | 9 | 10 | 13) {
                ctrl = true;
                Key::Char((b'a' - 1 + cp as u8) as char)
            } else if c == ' ' {
                // Canonicalize to Key::Space so a config `"Space"` binding matches.
                Key::Space
            } else if c.is_ascii_alphabetic() {
                if c.is_ascii_uppercase() {
                    shift = true;
                }
                Key::Char(c.to_ascii_lowercase())
            } else {
                Key::Char(c)
            }
        }
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        _ => return None,
    };
    Some(normalize(Chord {
        ctrl,
        alt,
        shift,
        key,
    }))
}

/// Drop a `SHIFT` that carries no information: for punctuation/symbol keys the shifted
/// form IS the distinct glyph (`%` is Shift+5), and some terminals report `Char('%')`
/// WITH a SHIFT modifier while others omit it. Zeroing it here — for both a live event
/// and a parsed config token — makes the two spellings compare equal (letters already
/// fold case into their lowercased char; only alphabetic chars and named keys keep
/// `SHIFT`, e.g. `C-S-Left`).
fn normalize(mut c: Chord) -> Chord {
    let keeps_shift = match c.key {
        Key::Char(ch) => ch.is_ascii_alphabetic(),
        Key::Space => false,
        _ => true, // arrows / Enter / Tab / Esc / Backspace
    };
    if !keeps_shift {
        c.shift = false;
    }
    c
}

/// Parse a config chord string (`"C-b"`, `"M-1"`, `"C-S-h"`, `"%"`, `"Left"`,
/// `"Enter"`) into a canonical [`Chord`]. Case-insensitive modifiers `C`/`M`/`S`.
pub fn parse_chord(s: &str) -> Result<Chord, String> {
    if s.is_empty() {
        return Err("empty chord".to_string());
    }
    // A single character is the key itself (so `-` and `%` parse as keys, not seps).
    let tokens: Vec<&str> = if s.chars().count() == 1 {
        vec![s]
    } else {
        s.split('-').collect()
    };
    let (mod_toks, key_tok) = tokens.split_at(tokens.len() - 1);
    let key_tok = key_tok[0];
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    for m in mod_toks {
        match *m {
            "C" | "c" => ctrl = true,
            "M" | "m" => alt = true,
            "S" | "s" => shift = true,
            "" => {} // stray separator
            other => return Err(format!("unknown modifier '{other}' in '{s}'")),
        }
    }
    let key = parse_key(key_tok, &mut shift).ok_or_else(|| format!("unknown key in '{s}'"))?;
    Ok(normalize(Chord {
        ctrl,
        alt,
        shift,
        key,
    }))
}

fn parse_key(tok: &str, shift: &mut bool) -> Option<Key> {
    match tok {
        "Left" | "left" => Some(Key::Left),
        "Right" | "right" => Some(Key::Right),
        "Up" | "up" => Some(Key::Up),
        "Down" | "down" => Some(Key::Down),
        "Enter" | "enter" | "CR" | "Return" => Some(Key::Enter),
        "Space" | "space" => Some(Key::Space),
        "Tab" | "tab" => Some(Key::Tab),
        "Esc" | "esc" | "Escape" => Some(Key::Esc),
        "BSpace" | "bspace" | "Backspace" | "backspace" => Some(Key::Backspace),
        _ if tok.chars().count() == 1 => {
            let c = tok.chars().next().unwrap();
            if c.is_ascii_alphabetic() {
                if c.is_ascii_uppercase() {
                    *shift = true;
                }
                Some(Key::Char(c.to_ascii_lowercase()))
            } else {
                Some(Key::Char(c))
            }
        }
        _ => None,
    }
}

/// Map a config action name (`"next-tab"`, `"tab-1"`) to an [`Action`].
fn action_from_name(name: &str) -> Option<Action> {
    Some(match name {
        "split-right" => Action::SplitRight,
        "split-down" => Action::SplitDown,
        "new-tab" => Action::NewTab,
        "next-tab" => Action::NextTab,
        "prev-tab" => Action::PrevTab,
        "close-tab" => Action::CloseTab,
        "rename-tab" => Action::RenameTab,
        "new-session" => Action::NewSession,
        "new-worktree" => Action::NewWorktree,
        "rename-session" => Action::RenameSession,
        "next-session" => Action::NextSession,
        "prev-session" => Action::PrevSession,
        "kill-session" => Action::KillSession,
        "notification-center" => Action::NotificationCenter,
        "jump-attention" => Action::JumpAttention,
        "detach" => Action::Detach,
        "close-pane" => Action::ClosePane,
        "toggle-sidebar" => Action::ToggleSidebar,
        "scrollback" => Action::Scrollback,
        "focus-next" => Action::FocusNext,
        "focus-left" => Action::FocusLeft,
        "focus-down" => Action::FocusDown,
        "focus-up" => Action::FocusUp,
        "focus-right" => Action::FocusRight,
        "resize-left" => Action::ResizeLeft,
        "resize-down" => Action::ResizeDown,
        "resize-up" => Action::ResizeUp,
        "resize-right" => Action::ResizeRight,
        "popup" => Action::Popup,
        "redraw" => Action::Redraw,
        "focus-sidebar" => Action::FocusSidebar,
        "prefix" => Action::EnterPrefix,
        _ => {
            // tab-1 .. tab-9
            let n = name.strip_prefix("tab-")?.parse::<u8>().ok()?;
            if (1..=9).contains(&n) {
                Action::SelectTab(n - 1)
            } else {
                return None;
            }
        }
    })
}

/// The built-in bindings in DECLARATION ORDER. Order is the collision priority: on a
/// duplicate chord, the earlier entry wins (deterministic). `(action, context, chords)`.
fn default_bindings() -> Vec<(Action, Ctx, &'static [&'static str])> {
    use Action::*;
    use Ctx::*;
    vec![
        // ---- global (prefix-less) ----
        (EnterPrefix, Global, &["C-b"]),
        (Popup, Global, &["C-f"]),
        (SelectTab(0), Global, &["M-1"]),
        (SelectTab(1), Global, &["M-2"]),
        (SelectTab(2), Global, &["M-3"]),
        (SelectTab(3), Global, &["M-4"]),
        (SelectTab(4), Global, &["M-5"]),
        (SelectTab(5), Global, &["M-6"]),
        (SelectTab(6), Global, &["M-7"]),
        (SelectTab(7), Global, &["M-8"]),
        (SelectTab(8), Global, &["M-9"]),
        (FocusLeft, Global, &["C-S-h", "C-S-Left"]),
        (FocusDown, Global, &["C-S-j", "C-S-Down"]),
        (FocusUp, Global, &["C-S-k", "C-S-Up"]),
        (FocusRight, Global, &["C-S-l", "C-S-Right"]),
        // ---- prefix table ----
        (SplitRight, Prefix, &["%"]),
        (SplitDown, Prefix, &["\""]),
        (NotificationCenter, Prefix, &["a"]),
        (Scrollback, Prefix, &["["]),
        (FocusNext, Prefix, &["o"]),
        (ClosePane, Prefix, &["x"]),
        (ToggleSidebar, Prefix, &["s"]),
        (Redraw, Prefix, &["r"]),
        (FocusSidebar, Prefix, &["e"]),
        (NewTab, Prefix, &["c"]),
        (NextTab, Prefix, &["n"]),
        (PrevTab, Prefix, &["p"]),
        (CloseTab, Prefix, &["&"]),
        (RenameTab, Prefix, &[","]),
        (SelectTab(0), Prefix, &["1"]),
        (SelectTab(1), Prefix, &["2"]),
        (SelectTab(2), Prefix, &["3"]),
        (SelectTab(3), Prefix, &["4"]),
        (SelectTab(4), Prefix, &["5"]),
        (SelectTab(5), Prefix, &["6"]),
        (SelectTab(6), Prefix, &["7"]),
        (SelectTab(7), Prefix, &["8"]),
        (SelectTab(8), Prefix, &["9"]),
        (NewSession, Prefix, &["C"]),
        (NewWorktree, Prefix, &["W"]),
        (RenameSession, Prefix, &["$"]),
        (KillSession, Prefix, &["X"]),
        (NextSession, Prefix, &[")"]),
        (PrevSession, Prefix, &["("]),
        (JumpAttention, Prefix, &["!"]),
        (Detach, Prefix, &["d", "q"]),
        (FocusLeft, Prefix, &["h", "Left"]),
        (FocusDown, Prefix, &["j", "Down"]),
        (FocusUp, Prefix, &["k", "Up"]),
        (FocusRight, Prefix, &["l", "Right"]),
        (ResizeLeft, Prefix, &["H"]),
        (ResizeDown, Prefix, &["J"]),
        (ResizeUp, Prefix, &["K"]),
        (ResizeRight, Prefix, &["L"]),
    ]
}

/// The resolved keymap: two chord→action lookup tables plus the prefix chord.
#[derive(Debug, Clone)]
pub struct Keymap {
    pub prefix_chord: Chord,
    prefix_map: HashMap<Chord, Action>,
    global_map: HashMap<Chord, Action>,
}

impl Keymap {
    /// Resolve a prefix-table chord (after the prefix was armed).
    pub fn prefix_action(&self, chord: &Chord) -> Option<Action> {
        self.prefix_map.get(chord).copied()
    }
    /// Resolve a prefix-less chord.
    pub fn global_action(&self, chord: &Chord) -> Option<Action> {
        self.global_map.get(chord).copied()
    }
}

/// `[worktree]` configuration for `comux worktree create` (mirrors `tmx`'s
/// `[worktree]`): the directory-naming pattern and per-repo post-create hooks.
#[derive(Debug, Clone)]
pub struct WorktreeConfig {
    /// Directory naming pattern; tokens `{repo}` / `{branch}` (default
    /// `{repo}-{branch}`). See [`crate::worktree::render_naming`].
    pub naming: String,
    /// Per-repo post-create hook: canonical main-worktree path → shell command run via
    /// `bash -c` (cwd = new worktree, `WORKTREE_PATH` exported). Keys are `~`-expanded
    /// then canonicalized so a linked-worktree caller still matches.
    pub scripts: HashMap<PathBuf, String>,
}

impl WorktreeConfig {
    /// The post-create hook for `main_root`, if any (canonical-path keyed).
    pub fn script_for(&self, main_root: &std::path::Path) -> Option<&str> {
        let key = crate::worktree::canonical_or_lexical(main_root);
        self.scripts.get(&key).map(|s| s.as_str())
    }
}

/// Effective configuration.
#[derive(Debug, Clone)]
pub struct MuxConfig {
    pub keymap: Keymap,
    pub mouse: bool,
    /// Relay a pane program's OSC 52 clipboard write out to the attached clients (tmux
    /// `set-clipboard`). On by default, matching tmux — comux hosts your own shells, and
    /// without it nothing inside a pane (nvim's osc52 provider, a `yank` helper) can reach
    /// the clipboard. Set `false` to keep a background pane from clobbering it. Clipboard
    /// READS are never answered regardless (see `term.rs`).
    pub osc52: bool,
    pub notify: bool,
    pub sidebar: bool,
    pub sidebar_width: u16,
    pub sidebar_min_cols: u16,
    pub scroll_step: i32,
    /// Restore the saved session layout on server start (continuum-style autorestore).
    pub persist: bool,
    /// Periodic autosave interval in seconds; `0` disables periodic saves.
    pub autosave_secs: u32,
    /// Process basenames whose running command is saved and RE-RUN on restore (tmux
    /// -resurrect's process whitelist). Default = the built-in AI agents; an empty list
    /// disables program re-execution (panes restore as bare shells).
    pub restore_processes: Vec<String>,
    /// When re-running a restored agent (`restore_processes`), resume its live conversation
    /// instead of starting fresh — `claude --resume <id>` / `codex resume <id>`, using the
    /// session the process was actually in. Default on; set false to always restart cleanly.
    pub restore_agent_sessions: bool,
    /// Session ordering in the sidebar / switcher / cycle.
    pub sort_by: SortBy,
    /// Rows per sidebar entry (`comfortable` | `compact` | `auto`).
    pub sidebar_density: Density,
    /// How the status-bar usage/limits readout is rendered (off/text/bar).
    pub usage: UsageStyle,
    /// Which rate-limit windows the readout shows (config `usage_windows`); a window
    /// renders only if both available and enabled. Default = all (Claude 5h + weekly,
    /// Codex weekly).
    pub usage_windows: crate::usagepoll::UsageWindows,
    /// Width in cells of each progress bar when `usage = "bar"`.
    pub usage_bar_width: u16,
    /// Paged carousel (default) vs the legacy inline row for the usage readout.
    pub usage_layout: UsageLayout,
    /// Carousel page granularity: one window per page (default) or one provider.
    pub usage_page_unit: crate::usagepoll::PageUnit,
    /// How the carousel shows a window's reset time (relative / absolute / off).
    pub usage_reset: crate::usagepoll::ResetStyle,
    /// Auto-advance the carousel every N seconds (0 = off, manual paging only). A
    /// manual wheel/click resets the timer so it doesn't jump right after you page.
    pub usage_rotate_secs: u32,
    /// What each status-bar tab chip shows (number / process name / both).
    pub tab_labels: TabLabels,
    /// Check GitHub releases in the background and show a `⬆ x.y.z` hint in the
    /// status bar when a newer version exists (equivalent to
    /// `COPAD_MUX_UPDATE_CHECK=0` when false).
    pub update_check: bool,
    /// Environment variables refreshed from the attaching client into new panes
    /// (tmux `update-environment`). Scrubbed from the daemon at startup so a pane
    /// never inherits the session the server was born in; re-injected per-pane from
    /// the latest client. Empty list = no refresh (panes inherit the daemon env as-is).
    pub update_environment: Vec<String>,
    /// `comux worktree create` naming + post-create hooks.
    pub worktree: WorktreeConfig,
}

#[derive(Deserialize, Default)]
struct RawConfig {
    prefix: Option<String>,
    mouse: Option<bool>,
    osc52: Option<bool>,
    notify: Option<bool>,
    sidebar: Option<bool>,
    sidebar_width: Option<i64>,
    sidebar_min_cols: Option<i64>,
    scroll_step: Option<i64>,
    persist: Option<bool>,
    autosave_secs: Option<i64>,
    restore_processes: Option<Vec<String>>,
    restore_agent_sessions: Option<bool>,
    sort_by: Option<String>,
    sidebar_density: Option<String>,
    usage: Option<String>,
    usage_windows: Option<Vec<String>>,
    usage_bar_width: Option<i64>,
    usage_layout: Option<String>,
    usage_page_unit: Option<String>,
    usage_reset: Option<String>,
    usage_rotate_secs: Option<i64>,
    tab_labels: Option<String>,
    update_check: Option<bool>,
    update_environment: Option<Vec<String>>,
    keys: Option<HashMap<String, ChordSpec>>,
    global: Option<HashMap<String, ChordSpec>>,
    worktree: Option<RawWorktree>,
}

#[derive(Deserialize, Default)]
struct RawWorktree {
    naming: Option<String>,
    scripts: Option<HashMap<String, String>>,
}

/// A binding value: one chord or a list of chords.
#[derive(Deserialize)]
#[serde(untagged)]
enum ChordSpec {
    One(String),
    Many(Vec<String>),
}

impl ChordSpec {
    fn strings(&self) -> Vec<&str> {
        match self {
            ChordSpec::One(s) => vec![s.as_str()],
            ChordSpec::Many(v) => v.iter().map(|s| s.as_str()).collect(),
        }
    }
}

impl MuxConfig {
    /// The config path: `$XDG_CONFIG_HOME/copad/mux.toml`, else macOS `~/.config`,
    /// else the platform config dir.
    pub fn config_path() -> PathBuf {
        config_dir().join("copad").join("mux.toml")
    }

    /// Load the effective config, returning any warnings (bad bindings, out-of-range
    /// numbers, collisions). A missing file yields the default config and no warnings.
    pub fn load() -> (MuxConfig, Vec<String>) {
        Self::load_from(&Self::config_path())
    }

    pub fn load_from(path: &std::path::Path) -> (MuxConfig, Vec<String>) {
        if !path.exists() {
            return (Self::default(), Vec::new());
        }
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                return (
                    Self::default(),
                    vec![format!("{}: {e} — using defaults", path.display())],
                );
            }
        };
        let raw: RawConfig = match toml::from_str(&contents) {
            Ok(r) => r,
            Err(e) => {
                return (
                    Self::default(),
                    vec![format!("{}: {e} — using defaults", path.display())],
                );
            }
        };
        Self::from_raw(raw)
    }

    fn default() -> MuxConfig {
        let (keymap, _warn) = build_keymap(&HashMap::new(), &HashMap::new());
        MuxConfig {
            keymap,
            mouse: true,
            osc52: true,
            notify: true,
            sidebar: true,
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_min_cols: DEFAULT_SIDEBAR_MIN_COLS,
            scroll_step: DEFAULT_SCROLL_STEP,
            persist: true,
            autosave_secs: DEFAULT_AUTOSAVE_SECS,
            restore_processes: default_restore_processes(),
            restore_agent_sessions: true,
            sort_by: SortBy::Created,
            sidebar_density: Density::Comfortable,
            usage: UsageStyle::Bar,
            usage_windows: crate::usagepoll::UsageWindows::all(),
            usage_bar_width: DEFAULT_USAGE_BAR_WIDTH,
            usage_layout: UsageLayout::Paged,
            usage_page_unit: crate::usagepoll::PageUnit::Window,
            usage_reset: crate::usagepoll::ResetStyle::Relative,
            usage_rotate_secs: 0,
            tab_labels: TabLabels::Number,
            update_check: true,
            update_environment: default_update_environment(),
            worktree: WorktreeConfig {
                naming: crate::worktree::DEFAULT_NAMING.to_string(),
                scripts: HashMap::new(),
            },
        }
    }

    fn from_raw(raw: RawConfig) -> (MuxConfig, Vec<String>) {
        let mut warnings = Vec::new();

        // Key overrides: action name → chords (bad names/chords warn + skip).
        let prefix_over = collect_overrides(raw.keys.as_ref(), &mut warnings, "keys");
        let mut global_over = collect_overrides(raw.global.as_ref(), &mut warnings, "global");
        // A custom prefix key is just the EnterPrefix global binding.
        if let Some(p) = raw.prefix.as_deref() {
            match parse_chord(p) {
                Ok(c) => {
                    global_over.insert(Action::EnterPrefix, vec![c]);
                }
                Err(e) => warnings.push(format!("prefix: {e} — keeping C-b")),
            }
        }

        let (keymap, kw) = build_keymap(&prefix_over, &global_over);
        warnings.extend(kw);

        // Numeric fields with per-field clamp + warn.
        let sidebar_width = clamp_field(
            raw.sidebar_width,
            DEFAULT_SIDEBAR_WIDTH as i64,
            8,
            80,
            "sidebar_width",
            &mut warnings,
        ) as u16;
        let mut sidebar_min_cols = clamp_field(
            raw.sidebar_min_cols,
            DEFAULT_SIDEBAR_MIN_COLS as i64,
            40,
            400,
            "sidebar_min_cols",
            &mut warnings,
        ) as u16;
        // Relational: a visible sidebar must leave room for content [codex C3].
        let floor = sidebar_width + MIN_CONTENT_COLS;
        if sidebar_min_cols < floor {
            warnings.push(format!(
                "sidebar_min_cols ({sidebar_min_cols}) < sidebar_width+{MIN_CONTENT_COLS} \
                 ({floor}) — raised to {floor}"
            ));
            sidebar_min_cols = floor;
        }
        let scroll_step = clamp_field(
            raw.scroll_step,
            DEFAULT_SCROLL_STEP as i64,
            1,
            50,
            "scroll_step",
            &mut warnings,
        ) as i32;
        // autosave: 0 explicitly disables periodic saves; any other value is clamped to
        // a sane [5, 3600] s (a bad-but-nonzero value shouldn't hammer the disk).
        let autosave_secs = match raw.autosave_secs {
            None => DEFAULT_AUTOSAVE_SECS,
            Some(0) => 0,
            Some(v) if !(5..=3600).contains(&v) => {
                let c = v.clamp(5, 3600);
                warnings.push(format!(
                    "autosave_secs ({v}) out of range [5,3600] (or 0 to disable) — clamped to {c}"
                ));
                c as u32
            }
            Some(v) => v as u32,
        };

        let worktree = build_worktree(raw.worktree, &mut warnings);
        let update_environment = build_update_environment(raw.update_environment, &mut warnings);

        (
            MuxConfig {
                keymap,
                mouse: raw.mouse.unwrap_or(true),
                osc52: raw.osc52.unwrap_or(true),
                notify: raw.notify.unwrap_or(true),
                sidebar: raw.sidebar.unwrap_or(true),
                sidebar_width,
                sidebar_min_cols,
                scroll_step,
                persist: raw.persist.unwrap_or(true),
                autosave_secs,
                restore_processes: raw
                    .restore_processes
                    .unwrap_or_else(default_restore_processes),
                restore_agent_sessions: raw.restore_agent_sessions.unwrap_or(true),
                sidebar_density: match raw.sidebar_density.as_deref() {
                    None => Density::Comfortable,
                    Some(s) => Density::parse(s).unwrap_or_else(|| {
                        warnings.push(format!(
                            "sidebar_density '{s}' unknown (comfortable|compact|auto) — \
                             using comfortable"
                        ));
                        Density::Comfortable
                    }),
                },
                sort_by: match raw.sort_by.as_deref() {
                    None => SortBy::Created,
                    Some(s) => SortBy::parse(s).unwrap_or_else(|| {
                        warnings.push(format!(
                            "sort_by '{s}' unknown (created|alphabetical|recent|activity) — \
                             using created"
                        ));
                        SortBy::Created
                    }),
                },
                usage: match raw.usage.as_deref() {
                    None => UsageStyle::Bar,
                    Some(s) => UsageStyle::parse(s).unwrap_or_else(|| {
                        warnings.push(format!("usage '{s}' unknown (off|text|bar) — using bar"));
                        UsageStyle::Bar
                    }),
                },
                usage_windows: build_usage_windows(raw.usage_windows, &mut warnings),
                usage_bar_width: clamp_field(
                    raw.usage_bar_width,
                    DEFAULT_USAGE_BAR_WIDTH as i64,
                    3,
                    30,
                    "usage_bar_width",
                    &mut warnings,
                ) as u16,
                usage_layout: match raw.usage_layout.as_deref() {
                    None => UsageLayout::Paged,
                    Some(s) => UsageLayout::parse(s).unwrap_or_else(|| {
                        warnings.push(format!(
                            "usage_layout '{s}' unknown (paged|inline) — using paged"
                        ));
                        UsageLayout::Paged
                    }),
                },
                usage_page_unit: match raw.usage_page_unit.as_deref() {
                    None => crate::usagepoll::PageUnit::Window,
                    Some(s) => parse_page_unit(s).unwrap_or_else(|| {
                        warnings.push(format!(
                            "usage_page_unit '{s}' unknown (window|provider|metric) — using window"
                        ));
                        crate::usagepoll::PageUnit::Window
                    }),
                },
                usage_reset: match raw.usage_reset.as_deref() {
                    None => crate::usagepoll::ResetStyle::Relative,
                    Some(s) => parse_reset_style(s).unwrap_or_else(|| {
                        warnings.push(format!(
                            "usage_reset '{s}' unknown (relative|absolute|off) — using relative"
                        ));
                        crate::usagepoll::ResetStyle::Relative
                    }),
                },
                usage_rotate_secs: clamp_field(
                    raw.usage_rotate_secs,
                    0,
                    0,
                    3600,
                    "usage_rotate_secs",
                    &mut warnings,
                ) as u32,
                tab_labels: match raw.tab_labels.as_deref() {
                    None => TabLabels::Number,
                    Some(s) => TabLabels::parse(s).unwrap_or_else(|| {
                        warnings.push(format!(
                            "tab_labels '{s}' unknown (number|name|both) — using number"
                        ));
                        TabLabels::Number
                    }),
                },
                update_check: raw.update_check.unwrap_or(true),
                update_environment,
                worktree,
            },
            warnings,
        )
    }
}

/// Resolve `usage_windows`: default (all windows) when absent, else the configured
/// list of window ids (`claude-5h` / `claude-wk` / `codex-wk`) turned into an enabled
/// set. Unknown ids warn and are ignored; an explicit empty list hides every window
/// (the readout disappears, like `usage = "off"` but per-window). Aliases: `claude`
/// enables both Claude windows, `codex` the Codex weekly one.
fn build_usage_windows(
    raw: Option<Vec<String>>,
    warnings: &mut Vec<String>,
) -> crate::usagepoll::UsageWindows {
    use crate::usagepoll::UsageWindows;
    let Some(list) = raw else {
        return UsageWindows::all();
    };
    let mut sel = UsageWindows::none();
    for name in list {
        match name.trim().to_ascii_lowercase().as_str() {
            "" => {}
            "claude-5h" | "claude_5h" | "claude5h" => sel.claude_5h = true,
            "claude-wk" | "claude_wk" | "claude-week" | "claude-weekly" => sel.claude_wk = true,
            "codex-wk" | "codex_wk" | "codex-week" | "codex-weekly" => sel.codex_wk = true,
            "claude" => {
                sel.claude_5h = true;
                sel.claude_wk = true;
            }
            "codex" => sel.codex_wk = true,
            other => warnings.push(format!(
                "usage_windows: '{other}' unknown \
                 (claude-5h|claude-wk|codex-wk, or claude|codex) — ignored"
            )),
        }
    }
    sel
}

/// Resolve `update_environment`: default when absent, else the configured list with
/// load-bearing names ([`ENV_UPDATE_BLOCKLIST`]) dropped + warned and duplicates removed
/// (order preserved). An explicit empty list is honored (disables env refresh).
fn build_update_environment(raw: Option<Vec<String>>, warnings: &mut Vec<String>) -> Vec<String> {
    let Some(list) = raw else {
        return default_update_environment();
    };
    let mut out: Vec<String> = Vec::with_capacity(list.len());
    for name in list {
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        // A name with `=` or a NUL byte would panic `std::env::remove_var` when the server
        // scrubs it at startup — reject it here so a config typo can never crash the daemon.
        if name.contains('=') || name.contains('\0') {
            warnings.push(format!(
                "update_environment: '{name}' is not a valid variable name — ignoring"
            ));
            continue;
        }
        if ENV_UPDATE_BLOCKLIST
            .iter()
            .any(|b| b.eq_ignore_ascii_case(&name))
        {
            warnings.push(format!(
                "update_environment: '{name}' is load-bearing and cannot be refreshed — ignoring"
            ));
            continue;
        }
        if !out.iter().any(|e| e == &name) {
            out.push(name);
        }
    }
    out
}

/// Build the `[worktree]` config: naming (empty → default) and per-repo hooks whose
/// path keys are `~`-expanded then canonicalized (so a linked-worktree caller matches
/// the same repo hook). Duplicate canonical keys are last-wins with a warning.
fn build_worktree(raw: Option<RawWorktree>, warnings: &mut Vec<String>) -> WorktreeConfig {
    let raw = raw.unwrap_or_default();
    let naming = match raw.naming {
        Some(n) if !n.trim().is_empty() => n,
        _ => crate::worktree::DEFAULT_NAMING.to_string(),
    };
    let mut scripts: HashMap<PathBuf, String> = HashMap::new();
    for (k, v) in raw.scripts.unwrap_or_default() {
        let key = crate::worktree::canonical_or_lexical(&expand_tilde(&k));
        if scripts.insert(key.clone(), v).is_some() {
            warnings.push(format!(
                "worktree.scripts: duplicate repo key resolves to {} — last value wins",
                key.display()
            ));
        }
    }
    WorktreeConfig { naming, scripts }
}

/// Expand a leading `~` / `~/` to `$HOME` (config keys are written with `~`).
fn expand_tilde(p: &str) -> PathBuf {
    if (p == "~" || p.starts_with("~/"))
        && let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join(p.trim_start_matches('~').trim_start_matches('/'));
    }
    PathBuf::from(p)
}

fn collect_overrides(
    table: Option<&HashMap<String, ChordSpec>>,
    warnings: &mut Vec<String>,
    ctx: &str,
) -> HashMap<Action, Vec<Chord>> {
    let mut out = HashMap::new();
    let Some(table) = table else {
        return out;
    };
    for (name, spec) in table {
        let Some(action) = action_from_name(name) else {
            warnings.push(format!("[{ctx}] unknown action '{name}' — ignored"));
            continue;
        };
        let mut chords = Vec::new();
        for s in spec.strings() {
            match parse_chord(s) {
                Ok(c) => chords.push(c),
                Err(e) => warnings.push(format!("[{ctx}] {name}: {e} — ignored")),
            }
        }
        if !chords.is_empty() {
            out.insert(action, chords);
        }
    }
    out
}

fn clamp_field(
    val: Option<i64>,
    default: i64,
    lo: i64,
    hi: i64,
    name: &str,
    warnings: &mut Vec<String>,
) -> i64 {
    match val {
        None => default,
        Some(v) if v < lo || v > hi => {
            let c = v.clamp(lo, hi);
            warnings.push(format!(
                "{name} ({v}) out of range [{lo},{hi}] — clamped to {c}"
            ));
            c
        }
        Some(v) => v,
    }
}

/// Build the two lookup tables from defaults overlaid with per-action overrides.
///
/// Priority (deterministic): a USER override beats a DEFAULT, and within each tier
/// declaration order breaks ties (first to claim a chord wins; later duplicates warn).
/// So rebinding `focus-right = "n"` steals `n` from the default `next-tab`, not the
/// other way round. A global binding left on the prefix chord is then reclaimed for
/// prefix entry so the prefix always works.
fn build_keymap(
    prefix_over: &HashMap<Action, Vec<Chord>>,
    global_over: &HashMap<Action, Vec<Chord>>,
) -> (Keymap, Vec<String>) {
    let mut warnings = Vec::new();
    let mut prefix_map: HashMap<Chord, Action> = HashMap::new();
    let mut global_map: HashMap<Chord, Action> = HashMap::new();

    let defaults = default_bindings();
    // Two passes over the SAME declaration order: user-overridden actions first (so they
    // win chord collisions against defaults), then the rest at their default chords.
    for user_pass in [true, false] {
        for &(action, ctx, default_chords) in &defaults {
            let (over, map, ctx_name) = match ctx {
                Ctx::Prefix => (prefix_over, &mut prefix_map, "keys"),
                Ctx::Global => (global_over, &mut global_map, "global"),
            };
            let overridden = over.contains_key(&action);
            if overridden != user_pass {
                continue; // handled in the other pass
            }
            let chords: Vec<Chord> = match over.get(&action) {
                Some(v) => v.clone(),
                None => default_chords
                    .iter()
                    .map(|s| parse_chord(s).expect("built-in default chord must parse"))
                    .collect(),
            };
            for c in chords {
                if let Some(existing) = map.get(&c) {
                    if *existing != action {
                        warnings.push(format!(
                            "[{ctx_name}] chord {c:?} bound to multiple actions \
                             ({existing:?} kept, {action:?} ignored)"
                        ));
                    }
                    continue;
                }
                map.insert(c, action);
            }
        }
    }

    // Determine the prefix chord (EnterPrefix's global binding) and protect it.
    let prefix_chord = global_map
        .iter()
        .find(|(_, a)| **a == Action::EnterPrefix)
        .map(|(c, _)| *c)
        .unwrap_or(Chord {
            ctrl: true,
            alt: false,
            shift: false,
            key: Key::Char('b'),
        });
    // Any OTHER global binding on the prefix chord would block prefix entry.
    if let Some(a) = global_map.get(&prefix_chord).copied()
        && a != Action::EnterPrefix
    {
        warnings.push(format!(
            "[global] {a:?} shadows the prefix {prefix_chord:?} — dropped so the prefix works"
        ));
        global_map.insert(prefix_chord, Action::EnterPrefix);
    }

    (
        Keymap {
            prefix_chord,
            prefix_map,
            global_map,
        },
        warnings,
    )
}

/// `$XDG_CONFIG_HOME`, else `$HOME/.config` (both macOS and Linux — matches copad's own
/// convention and keeps copad-mux free of a `dirs` dependency), else a relative fallback.
fn config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg);
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join(".config");
    }
    PathBuf::from(".config")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn parse_and_event_agree_on_letters_and_case() {
        // `H` (config) == Char('H') (live) == Char('h')+SHIFT.
        let cfg = parse_chord("H").unwrap();
        assert_eq!(
            chord_of(&ev(KeyCode::Char('H'), KeyModifiers::NONE)),
            Some(cfg)
        );
        assert_eq!(
            chord_of(&ev(KeyCode::Char('h'), KeyModifiers::SHIFT)),
            Some(cfg)
        );
    }

    #[test]
    fn ctrl_shift_letter_matches_both_terminal_spellings() {
        let cfg = parse_chord("C-S-h").unwrap();
        assert_eq!(
            chord_of(&ev(KeyCode::Char('H'), KeyModifiers::CONTROL)),
            Some(cfg)
        );
        assert_eq!(
            chord_of(&ev(
                KeyCode::Char('h'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            Some(cfg)
        );
    }

    #[test]
    fn raw_control_byte_folds_to_ctrl_letter() {
        // `\u{2}` with no modifier == `C-b`.
        let cb = parse_chord("C-b").unwrap();
        assert_eq!(
            chord_of(&ev(KeyCode::Char('\u{2}'), KeyModifiers::NONE)),
            Some(cb)
        );
        let cf = parse_chord("C-f").unwrap();
        assert_eq!(
            chord_of(&ev(KeyCode::Char('\u{6}'), KeyModifiers::NONE)),
            Some(cf)
        );
    }

    #[test]
    fn alt_digit_and_symbols() {
        assert_eq!(
            chord_of(&ev(KeyCode::Char('1'), KeyModifiers::ALT)),
            Some(parse_chord("M-1").unwrap())
        );
        // `%` is not shifted from the app's view — and a terminal that reports it WITH
        // a SHIFT modifier (enhanced keyboard protocol) still matches the config `%`.
        assert_eq!(
            chord_of(&ev(KeyCode::Char('%'), KeyModifiers::NONE)),
            Some(parse_chord("%").unwrap())
        );
        assert_eq!(
            chord_of(&ev(KeyCode::Char('%'), KeyModifiers::SHIFT)),
            Some(parse_chord("%").unwrap())
        );
        // Same for the other shifted-punctuation defaults.
        for s in ['"', '&', '$', '(', ')', '!'] {
            assert_eq!(
                chord_of(&ev(KeyCode::Char(s), KeyModifiers::SHIFT)),
                Some(parse_chord(&s.to_string()).unwrap()),
                "shifted punctuation {s:?} must match its unshifted config chord"
            );
        }
    }

    #[test]
    fn default_keymap_reproduces_core_bindings() {
        let cfg = MuxConfig::default();
        let km = &cfg.keymap;
        // prefix table
        assert_eq!(
            km.prefix_action(&parse_chord("%").unwrap()),
            Some(Action::SplitRight)
        );
        assert_eq!(
            km.prefix_action(&parse_chord("d").unwrap()),
            Some(Action::Detach)
        );
        assert_eq!(
            km.prefix_action(&parse_chord("q").unwrap()),
            Some(Action::Detach)
        );
        assert_eq!(
            km.prefix_action(&parse_chord("1").unwrap()),
            Some(Action::SelectTab(0))
        );
        assert_eq!(
            km.prefix_action(&parse_chord("Left").unwrap()),
            Some(Action::FocusLeft)
        );
        assert_eq!(
            km.prefix_action(&parse_chord("X").unwrap()),
            Some(Action::KillSession)
        );
        assert_eq!(
            km.prefix_action(&parse_chord(",").unwrap()),
            Some(Action::RenameTab)
        );
        // global table
        assert_eq!(
            km.global_action(&parse_chord("M-1").unwrap()),
            Some(Action::SelectTab(0))
        );
        assert_eq!(
            km.global_action(&parse_chord("C-f").unwrap()),
            Some(Action::Popup)
        );
        assert_eq!(km.prefix_chord, parse_chord("C-b").unwrap());
    }

    #[test]
    fn override_replaces_action_chord_set() {
        let toml = r#"
            [keys]
            next-tab = "l"
            detach = ["d", "e"]
        "#;
        let (cfg, warns) = load_str(toml);
        let km = &cfg.keymap;
        assert_eq!(
            km.prefix_action(&parse_chord("l").unwrap()),
            Some(Action::NextTab)
        );
        // `n` was next-tab's only default chord, replaced → now unbound.
        assert_eq!(km.prefix_action(&parse_chord("n").unwrap()), None);
        assert_eq!(
            km.prefix_action(&parse_chord("e").unwrap()),
            Some(Action::Detach)
        );
        // `q` was part of detach's DEFAULT set, replaced by [d,e] → q now unbound.
        assert_eq!(km.prefix_action(&parse_chord("q").unwrap()), None);
        // User's next-tab=l steals `l` from the default focus-right (user beats default),
        // which is warned; focus-right is still reachable via its arrow alias.
        assert_eq!(
            km.prefix_action(&parse_chord("Right").unwrap()),
            Some(Action::FocusRight)
        );
        assert!(
            warns.iter().any(|w| w.contains("FocusRight")),
            "expected a focus-right collision warning: {warns:?}"
        );
    }

    #[test]
    fn chord_labels_use_the_terminal_caret_convention() {
        // What the status bar shows while the prefix is armed. The default prefix must
        // read as `^b`; a user who set `prefix = "C-a"` must see THEIR key, not `^b`.
        let label = |s: &str| parse_chord(s).unwrap().label();
        assert_eq!(label("C-b"), "^b");
        assert_eq!(label("C-a"), "^a");
        assert_eq!(label("M-1"), "M-1");
        assert_eq!(label("%"), "%");
        // Shift on a letter lives in the letter's case (that is how both a live event and
        // a config token canonicalize), so an `S-` prefix would double it up.
        assert_eq!(label("C-S-h"), "^H");
        // Named keys have no case to carry it, so they keep the explicit `S-`.
        assert_eq!(label("C-S-Left"), "^S-Left");
        assert_eq!(label("M-C-Space"), "M-^Space");
    }

    #[test]
    fn custom_prefix_key() {
        let toml = r#"prefix = "C-a""#;
        let (cfg, warns) = load_str(toml);
        assert!(warns.is_empty(), "warns: {warns:?}");
        assert_eq!(cfg.keymap.prefix_chord, parse_chord("C-a").unwrap());
        assert_eq!(
            cfg.keymap.global_action(&parse_chord("C-a").unwrap()),
            Some(Action::EnterPrefix)
        );
    }

    #[test]
    fn bad_chord_and_out_of_range_warn_and_fall_back() {
        let toml = r#"
            scroll_step = 999
            sidebar_width = 2
            [keys]
            next-tab = "Nonsense"
        "#;
        let (cfg, warns) = load_str(toml);
        assert_eq!(cfg.scroll_step, 50); // clamped
        assert_eq!(cfg.sidebar_width, 8); // clamped up
        // next-tab kept its default since the override was invalid.
        assert_eq!(
            cfg.keymap.prefix_action(&parse_chord("n").unwrap()),
            Some(Action::NextTab)
        );
        assert!(warns.len() >= 3, "warns: {warns:?}");
    }

    #[test]
    fn sidebar_relational_floor_enforced() {
        let toml = r#"
            sidebar_width = 60
            sidebar_min_cols = 40
        "#;
        let (cfg, warns) = load_str(toml);
        assert_eq!(cfg.sidebar_width, 60);
        assert_eq!(cfg.sidebar_min_cols, 60 + MIN_CONTENT_COLS);
        assert!(warns.iter().any(|w| w.contains("sidebar_min_cols")));
    }

    #[test]
    fn global_binding_cannot_shadow_prefix() {
        // Bind popup to C-b (the prefix) — prefix entry must still win.
        let toml = r#"
            [global]
            popup = "C-b"
        "#;
        let (cfg, warns) = load_str(toml);
        assert_eq!(
            cfg.keymap.global_action(&parse_chord("C-b").unwrap()),
            Some(Action::EnterPrefix)
        );
        assert!(warns.iter().any(|w| w.contains("shadow")));
    }

    #[test]
    fn usage_style_parses_and_defaults() {
        assert_eq!(MuxConfig::default().usage, UsageStyle::Bar);
        assert_eq!(
            MuxConfig::default().usage_bar_width,
            DEFAULT_USAGE_BAR_WIDTH
        );
        assert_eq!(load_str("usage = \"text\"").0.usage, UsageStyle::Text);
        assert_eq!(load_str("usage = \"off\"").0.usage, UsageStyle::Off);
        assert_eq!(load_str("usage = \"bar\"").0.usage, UsageStyle::Bar);
        // unknown → warns, falls back to bar
        let (cfg, warns) = load_str("usage = \"bogus\"");
        assert_eq!(cfg.usage, UsageStyle::Bar);
        assert!(warns.iter().any(|w| w.contains("usage")));
        // bar width clamps into [3,30]
        assert_eq!(load_str("usage_bar_width = 12").0.usage_bar_width, 12);
        let (cfg, warns) = load_str("usage_bar_width = 999");
        assert_eq!(cfg.usage_bar_width, 30);
        assert!(warns.iter().any(|w| w.contains("usage_bar_width")));
    }

    #[test]
    fn tab_labels_parse_and_default() {
        assert_eq!(MuxConfig::default().tab_labels, TabLabels::Number);
        assert_eq!(
            load_str("tab_labels = \"name\"").0.tab_labels,
            TabLabels::Name
        );
        assert_eq!(
            load_str("tab_labels = \"both\"").0.tab_labels,
            TabLabels::Both
        );
        assert_eq!(
            load_str("tab_labels = \"number\"").0.tab_labels,
            TabLabels::Number
        );
        // Unknown → default + warning.
        let (cfg, warns) = load_str("tab_labels = \"bogus\"");
        assert_eq!(cfg.tab_labels, TabLabels::Number);
        assert!(warns.iter().any(|w| w.contains("tab_labels")));
    }

    #[test]
    fn usage_windows_parse_default_and_filter() {
        // Absent → all windows enabled (historical readout).
        let def = MuxConfig::default().usage_windows;
        assert!(def.claude_5h && def.claude_wk && def.codex_wk);
        // Selecting only the weekly windows.
        let sel = load_str(r#"usage_windows = ["claude-wk", "codex-wk"]"#)
            .0
            .usage_windows;
        assert!(!sel.claude_5h && sel.claude_wk && sel.codex_wk);
        // `claude` alias enables both Claude windows.
        let sel = load_str(r#"usage_windows = ["claude"]"#).0.usage_windows;
        assert!(sel.claude_5h && sel.claude_wk && !sel.codex_wk);
        // Empty list hides every window.
        let sel = load_str("usage_windows = []").0.usage_windows;
        assert!(!sel.claude_5h && !sel.claude_wk && !sel.codex_wk);
        // Unknown id warns and is ignored (others still take).
        let (cfg, warns) = load_str(r#"usage_windows = ["bogus", "codex-wk"]"#);
        assert!(!cfg.usage_windows.claude_5h && cfg.usage_windows.codex_wk);
        assert!(warns.iter().any(|w| w.contains("usage_windows")));
    }

    #[test]
    fn persist_defaults_on_and_autosave_has_a_default() {
        let cfg = MuxConfig::default();
        assert!(cfg.persist);
        assert_eq!(cfg.autosave_secs, DEFAULT_AUTOSAVE_SECS);
        // restore_processes defaults to the built-in agents (claude et al.).
        assert!(cfg.restore_processes.iter().any(|p| p == "claude"));
        // Resuming the live agent conversation on restore is on by default.
        assert!(cfg.restore_agent_sessions);
        assert!(
            !load_str("restore_agent_sessions = false")
                .0
                .restore_agent_sessions
        );
    }

    #[test]
    fn sidebar_density_parses_and_defaults() {
        assert_eq!(MuxConfig::default().sidebar_density, Density::Comfortable);
        assert_eq!(
            load_str("sidebar_density = \"compact\"").0.sidebar_density,
            Density::Compact
        );
        assert_eq!(
            load_str("sidebar_density = \"auto\"").0.sidebar_density,
            Density::Auto
        );
        // Unknown values warn and fall back rather than breaking the mux.
        let (cfg, warns) = load_str("sidebar_density = \"tiny\"");
        assert_eq!(cfg.sidebar_density, Density::Comfortable);
        assert!(warns.iter().any(|w| w.contains("sidebar_density")));
    }

    #[test]
    /// `auto` is the only mode that consults the overflow flag; the other two are fixed.
    fn density_entry_rows_only_varies_for_auto() {
        assert_eq!(Density::Comfortable.entry_rows(true), 2);
        assert_eq!(Density::Compact.entry_rows(false), 1);
        assert_eq!(Density::Auto.entry_rows(false), 2);
        assert_eq!(Density::Auto.entry_rows(true), 1);
    }

    #[test]
    fn sort_by_parses_and_defaults() {
        assert_eq!(MuxConfig::default().sort_by, SortBy::Created);
        assert_eq!(
            load_str("sort_by = \"alphabetical\"").0.sort_by,
            SortBy::Alphabetical
        );
        assert_eq!(load_str("sort_by = \"recent\"").0.sort_by, SortBy::Recent);
        assert_eq!(
            load_str("sort_by = \"activity\"").0.sort_by,
            SortBy::Activity
        );
        // Unknown → default + warning.
        let (cfg, warns) = load_str("sort_by = \"bogus\"");
        assert_eq!(cfg.sort_by, SortBy::Created);
        assert!(warns.iter().any(|w| w.contains("sort_by")));
    }

    #[test]
    fn restore_processes_override_replaces_default() {
        let (cfg, _) = load_str(r#"restore_processes = ["nvim", "python"]"#);
        assert_eq!(cfg.restore_processes, vec!["nvim", "python"]);
        // Empty list disables program re-execution.
        let (off, _) = load_str("restore_processes = []");
        assert!(off.restore_processes.is_empty());
    }

    #[test]
    fn update_environment_default_override_and_blocklist() {
        // Default (no key) carries the volatile session vars, incl. the SSH_* set.
        let (def, _) = load_str("");
        assert!(def.update_environment.iter().any(|v| v == "SSH_CONNECTION"));
        assert!(def.update_environment.iter().any(|v| v == "DISPLAY"));
        // Explicit override replaces the default.
        let (cfg, _) = load_str(r#"update_environment = ["FOO", "BAR"]"#);
        assert_eq!(cfg.update_environment, vec!["FOO", "BAR"]);
        // Empty list is honored (disables env refresh).
        let (off, _) = load_str("update_environment = []");
        assert!(off.update_environment.is_empty());
        // Load-bearing names are dropped + warned; duplicates collapse.
        let (guarded, warns) =
            load_str(r#"update_environment = ["PATH", "HOME", "DISPLAY", "DISPLAY"]"#);
        assert_eq!(guarded.update_environment, vec!["DISPLAY"]);
        assert!(warns.iter().any(|w| w.contains("PATH")));
        assert!(warns.iter().any(|w| w.contains("HOME")));
        // Invalid names (`=`) are dropped + warned — they would panic env::remove_var.
        let (bad, badwarn) = load_str(r#"update_environment = ["BAD=NAME", "OK"]"#);
        assert_eq!(bad.update_environment, vec!["OK"]);
        assert!(badwarn.iter().any(|w| w.contains("valid variable name")));
    }

    #[test]
    fn autosave_zero_disables_and_out_of_range_clamps() {
        let (z, _) = load_str("autosave_secs = 0");
        assert_eq!(z.autosave_secs, 0); // 0 is a valid "disabled" value, not clamped
        let (hi, warns) = load_str("autosave_secs = 100000");
        assert_eq!(hi.autosave_secs, 3600);
        assert!(warns.iter().any(|w| w.contains("autosave_secs")));
        let (off, _) = load_str("persist = false");
        assert!(!off.persist);
    }

    fn load_str(toml: &str) -> (MuxConfig, Vec<String>) {
        let raw: RawConfig = toml::from_str(toml).unwrap();
        MuxConfig::from_raw(raw)
    }
}
