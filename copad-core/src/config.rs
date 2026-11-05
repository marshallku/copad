use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::trigger::Trigger;

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn default_font_family() -> String {
    "JetBrainsMono Nerd Font Mono".to_string()
}

fn default_font_size() -> u32 {
    14
}

fn default_tint() -> f64 {
    0.85
}

fn default_tint_color() -> String {
    "#1e1e2e".to_string()
}

fn default_opacity() -> f64 {
    0.95
}

/// Image extensions scanned when `[background] image` is a directory.
fn default_extensions() -> Vec<String> {
    ["jpg", "jpeg", "png", "webp"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_window_opacity() -> f64 {
    1.0
}

fn default_window_blur() -> bool {
    false
}

fn default_tab_position() -> String {
    "top".to_string()
}

fn default_tab_width() -> u32 {
    120
}

fn default_theme() -> String {
    "catppuccin-mocha".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalConfig {
    #[serde(default = "default_shell")]
    pub shell: String,

    #[serde(default = "default_font_family")]
    pub font_family: String,

    #[serde(default = "default_font_size")]
    pub font_size: u32,

    /// Close the owning pane (cascading to tab and window) when the PTY child exits. `false`
    /// keeps the dead viewport visible so the exit message can be read; close it manually.
    #[serde(default = "default_true")]
    pub close_on_exit: bool,

    /// Let programs negotiate the kitty keyboard protocol (`CSI > flags u`), which is how
    /// Claude Code receives Shift+Enter as a newline (decisions #164). `false` leaves the
    /// query unanswered, so programs keep the legacy key encoding.
    #[serde(default = "default_true")]
    pub kitty_keyboard: bool,

    /// Lines of history per pane. On macOS every kept line costs its full width (alacritty
    /// stores cells: a 200-column line is ~4.8 KB), so this is the main memory lever; VTE on
    /// Linux compresses history. Any integer is accepted and clamped to 0..=100 000. Lowering
    /// it on a running copad drops the oldest lines for good.
    #[serde(
        default = "default_scrollback",
        deserialize_with = "deserialize_scrollback"
    )]
    pub scrollback: usize,

    /// Let programs draw images in the terminal (sixel on Linux, kitty graphics on macOS), as
    /// yazi and `kitten icat` do. `false` ignores image sequences and stops advertising support.
    #[serde(default = "default_true")]
    pub images: bool,
}

pub const MAX_SCROLLBACK: usize = 100_000;

fn default_scrollback() -> usize {
    10_000
}

/// Any integer, clamped: a negative or huge value means "none" / "as much as allowed", not a
/// config error that would discard every other setting.
fn deserialize_scrollback<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<usize, D::Error> {
    let n = i64::deserialize(d)?;
    Ok(n.clamp(0, MAX_SCROLLBACK as i64) as usize)
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            shell: default_shell(),
            font_family: default_font_family(),
            font_size: default_font_size(),
            close_on_exit: true,
            kitty_keyboard: true,
            scrollback: default_scrollback(),
            images: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundConfig {
    /// A single image file or a directory to pick random wallpapers from, resolved by `stat` at
    /// use time (`path` is an accepted alias, as in macOS's Swift config). A directory becomes
    /// the rotation source and `list` is not consulted.
    #[serde(default, alias = "path")]
    pub image: Option<String>,

    /// Explicit wallpaper list file, one path per line, used when `image` is unset or a plain
    /// file; defaults to the platform cache location. Written by `coctl background cache`.
    #[serde(default)]
    pub list: Option<String>,

    /// Extensions treated as images when `image` is a directory, case-insensitive, with or
    /// without a leading dot.
    #[serde(default = "default_extensions")]
    pub extensions: Vec<String>,

    /// Descend into subdirectories when `image` is a directory. Off by default.
    #[serde(default)]
    pub recursive: bool,

    #[serde(default = "default_tint")]
    pub tint: f64,

    #[serde(default = "default_tint_color")]
    pub tint_color: String,

    #[serde(default = "default_opacity")]
    pub opacity: f64,

    /// Auto-rotation cadence in seconds; 0 (default) disables the timer, and manual
    /// `background.next` works either way. A static `image` is applied at startup, then the first
    /// rotation tick takes over.
    #[serde(default)]
    pub rotate_interval: u64,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            image: None,
            list: None,
            extensions: default_extensions(),
            recursive: false,
            tint: default_tint(),
            tint_color: default_tint_color(),
            opacity: default_opacity(),
            rotate_interval: 0,
        }
    }
}

impl BackgroundConfig {
    /// `image`, tilde-expanded. File vs directory is decided by the caller at use time (it may not
    /// exist yet).
    pub fn source_path(&self) -> Option<PathBuf> {
        self.image
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(expand_tilde)
    }

    pub fn list_path(&self) -> Option<PathBuf> {
        self.list
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(expand_tilde)
    }
}

pub fn expand_tilde(raw: &str) -> PathBuf {
    let Some(rest) = raw.strip_prefix('~') else {
        return PathBuf::from(raw);
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return PathBuf::from(raw);
    }
    match dirs::home_dir() {
        Some(home) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(raw),
    }
}

/// `[window]`: window-level transparency and blur, distinct from `[background]` (the optional
/// image layer). `blur` is macOS-only (NSVisualEffectView); Linux ignores it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    /// 0.0 fully transparent to 1.0 fully opaque. Not clamped here; callers normalize.
    #[serde(default = "default_window_opacity")]
    pub opacity: f64,

    /// macOS-only: with `opacity < 1.0`, installs an `NSVisualEffectView` behind the content so
    /// the desktop is blurred. A no-op on Linux; the key is accepted so one config.toml works on
    /// both.
    #[serde(default = "default_window_blur")]
    pub blur: bool,

    /// Solid base color for the window backdrop (`#rrggbb`); `opacity` is its alpha, so a dark
    /// value keeps text readable on a bright wallpaper. Defaults to the theme background.
    /// Linux-only (macOS uses the theme background).
    #[serde(default)]
    pub background: Option<String>,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            opacity: default_window_opacity(),
            blur: default_window_blur(),
            background: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default = "default_theme")]
    pub name: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: default_theme(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabsConfig {
    #[serde(default = "default_tab_position")]
    pub position: String,
    #[serde(default = "default_tab_width")]
    pub width: u32,
    #[serde(default = "default_true")]
    pub collapsed: bool,
}

fn default_true() -> bool {
    true
}

impl Default for TabsConfig {
    fn default() -> Self {
        Self {
            position: default_tab_position(),
            width: default_tab_width(),
            collapsed: true,
        }
    }
}

fn default_statusbar_height() -> u32 {
    28
}

fn default_statusbar_position() -> String {
    "bottom".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusBarConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_statusbar_position")]
    pub position: String,
    #[serde(default = "default_statusbar_height")]
    pub height: u32,
}

impl Default for StatusBarConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            position: default_statusbar_position(),
            height: default_statusbar_height(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KeybindingsConfig {
    #[serde(flatten)]
    pub map: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct ParsedKeybinding {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: String,
    pub command: String,
}

impl KeybindingsConfig {
    pub fn parse(&self) -> Vec<ParsedKeybinding> {
        self.map
            .iter()
            .filter_map(|(combo, cmd)| Self::parse_one(combo, cmd))
            .collect()
    }

    fn parse_one(combo: &str, command: &str) -> Option<ParsedKeybinding> {
        let parts: Vec<&str> = combo.split('+').collect();
        if parts.is_empty() {
            return None;
        }

        let mut ctrl = false;
        let mut shift = false;
        let mut alt = false;
        let mut key = None;

        for part in &parts {
            match part.to_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "shift" => shift = true,
                "alt" => alt = true,
                k => key = Some(k.to_string()),
            }
        }

        Some(ParsedKeybinding {
            ctrl,
            shift,
            alt,
            key: key?,
            command: command.to_string(),
        })
    }
}

fn default_restore_policy() -> String {
    "origin".to_string()
}

fn default_secret_confirm() -> String {
    "once".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserConfig {
    /// How much of a live URL may be persisted: `origin` (default) | `url` | `full`. `url` and
    /// `full` are sensitive persistence; see `browser::restore::RestorePolicy`.
    #[serde(default = "default_restore_policy")]
    pub restore: String,
    /// Profile whose data store new browser panes use. `default` maps onto the platform's
    /// pre-existing default store; any other name starts logged out (see `browser::profile`).
    #[serde(default = "default_profile")]
    pub profile: String,
    /// `once`: confirm a credential fill once per (origin, credential) per app run; `always`:
    /// every time.
    #[serde(default = "default_secret_confirm")]
    pub secret_confirm: String,
    #[serde(default)]
    pub capture_bodies: bool,
}

fn default_profile() -> String {
    crate::browser::DEFAULT_PROFILE.to_string()
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            restore: default_restore_policy(),
            profile: default_profile(),
            secret_confirm: default_secret_confirm(),
            capture_bodies: false,
        }
    }
}

impl BrowserConfig {
    /// Falls back to the safe default on an unknown value: a typo must neither escalate what
    /// reaches disk nor brick startup.
    pub fn restore_policy(&self) -> crate::browser::RestorePolicy {
        crate::browser::RestorePolicy::parse(&self.restore).unwrap_or_default()
    }

    pub fn confirm_every_fill(&self) -> bool {
        self.secret_confirm.eq_ignore_ascii_case("always")
    }

    pub fn log_caps(&self) -> crate::browser::LogCaps {
        crate::browser::LogCaps {
            capture_bodies: self.capture_bodies,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CopadConfig {
    #[serde(default)]
    pub terminal: TerminalConfig,

    #[serde(default)]
    pub background: BackgroundConfig,

    #[serde(default)]
    pub window: WindowConfig,

    #[serde(default)]
    pub tabs: TabsConfig,

    #[serde(default)]
    pub theme: ThemeConfig,

    #[serde(default)]
    pub statusbar: StatusBarConfig,

    #[serde(default)]
    pub browser: BrowserConfig,

    #[serde(default)]
    pub keybindings: KeybindingsConfig,

    #[serde(default)]
    pub triggers: Vec<Trigger>,

    #[serde(default)]
    pub projects: Vec<crate::project::Project>,
}

impl CopadConfig {
    /// `$XDG_CONFIG_HOME/copad/config.toml`, else `~/.config/copad/config.toml`. macOS overrides
    /// `dirs::config_dir()` so the Swift renderer (which hardcodes `~/.config/copad/`), the Rust
    /// daemon and `coctl` load the same file.
    pub fn config_path() -> PathBuf {
        Self::config_dir().join("copad").join("config.toml")
    }

    fn config_dir() -> PathBuf {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
            && !xdg.is_empty()
        {
            return PathBuf::from(xdg);
        }
        #[cfg(target_os = "macos")]
        {
            if let Some(home) = dirs::home_dir() {
                return home.join(".config");
            }
        }
        dirs::config_dir().unwrap_or_else(|| PathBuf::from("/etc"))
    }

    pub fn load() -> Result<Self> {
        Self::load_from(&Self::config_path())
    }

    /// First-start loader for the GUI: on a parse/IO error, falls back to defaults and returns a
    /// warning to surface loudly (desktop-entry launches hide stderr, so a typo would otherwise
    /// silently discard the whole config). Unlike [`load`](Self::load) it does not propagate the
    /// error. A missing file is the normal first run and yields `(default, None)`.
    pub fn load_or_default_warning() -> (Self, Option<String>) {
        Self::load_or_default_warning_from(&Self::config_path())
    }

    fn load_or_default_warning_from(path: &Path) -> (Self, Option<String>) {
        match Self::load_from(path) {
            Ok(config) => (config, None),
            Err(e) => {
                let warning = format!(
                    "{} could not be parsed — built-in defaults are in effect and your settings \
                     were NOT applied: {e}",
                    path.display()
                );
                (Self::default(), Some(warning))
            }
        }
    }

    /// Path-taking loader (daemon config watcher, isolated tests). A missing file yields the
    /// default config, like `load()`, so first run doesn't fail startup.
    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = std::fs::read_to_string(path)?;
        toml::from_str(&contents).map_err(|e| crate::error::CopadError::Config(e.to_string()))
    }

    pub fn write_default() -> Result<PathBuf> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let default_config = r##"[terminal]
# shell = "/bin/zsh"
font_family = "JetBrainsMono Nerd Font Mono"
font_size = 14

[background]
# image = "/path/to/wallpaper.jpg"   # a FILE = fixed wallpaper
# image = "~/wallpapers"             # a DIRECTORY = random pick from it
# recursive = false      # descend into subdirectories (directory source only)
# extensions = ["jpg", "jpeg", "png", "webp"]
# list = "~/.cache/terminal-wallpapers.txt"  # explicit list file (one path per
#                        # line), used when `image` is unset or is a plain file;
#                        # `coctl background cache` writes this file
# tint = 0.85
# tint_color = "#1e1e2e"
# opacity = 0.95
# rotate_interval = 300  # seconds between random wallpapers; 0 (default) = no
#                        # auto-rotation (`coctl background next` still works)

[window]
# opacity = 0.85        # 0.0 = fully transparent, 1.0 = fully opaque (default)
# background = "#000000" # base color blended with the desktop at `opacity`;
#                        # a dark value keeps text readable on a bright wallpaper
#                        # (defaults to the theme background when unset)
# blur = false          # macOS only: blur the desktop behind the window (Ghostty-style)

[tabs]
# position = "top"  # top, bottom, left, right
# width = 120       # vertical tab width in pixels (left/right)
# collapsed = true  # start with tab bar collapsed (icon-only)

[theme]
# Available: catppuccin-mocha, catppuccin-latte, catppuccin-frappe, catppuccin-macchiato,
#            dracula, nord, tokyo-night, gruvbox-dark, one-dark, solarized-dark
name = "catppuccin-mocha"

[statusbar]
# enabled = true       # Show/hide the status bar
# position = "bottom"  # "top" or "bottom"
# height = 28          # Height in pixels

[keybindings]
# Map key combos to shell commands (spawn:) — runs in background
# "ctrl+shift+g" = "spawn:~/my-script.sh --next"
# "ctrl+shift+m" = "spawn:~/my-script.sh --toggle"

# [[triggers]]
# name = "log-cwd"
# action = "system.log"
# # Interpolation tokens: {event.<payload-key>} reaches into the event's
# # JSON payload; if missing there, falls back to {event.kind|source|timestamp_ms}.
# params = { message = "[{event.timestamp_ms}] cwd: {event.cwd}" }
# [triggers.when]
# event_kind = "terminal.cwd_changed"
"##;
        std::fs::write(&path, default_config)?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "copad-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn load_from_missing_path_returns_default() {
        let dir = tmp_dir();
        let path = dir.join("does-not-exist.toml");
        let cfg = CopadConfig::load_from(&path).expect("load");
        assert!(cfg.triggers.is_empty());
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn scrollback_is_clamped_not_rejected() {
        let parse = |t: &str| toml::from_str::<CopadConfig>(t).map(|c| c.terminal);
        assert_eq!(parse("").unwrap().scrollback, 10_000);
        assert_eq!(
            parse("[terminal]\nscrollback = 2000").unwrap().scrollback,
            2_000
        );
        let t = parse("[terminal]\nscrollback = -5\nfont_size = 17").unwrap();
        assert_eq!((t.scrollback, t.font_size), (0, 17));
        assert_eq!(
            parse("[terminal]\nscrollback = 9999999")
                .unwrap()
                .scrollback,
            MAX_SCROLLBACK
        );
        assert!(parse("[terminal]\nscrollback = \"lots\"").is_err());
    }

    #[test]
    fn images_default_on_and_can_be_turned_off() {
        let parse = |t: &str| toml::from_str::<CopadConfig>(t).map(|c| c.terminal);
        assert!(parse("").unwrap().images);
        assert!(!parse("[terminal]\nimages = false").unwrap().images);
    }

    #[test]
    fn terminal_close_on_exit_defaults_true() {
        let cfg = CopadConfig::default();
        assert!(
            cfg.terminal.close_on_exit,
            "default must close pane on shell exit — matches long-standing Linux behavior pre-opt-out"
        );
    }

    #[test]
    fn load_from_parses_terminal_close_on_exit_false() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
[terminal]
close_on_exit = false
"#,
        )
        .expect("write");
        let cfg = CopadConfig::load_from(&path).expect("load");
        assert!(!cfg.terminal.close_on_exit);
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn background_accepts_path_as_an_alias_for_image() {
        // macOS's Swift config takes both spellings; Rust must agree or one config.toml behaves
        // differently per platform.
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
[background]
path = "/wallpapers"
"#,
        )
        .expect("write");
        let cfg = CopadConfig::load_from(&path).expect("load");
        assert_eq!(cfg.background.image.as_deref(), Some("/wallpapers"));
        assert_eq!(
            cfg.background.source_path(),
            Some(PathBuf::from("/wallpapers"))
        );
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn background_source_and_list_expand_tilde() {
        let home = dirs::home_dir().expect("home");
        let cfg = BackgroundConfig {
            image: Some("~/wallpapers".to_string()),
            list: Some("~/.cache/list.txt".to_string()),
            ..Default::default()
        };
        assert_eq!(cfg.source_path(), Some(home.join("wallpapers")));
        assert_eq!(cfg.list_path(), Some(home.join(".cache/list.txt")));

        // An empty string is "unset", not the home directory.
        let blank = BackgroundConfig {
            image: Some(String::new()),
            list: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(blank.source_path(), None);
        assert_eq!(blank.list_path(), None);
    }

    #[test]
    fn expand_tilde_leaves_non_leading_and_named_tildes_alone() {
        assert_eq!(expand_tilde("/a/~/b"), PathBuf::from("/a/~/b"));
        assert_eq!(expand_tilde("~other/pics"), PathBuf::from("~other/pics"));
        assert_eq!(expand_tilde("relative/dir"), PathBuf::from("relative/dir"));
    }

    #[test]
    fn background_defaults_keep_the_pre_directory_source_behavior() {
        let cfg = BackgroundConfig::default();
        // Directory scanning is opt-in: an existing `image = "/some.jpg"` must not start recursing.
        assert!(!cfg.recursive);
        assert_eq!(cfg.list, None);
        assert_eq!(cfg.extensions, vec!["jpg", "jpeg", "png", "webp"]);
    }

    #[test]
    fn window_config_defaults_when_section_absent() {
        // No config file must give `opacity = 1.0` so windows stay opaque for users who never
        // opt in.
        let cfg = CopadConfig::default();
        assert_eq!(cfg.window.opacity, 1.0);
        assert!(!cfg.window.blur);
        assert_eq!(cfg.window.background, None);
    }

    #[test]
    fn load_from_parses_window_section() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r##"
[window]
opacity = 0.85
blur = true
background = "#000000"
"##,
        )
        .expect("write");
        let cfg = CopadConfig::load_from(&path).expect("load");
        assert!((cfg.window.opacity - 0.85).abs() < 1e-9);
        assert!(cfg.window.blur);
        assert_eq!(cfg.window.background.as_deref(), Some("#000000"));
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn background_rotate_interval_defaults_zero_and_parses() {
        assert_eq!(CopadConfig::default().background.rotate_interval, 0);

        let dir = tmp_dir();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r##"
[background]
rotate_interval = 300
"##,
        )
        .expect("write");
        let cfg = CopadConfig::load_from(&path).expect("load");
        assert_eq!(cfg.background.rotate_interval, 300);
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn load_from_parses_triggers_section() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
[[triggers]]
name = "t1"
action = "system.log"
params = { message = "hi" }
[triggers.when]
event_kind = "x.fired"
"#,
        )
        .expect("write");
        let cfg = CopadConfig::load_from(&path).expect("load");
        assert_eq!(cfg.triggers.len(), 1);
        assert_eq!(cfg.triggers[0].name, "t1");
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn load_from_parses_projects_section() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
[[projects]]
name = "copad"
path = "/home/me/dev/copad"
git_remote = "marshallku/copad"
aliases = ["copad-app"]

[[projects]]
name = "monorepo"
path = "/home/me/dev/mono"
subpath = "apps/web"
"#,
        )
        .expect("write");
        let cfg = CopadConfig::load_from(&path).expect("load");
        assert_eq!(cfg.projects.len(), 2);
        assert_eq!(cfg.projects[0].name, "copad");
        assert_eq!(
            cfg.projects[0].git_remote.as_deref(),
            Some("marshallku/copad")
        );
        assert_eq!(cfg.projects[0].aliases, vec!["copad-app".to_string()]);
        assert_eq!(
            cfg.projects[1].subpath,
            Some(std::path::PathBuf::from("apps/web"))
        );
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn load_from_missing_projects_section_is_empty() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[terminal]\n").expect("write");
        let cfg = CopadConfig::load_from(&path).expect("load");
        assert!(cfg.projects.is_empty());
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn load_from_bubbles_toml_parse_error() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not valid toml = = =").expect("write");
        let err = CopadConfig::load_from(&path).unwrap_err();
        assert!(matches!(err, crate::error::CopadError::Config(_)));
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn load_or_default_warning_missing_file_is_quiet() {
        let dir = tmp_dir();
        let path = dir.join("does-not-exist.toml");
        let (cfg, warning) = CopadConfig::load_or_default_warning_from(&path);
        assert!(
            warning.is_none(),
            "a missing file is the normal first-run state, not a warning"
        );
        assert!(cfg.triggers.is_empty());
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn load_or_default_warning_valid_file_is_quiet() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        std::fs::write(&path, "[terminal]\nfont_size = 18\n").expect("write");
        let (cfg, warning) = CopadConfig::load_or_default_warning_from(&path);
        assert!(warning.is_none());
        assert_eq!(cfg.terminal.font_size, 18);
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn load_or_default_warning_parse_error_falls_back_loudly() {
        let dir = tmp_dir();
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not valid toml = = =").expect("write");
        let (cfg, warning) = CopadConfig::load_or_default_warning_from(&path);
        assert_eq!(
            cfg.terminal.font_size,
            CopadConfig::default().terminal.font_size
        );
        let warning = warning.expect("a parse error must produce a warning");
        assert!(
            warning.contains("config.toml"),
            "warning names the file: {warning}"
        );
        assert!(
            warning.contains("NOT applied"),
            "warning says config was dropped: {warning}"
        );
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn browser_defaults_to_the_safe_restore_policy_and_the_platform_default_profile() {
        let c = BrowserConfig::default();
        assert_eq!(c.restore_policy(), crate::browser::RestorePolicy::Origin);
        assert_eq!(c.profile, crate::browser::DEFAULT_PROFILE);
        assert!(!c.capture_bodies);
        assert!(!c.confirm_every_fill());
    }

    #[test]
    fn an_absent_browser_section_still_yields_the_safe_defaults() {
        let c: CopadConfig = toml::from_str("[terminal]\n").unwrap();
        assert_eq!(
            c.browser.restore_policy(),
            crate::browser::RestorePolicy::Origin
        );
    }

    #[test]
    fn a_typo_in_restore_falls_back_to_origin_rather_than_escalating() {
        let c: CopadConfig = toml::from_str("[browser]\nrestore = \"ful\"\n").unwrap();
        assert_eq!(
            c.browser.restore_policy(),
            crate::browser::RestorePolicy::Origin
        );
    }

    #[test]
    fn opting_into_full_restore_is_honoured() {
        let c: CopadConfig = toml::from_str(
            "[browser]\nrestore = \"full\"\ncapture_bodies = true\nsecret_confirm = \"always\"\n",
        )
        .unwrap();
        assert_eq!(
            c.browser.restore_policy(),
            crate::browser::RestorePolicy::Full
        );
        assert!(c.browser.log_caps().capture_bodies);
        assert!(c.browser.confirm_every_fill());
    }
}
