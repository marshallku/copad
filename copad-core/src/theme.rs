use serde::{Deserialize, Serialize};

/// A complete terminal color theme with semantic UI colors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,
    /// Terminal foreground (text color)
    pub foreground: String,
    /// Terminal background
    pub background: String,
    /// 16-color ANSI palette (8 normal + 8 bright)
    pub palette: [String; 16],

    // UI semantic colors
    /// Darker background (tab header, panels)
    pub surface0: String,
    /// Slightly lighter than surface0 (hover bg)
    pub surface1: String,
    /// Active/selected bg (active tab, search bar bg)
    pub surface2: String,
    /// Borders, subtle separators
    pub overlay0: String,
    /// Primary text (active tabs, UI labels)
    pub text: String,
    /// Secondary/dim text (inactive tabs)
    pub subtext0: String,
    /// Hover text (slightly brighter than subtext0)
    pub subtext1: String,
    /// Accent color (focus rings, active indicators)
    pub accent: String,
    /// Destructive/error color (close hover)
    pub red: String,
}

impl Theme {
    /// Look up a built-in theme by name. Returns None if not found.
    pub fn by_name(name: &str) -> Option<Self> {
        match name {
            "catppuccin-mocha" => Some(catppuccin_mocha()),
            "catppuccin-latte" => Some(catppuccin_latte()),
            "catppuccin-frappe" => Some(catppuccin_frappe()),
            "catppuccin-macchiato" => Some(catppuccin_macchiato()),
            "dracula" => Some(dracula()),
            "nord" => Some(nord()),
            "tokyo-night" => Some(tokyo_night()),
            "gruvbox-dark" => Some(gruvbox_dark()),
            "one-dark" => Some(one_dark()),
            "solarized-dark" => Some(solarized_dark()),
            _ => None,
        }
    }

    /// List all built-in theme names.
    pub fn list() -> &'static [&'static str] {
        &[
            "catppuccin-mocha",
            "catppuccin-latte",
            "catppuccin-frappe",
            "catppuccin-macchiato",
            "dracula",
            "nord",
            "tokyo-night",
            "gruvbox-dark",
            "one-dark",
            "solarized-dark",
        ]
    }
}

impl Default for Theme {
    fn default() -> Self {
        catppuccin_mocha()
    }
}

fn catppuccin_mocha() -> Theme {
    Theme {
        name: "catppuccin-mocha".into(),
        foreground: "#cdd6f4".into(),
        background: "#1e1e2e".into(),
        palette: [
            "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#bac2de",
            "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5", "#a6adc8",
        ]
        .map(Into::into),
        surface0: "#181825".into(),
        surface1: "#262637".into(),
        surface2: "#313244".into(),
        overlay0: "#45475a".into(),
        text: "#cdd6f4".into(),
        subtext0: "#6c7086".into(),
        subtext1: "#bac2de".into(),
        accent: "#89b4fa".into(),
        red: "#f38ba8".into(),
    }
}

fn catppuccin_latte() -> Theme {
    Theme {
        name: "catppuccin-latte".into(),
        foreground: "#4c4f69".into(),
        background: "#eff1f5".into(),
        palette: [
            "#5c5f77", "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299", "#acb0be",
            "#6c6f85", "#d20f39", "#40a02b", "#df8e1d", "#1e66f5", "#ea76cb", "#179299", "#bcc0cc",
        ]
        .map(Into::into),
        surface0: "#ccd0da".into(),
        surface1: "#dce0e8".into(),
        surface2: "#bcc0cc".into(),
        overlay0: "#9ca0b0".into(),
        text: "#4c4f69".into(),
        subtext0: "#6c6f85".into(),
        subtext1: "#5c5f77".into(),
        accent: "#1e66f5".into(),
        red: "#d20f39".into(),
    }
}

fn catppuccin_frappe() -> Theme {
    Theme {
        name: "catppuccin-frappe".into(),
        foreground: "#c6d0f5".into(),
        background: "#303446".into(),
        palette: [
            "#51576d", "#e78284", "#a6d189", "#e5c890", "#8caaee", "#f4b8e4", "#81c8be", "#b5bfe2",
            "#626880", "#e78284", "#a6d189", "#e5c890", "#8caaee", "#f4b8e4", "#81c8be", "#a5adce",
        ]
        .map(Into::into),
        surface0: "#292c3c".into(),
        surface1: "#353849".into(),
        surface2: "#414559".into(),
        overlay0: "#51576d".into(),
        text: "#c6d0f5".into(),
        subtext0: "#737994".into(),
        subtext1: "#b5bfe2".into(),
        accent: "#8caaee".into(),
        red: "#e78284".into(),
    }
}

fn catppuccin_macchiato() -> Theme {
    Theme {
        name: "catppuccin-macchiato".into(),
        foreground: "#cad3f5".into(),
        background: "#24273a".into(),
        palette: [
            "#494d64", "#ed8796", "#a6da95", "#eed49f", "#8aadf4", "#f5bde6", "#8bd5ca", "#b8c0e0",
            "#5b6078", "#ed8796", "#a6da95", "#eed49f", "#8aadf4", "#f5bde6", "#8bd5ca", "#a5adcb",
        ]
        .map(Into::into),
        surface0: "#1e2030".into(),
        surface1: "#2e3248".into(),
        surface2: "#363a4f".into(),
        overlay0: "#494d64".into(),
        text: "#cad3f5".into(),
        subtext0: "#6e738d".into(),
        subtext1: "#b8c0e0".into(),
        accent: "#8aadf4".into(),
        red: "#ed8796".into(),
    }
}

fn dracula() -> Theme {
    Theme {
        name: "dracula".into(),
        foreground: "#f8f8f2".into(),
        background: "#282a36".into(),
        palette: [
            "#21222c", "#ff5555", "#50fa7b", "#f1fa8c", "#bd93f9", "#ff79c6", "#8be9fd", "#f8f8f2",
            "#6272a4", "#ff6e6e", "#69ff94", "#ffffa5", "#d6acff", "#ff92df", "#a4ffff", "#ffffff",
        ]
        .map(Into::into),
        surface0: "#21222c".into(),
        surface1: "#2d2f3f".into(),
        surface2: "#44475a".into(),
        overlay0: "#6272a4".into(),
        text: "#f8f8f2".into(),
        subtext0: "#6272a4".into(),
        subtext1: "#bfbfbf".into(),
        accent: "#bd93f9".into(),
        red: "#ff5555".into(),
    }
}

fn nord() -> Theme {
    Theme {
        name: "nord".into(),
        foreground: "#d8dee9".into(),
        background: "#2e3440".into(),
        palette: [
            "#3b4252", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#88c0d0", "#e5e9f0",
            "#4c566a", "#bf616a", "#a3be8c", "#ebcb8b", "#81a1c1", "#b48ead", "#8fbcbb", "#eceff4",
        ]
        .map(Into::into),
        surface0: "#272c36".into(),
        surface1: "#353b49".into(),
        surface2: "#3b4252".into(),
        overlay0: "#4c566a".into(),
        text: "#d8dee9".into(),
        subtext0: "#7b88a1".into(),
        subtext1: "#e5e9f0".into(),
        accent: "#88c0d0".into(),
        red: "#bf616a".into(),
    }
}

fn tokyo_night() -> Theme {
    Theme {
        name: "tokyo-night".into(),
        foreground: "#a9b1d6".into(),
        background: "#1a1b26".into(),
        palette: [
            "#32344a", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#ad8ee6", "#449dab", "#787c99",
            "#444b6a", "#ff7a93", "#b9f27c", "#ff9e64", "#7da6ff", "#bb9af7", "#0db9d7", "#acb0d0",
        ]
        .map(Into::into),
        surface0: "#16161e".into(),
        surface1: "#1f2335".into(),
        surface2: "#292e42".into(),
        overlay0: "#3b4261".into(),
        text: "#a9b1d6".into(),
        subtext0: "#565f89".into(),
        subtext1: "#787c99".into(),
        accent: "#7aa2f7".into(),
        red: "#f7768e".into(),
    }
}

fn gruvbox_dark() -> Theme {
    Theme {
        name: "gruvbox-dark".into(),
        foreground: "#ebdbb2".into(),
        background: "#282828".into(),
        palette: [
            "#282828", "#cc241d", "#98971a", "#d79921", "#458588", "#b16286", "#689d6a", "#a89984",
            "#928374", "#fb4934", "#b8bb26", "#fabd2f", "#83a598", "#d3869b", "#8ec07c", "#ebdbb2",
        ]
        .map(Into::into),
        surface0: "#1d2021".into(),
        surface1: "#32302f".into(),
        surface2: "#3c3836".into(),
        overlay0: "#504945".into(),
        text: "#ebdbb2".into(),
        subtext0: "#928374".into(),
        subtext1: "#a89984".into(),
        accent: "#83a598".into(),
        red: "#fb4934".into(),
    }
}

fn one_dark() -> Theme {
    Theme {
        name: "one-dark".into(),
        foreground: "#abb2bf".into(),
        background: "#282c34".into(),
        palette: [
            "#282c34", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#abb2bf",
            "#545862", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#c8ccd4",
        ]
        .map(Into::into),
        surface0: "#21252b".into(),
        surface1: "#2c313c".into(),
        surface2: "#333842".into(),
        overlay0: "#4b5263".into(),
        text: "#abb2bf".into(),
        subtext0: "#636d83".into(),
        subtext1: "#828997".into(),
        accent: "#61afef".into(),
        red: "#e06c75".into(),
    }
}

fn solarized_dark() -> Theme {
    Theme {
        name: "solarized-dark".into(),
        foreground: "#839496".into(),
        background: "#002b36".into(),
        palette: [
            "#073642", "#dc322f", "#859900", "#b58900", "#268bd2", "#d33682", "#2aa198", "#eee8d5",
            "#002b36", "#cb4b16", "#586e75", "#657b83", "#839496", "#6c71c4", "#93a1a1", "#fdf6e3",
        ]
        .map(Into::into),
        surface0: "#001e26".into(),
        surface1: "#003847".into(),
        surface2: "#073642".into(),
        overlay0: "#586e75".into(),
        text: "#839496".into(),
        subtext0: "#657b83".into(),
        subtext1: "#93a1a1".into(),
        accent: "#268bd2".into(),
        red: "#dc322f".into(),
    }
}
