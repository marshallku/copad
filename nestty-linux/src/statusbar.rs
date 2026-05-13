use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;

use nestty_core::config::NesttyConfig;
use nestty_core::plugin::LoadedPlugin;
use nestty_core::protocol::{Request, Response};
use nestty_core::theme::Theme;
use serde_json::json;

struct ModuleHandle {
    label: gtk4::Label,
    plugin: String,
    module: String,
    interval: u64,
}

pub struct StatusBar {
    pub container: gtk4::Box,
    bar: gtk4::Box,
    modules: Rc<RefCell<Vec<ModuleHandle>>>,
    /// Label widgets keyed by dom_id for reload lookups
    labels: Rc<RefCell<HashMap<String, gtk4::Label>>>,
}

/// JSON `{text, tooltip?}` if it parses, otherwise the raw trimmed string.
fn parse_output(output: &str) -> (String, Option<String>) {
    let trimmed = output.trim();
    if trimmed.starts_with('{')
        && let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
    {
        let text = val["text"].as_str().unwrap_or(trimmed).to_string();
        let tooltip = val["tooltip"].as_str().map(|s| s.to_string());
        return (text, tooltip);
    }
    (trimmed.to_string(), None)
}

/// Fire a `_module.run` RPC to nesttyd and yield the module's stdout
/// through a channel. Daemon owns the shell exec now; the GUI keeps
/// the per-module timer and label update. On connect/transport failure
/// (no daemon, slow daemon) the channel receives an empty string —
/// same UX as the legacy "module errored → blank label".
fn run_module_via_daemon(plugin: &str, module: &str) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let plugin = plugin.to_string();
    let module = module.to_string();
    std::thread::spawn(move || {
        let _ = tx.send(invoke_module(&plugin, &module).unwrap_or_default());
    });
    rx
}

fn invoke_module(plugin: &str, module: &str) -> Option<String> {
    // `daemon_socket_path()` (vs `socket_path()`) skips an inherited
    // per-GUI NESTTY_SOCKET override and refuses untrusted runtime dirs
    // — same guard `gui_client` uses. None on no-daemon → blank label.
    let socket_path = nestty_core::paths::daemon_socket_path()?;
    let stream = UnixStream::connect(&socket_path).ok()?;
    // Module ticks must not stall: bound both directions on the
    // daemon's MODULE_RUN_TIMEOUT (8 s) + a small margin.
    let timeout = Duration::from_secs(10);
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    let mut write = stream.try_clone().ok()?;
    let req = Request::new(
        "sb",
        "_module.run",
        json!({ "plugin": plugin, "module": module }),
    );
    let line = serde_json::to_string(&req).ok()?;
    writeln!(write, "{line}").ok()?;
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader.read_line(&mut buf).ok()?;
    let resp: Response = serde_json::from_str(buf.trim()).ok()?;
    if !resp.ok {
        return None;
    }
    resp.result?
        .get("stdout")
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Separator goes on the edge facing the notebook (`top` → bottom-edge,
/// `bottom` → top-edge); with the transparent bar bg a wrong-edge border
/// would float against the window frame instead of dividing the UI.
fn apply_theme_css(theme: &Theme, height: u32, position: &str) {
    let border_edge = if position == "top" {
        "border-bottom"
    } else {
        "border-top"
    };
    let css = format!(
        r#"
        .nestty-statusbar {{
            background-color: transparent;
            {border_edge}: 1px solid {overlay0};
            min-height: {height}px;
            padding: 0 10px;
        }}
        .nestty-statusbar label {{
            color: {subtext0};
            font-family: system-ui, -apple-system, sans-serif;
            font-size: 12px;
        }}
        "#,
        overlay0 = theme.overlay0,
        subtext0 = theme.subtext0,
        height = height,
    );

    let provider = gtk4::CssProvider::new();
    provider.load_from_string(&css);
    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().unwrap(),
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
}

/// Sorted module entries for a section
struct ModuleEntry {
    order: i32,
    label: gtk4::Label,
}

fn build_section(entries: &mut [ModuleEntry]) -> gtk4::Box {
    entries.sort_by_key(|e| e.order);
    let section = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    for entry in entries.iter() {
        section.append(&entry.label);
    }
    section
}

impl StatusBar {
    pub fn new(config: &NesttyConfig, plugins: &[LoadedPlugin]) -> Self {
        let theme = Theme::by_name(&config.theme.name).unwrap_or_default();
        let height = config.statusbar.height;

        apply_theme_css(&theme, height, &config.statusbar.position);

        let mut left_entries: Vec<ModuleEntry> = Vec::new();
        let mut center_entries: Vec<ModuleEntry> = Vec::new();
        let mut right_entries: Vec<ModuleEntry> = Vec::new();

        let modules: Rc<RefCell<Vec<ModuleHandle>>> = Rc::new(RefCell::new(Vec::new()));
        let labels: Rc<RefCell<HashMap<String, gtk4::Label>>> =
            Rc::new(RefCell::new(HashMap::new()));

        // Dedup duplicate plugin names so the bar reflects the same
        // winner as daemon `_module.run` resolution.
        // rev → filter (sorted-last wins) → rev again to restore the
        // sorted slice's original traversal order. Without the second
        // rev, equal-`order` modules would render reversed across the
        // plugin list.
        let mut seen = std::collections::HashSet::new();
        let mut winners: Vec<&LoadedPlugin> = plugins
            .iter()
            .rev()
            .filter(|p| seen.insert(p.manifest.plugin.name.as_str()))
            .collect();
        winners.reverse();

        for plugin in winners.iter() {
            for module in &plugin.manifest.modules {
                let dom_id = format!("mod-{}-{}", plugin.manifest.plugin.name, module.name);

                let label = gtk4::Label::new(Some("..."));
                label.set_widget_name(&dom_id);

                let entry = ModuleEntry {
                    order: module.order,
                    label: label.clone(),
                };

                match module.position.as_str() {
                    "left" => left_entries.push(entry),
                    "center" => center_entries.push(entry),
                    _ => right_entries.push(entry),
                }

                labels.borrow_mut().insert(dom_id.clone(), label.clone());
                modules.borrow_mut().push(ModuleHandle {
                    label,
                    plugin: plugin.manifest.plugin.name.clone(),
                    module: module.name.clone(),
                    interval: module.interval,
                });
            }
        }

        eprintln!(
            "[nestty] statusbar modules: left={}, center={}, right={}",
            left_entries.len(),
            center_entries.len(),
            right_entries.len()
        );

        let left_box = build_section(&mut left_entries);
        left_box.set_halign(gtk4::Align::Start);
        left_box.set_hexpand(true);

        let center_box = build_section(&mut center_entries);
        center_box.set_halign(gtk4::Align::Center);

        let right_box = build_section(&mut right_entries);
        right_box.set_halign(gtk4::Align::End);
        right_box.set_hexpand(true);

        let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        bar.add_css_class("nestty-statusbar");
        bar.set_hexpand(true);
        bar.set_vexpand(false);
        bar.set_valign(gtk4::Align::Center);
        bar.append(&left_box);
        bar.append(&center_box);
        bar.append(&right_box);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        container.set_hexpand(true);
        container.set_vexpand(false);
        container.append(&bar);

        let has_modules = !modules.borrow().is_empty();
        if !config.statusbar.enabled || !has_modules {
            container.set_visible(false);
        }

        // Schedule module execution
        if has_modules {
            schedule_modules(&modules);
        }

        Self {
            container,
            bar,
            modules,
            labels,
        }
    }

    pub fn set_visible(&self, visible: bool) {
        self.container.set_visible(visible);
    }

    pub fn is_visible(&self) -> bool {
        self.container.is_visible()
    }

    pub fn toggle(&self) -> bool {
        let new_visible = !self.is_visible();
        self.set_visible(new_visible);
        new_visible
    }

    pub fn reload(&self, config: &NesttyConfig, plugins: &[LoadedPlugin]) {
        let theme = Theme::by_name(&config.theme.name).unwrap_or_default();
        apply_theme_css(&theme, config.statusbar.height, &config.statusbar.position);

        // Clear existing labels from the bar sections
        let mut child = self.bar.first_child();
        while let Some(section) = child {
            child = section.next_sibling();
            if let Some(bx) = section.downcast_ref::<gtk4::Box>() {
                let mut label_child = bx.first_child();
                while let Some(lc) = label_child {
                    label_child = lc.next_sibling();
                    bx.remove(&lc);
                }
            }
        }

        let mut left_entries: Vec<ModuleEntry> = Vec::new();
        let mut center_entries: Vec<ModuleEntry> = Vec::new();
        let mut right_entries: Vec<ModuleEntry> = Vec::new();

        self.modules.borrow_mut().clear();
        self.labels.borrow_mut().clear();

        // rev → filter (sorted-last wins) → rev again to restore the
        // sorted slice's original traversal order. Without the second
        // rev, equal-`order` modules would render reversed across the
        // plugin list.
        let mut seen = std::collections::HashSet::new();
        let mut winners: Vec<&LoadedPlugin> = plugins
            .iter()
            .rev()
            .filter(|p| seen.insert(p.manifest.plugin.name.as_str()))
            .collect();
        winners.reverse();

        for plugin in winners.iter() {
            for module in &plugin.manifest.modules {
                let dom_id = format!("mod-{}-{}", plugin.manifest.plugin.name, module.name);

                let label = gtk4::Label::new(Some("..."));
                label.set_widget_name(&dom_id);

                let entry = ModuleEntry {
                    order: module.order,
                    label: label.clone(),
                };

                match module.position.as_str() {
                    "left" => left_entries.push(entry),
                    "center" => center_entries.push(entry),
                    _ => right_entries.push(entry),
                }

                self.labels
                    .borrow_mut()
                    .insert(dom_id.clone(), label.clone());
                self.modules.borrow_mut().push(ModuleHandle {
                    label,
                    plugin: plugin.manifest.plugin.name.clone(),
                    module: module.name.clone(),
                    interval: module.interval,
                });
            }
        }

        // Re-populate sections (bar has 3 children: left, center, right)
        let sections: Vec<gtk4::Box> = {
            let mut v = Vec::new();
            let mut child = self.bar.first_child();
            while let Some(c) = child {
                child = c.next_sibling();
                if let Some(bx) = c.downcast_ref::<gtk4::Box>() {
                    v.push(bx.clone());
                }
            }
            v
        };

        if sections.len() == 3 {
            left_entries.sort_by_key(|e| e.order);
            center_entries.sort_by_key(|e| e.order);
            right_entries.sort_by_key(|e| e.order);

            for entry in &left_entries {
                sections[0].append(&entry.label);
            }
            for entry in &center_entries {
                sections[1].append(&entry.label);
            }
            for entry in &right_entries {
                sections[2].append(&entry.label);
            }
        }

        let has_modules = !self.modules.borrow().is_empty();
        self.container
            .set_visible(config.statusbar.enabled && has_modules);

        if has_modules {
            schedule_modules(&self.modules);
        }
    }
}

fn schedule_modules(modules: &Rc<RefCell<Vec<ModuleHandle>>>) {
    let modules_ref = modules.borrow();
    eprintln!(
        "[nestty] statusbar: scheduling {} modules",
        modules_ref.len()
    );
    for module in modules_ref.iter() {
        eprintln!(
            "[nestty] statusbar: module {}.{} interval={}s",
            module.plugin, module.module, module.interval,
        );
        let ctx = ModuleRunCtx {
            label: module.label.clone(),
            plugin: module.plugin.clone(),
            module: module.module.clone(),
            interval: module.interval,
        };
        run_and_schedule(ctx);
    }
}

#[derive(Clone)]
struct ModuleRunCtx {
    label: gtk4::Label,
    plugin: String,
    module: String,
    interval: u64,
}

fn run_and_schedule(ctx: ModuleRunCtx) {
    let rx = run_module_via_daemon(&ctx.plugin, &ctx.module);

    glib::timeout_add_local(Duration::from_millis(50), move || match rx.try_recv() {
        Ok(output) => {
            let (text, tooltip) = parse_output(&output);
            eprintln!(
                "[nestty] statusbar: {} -> {:?}",
                ctx.label.widget_name(),
                text
            );

            ctx.label.set_text(&text);
            if let Some(tt) = &tooltip {
                ctx.label.set_tooltip_text(Some(tt));
            }

            let next = ctx.clone();
            glib::timeout_add_local_once(Duration::from_secs(ctx.interval), move || {
                run_and_schedule(next);
            });

            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
    });
}
