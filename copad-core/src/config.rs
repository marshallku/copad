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

    /// When the PTY child (typically the user's shell) exits, close
    /// the owning pane → cascade up to tab + window if it was the
    /// last pane / tab. `false` keeps the dead-PTY viewport visible
    /// so the user can read the exit message; they must close the
    /// pane manually (Cmd+W / Ctrl+Shift+W). Default `true` matches
    /// the long-standing Linux behavior pre-config.
    #[serde(default = "default_true")]
    pub close_on_exit: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            shell: default_shell(),
            font_family: default_font_family(),
            font_size: default_font_size(),
            close_on_exit: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundConfig {
    #[serde(default)]
    pub image: Option<String>,

    #[serde(default = "default_tint")]
    pub tint: f64,

    #[serde(default = "default_tint_color")]
    pub tint_color: String,

    #[serde(default = "default_opacity")]
    pub opacity: f64,

    /// Auto-rotation cadence in seconds for random wallpapers from the
    /// platform list file. 0 (default) disables the timer — manual
    /// `background.next` keeps working either way. A static `image` is
    /// applied at startup; the first rotation tick then takes over.
    #[serde(default)]
    pub rotate_interval: u64,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            image: None,
            tint: default_tint(),
            tint_color: default_tint_color(),
            opacity: default_opacity(),
            rotate_interval: 0,
        }
    }
}

/// `[window]` — window-level transparency / blur. Distinct from
/// `[background]` (which only affects the optional background-image
/// layer); these knobs control the window itself so the desktop /
/// blurred surface behind it shows through the terminal (Ghostty
/// model). `blur` is macOS-only (NSVisualEffectView); Linux ignores it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowConfig {
    /// 0.0 = fully transparent, 1.0 = fully opaque. Clamped at read
    /// time (`load_from`-side callers should normalize; the field
    /// here trusts the parse).
    #[serde(default = "default_window_opacity")]
    pub opacity: f64,

    /// macOS-only. When true and `opacity < 1.0`, the window installs
    /// an `NSVisualEffectView` behind the content view so the desktop
    /// behind is blurred (Ghostty `background-blur-radius` equivalent).
    /// No-op on Linux today; the key is accepted for cross-platform
    /// config parity so the same config.toml works on both.
    #[serde(default = "default_window_blur")]
    pub blur: bool,

    /// Solid base color for the window backdrop (`#rrggbb`). `opacity`
    /// is this color's alpha, so it is the fraction of the color that
    /// stays "maintained" over the desktop — a dark value keeps text
    /// readable on a bright wallpaper. Defaults to the theme background
    /// when unset. Linux-only today (macOS uses the theme background).
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
    /// Tab bar position: "top", "bottom", "left", "right"
    #[serde(default = "default_tab_position")]
    pub position: String,
    /// Width of vertical tabs in pixels (left/right position)
    #[serde(default = "default_tab_width")]
    pub width: u32,
    /// Whether the tab bar starts collapsed (icon-only). Default: true
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
    /// Whether the status bar is enabled. Default: true
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Position: "top" or "bottom". Default: "bottom"
    #[serde(default = "default_statusbar_position")]
    pub position: String,
    /// Height in pixels. Default: 28
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
    /// Key combo → command mapping, e.g. "ctrl+shift+g" = "spawn:~/script.sh --arg"
    #[serde(flatten)]
    pub map: HashMap<String, String>,
}

/// Parsed keybinding ready for matching
#[derive(Debug, Clone)]
pub struct ParsedKeybinding {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: String,
    pub command: String,
}

impl KeybindingsConfig {
    /// Parse all keybinding entries into structured form
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
    pub keybindings: KeybindingsConfig,

    /// Declarative event → action automation. See `docs/workflow-runtime.md`.
    #[serde(default)]
    pub triggers: Vec<Trigger>,

    /// Project entries — see `docs/project-orchestration.md` § Project entity
    /// + git_remote resolution. Phase 22.2+.
    #[serde(default)]
    pub projects: Vec<crate::project::Project>,
}

impl CopadConfig {
    /// `$XDG_CONFIG_HOME/copad/config.toml`, else `~/.config/copad/
    /// config.toml`. macOS deliberately overrides `dirs::config_dir()`
    /// (which would point at `~/Library/Application Support/`) so the
    /// Swift renderer (which hardcodes `~/.config/copad/`), the Rust
    /// daemon, and `coctl` all load the same file — and users with
    /// XDG-style dotfiles repos see their config on macOS too.
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

    /// First-start loader for the GUI: on a parse/IO error, fall back to built-in defaults but
    /// return a human-readable warning so the caller can surface it loudly (desktop-entry launches
    /// hide stderr, so a typo in `config.toml` would otherwise silently discard the whole user
    /// config). Distinct from [`load`](Self::load), which propagates the error, and from the
    /// hot-reload path, which keeps the already-running config rather than resetting to defaults.
    /// A missing file is the normal first-run state and yields `(default, None)`.
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

    /// Path-taking loader. Daemon config watcher uses this with a
    /// monitored path; tests use it with an isolated tempfile.
    /// Returns the default config when the path doesn't exist
    /// (matches `load()`'s contract — first-run users have no file
    /// yet and we don't want startup to fail).
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
# image = "/path/to/wallpaper.jpg"
# tint = 0.85
# tint_color = "#1e1e2e"
# opacity = 0.95
# rotate_interval = 300  # seconds between random wallpapers from the
#                        # platform list file; 0 (default) = no auto-rotation

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
    fn window_config_defaults_when_section_absent() {
        // Default-constructed `CopadConfig` (no config file present) must
        // give `opacity = 1.0` so the macOS / Linux window stays opaque
        // for users who never opt in. Regression guard: a future
        // refactor that changes the `Default` derive must keep this.
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
        // Falls back to defaults (so the GUI still starts) ...
        assert_eq!(
            cfg.terminal.font_size,
            CopadConfig::default().terminal.font_size
        );
        // ... but loudly: the warning names the offending file so the user can find it.
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
}
