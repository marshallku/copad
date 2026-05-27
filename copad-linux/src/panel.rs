use crate::plugin_panel::PluginPanel;
use crate::terminal::TerminalPanel;
use crate::webview::WebViewPanel;

pub trait Panel {
    fn widget(&self) -> &gtk4::Widget;
    fn title(&self) -> String;
    fn panel_type(&self) -> &str;
    fn grab_focus(&self);
    fn id(&self) -> &str;
}

pub enum PanelVariant {
    Terminal(TerminalPanel),
    WebView(WebViewPanel),
    Plugin(PluginPanel),
}

impl Panel for PanelVariant {
    fn widget(&self) -> &gtk4::Widget {
        match self {
            PanelVariant::Terminal(p) => p.widget(),
            PanelVariant::WebView(p) => p.widget(),
            PanelVariant::Plugin(p) => p.widget(),
        }
    }

    fn title(&self) -> String {
        match self {
            PanelVariant::Terminal(p) => p.title(),
            PanelVariant::WebView(p) => p.title(),
            PanelVariant::Plugin(p) => p.title(),
        }
    }

    fn panel_type(&self) -> &str {
        match self {
            PanelVariant::Terminal(p) => p.panel_type(),
            PanelVariant::WebView(p) => p.panel_type(),
            PanelVariant::Plugin(p) => p.panel_type(),
        }
    }

    fn grab_focus(&self) {
        match self {
            PanelVariant::Terminal(p) => p.grab_focus(),
            PanelVariant::WebView(p) => p.grab_focus(),
            PanelVariant::Plugin(p) => p.grab_focus(),
        }
    }

    fn id(&self) -> &str {
        match self {
            PanelVariant::Terminal(p) => p.id(),
            PanelVariant::WebView(p) => p.id(),
            PanelVariant::Plugin(p) => p.id(),
        }
    }
}

impl PanelVariant {
    pub fn as_terminal(&self) -> Option<&TerminalPanel> {
        match self {
            PanelVariant::Terminal(p) => Some(p),
            _ => None,
        }
    }

    pub fn as_webview(&self) -> Option<&WebViewPanel> {
        match self {
            PanelVariant::WebView(p) => Some(p),
            _ => None,
        }
    }
}
