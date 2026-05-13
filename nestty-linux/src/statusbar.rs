use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gtk4::glib;
use gtk4::prelude::*;

use nestty_core::config::NesttyConfig;
use nestty_core::plugin::LoadedPlugin;
use nestty_core::theme::Theme;

struct ModuleHandle {
    label: gtk4::Label,
    exec: String,
    interval: u64,
    plugin_dir: std::path::PathBuf,
    socket_path: String,
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

/// Run a module's exec command in a thread, send result back via channel.
fn run_module_exec(
    exec: &str,
    plugin_dir: &std::path::Path,
    socket_path: &str,
) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let exec = exec.to_string();
    let dir = plugin_dir.to_path_buf();
    let sock = socket_path.to_string();

    std::thread::spawn(move || {
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(&exec)
            .current_dir(&dir)
            .env("NESTTY_SOCKET", &sock)
            .env("NESTTY_PLUGIN_DIR", &dir)
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let _ = tx.send(stdout);
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                eprintln!("[nestty] statusbar module error: {stderr}");
                let _ = tx.send(String::new());
            }
            Err(e) => {
                eprintln!("[nestty] statusbar module exec failed: {e}");
                let _ = tx.send(String::new());
            }
        }
    });

    rx
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
        let socket_path = nestty_core::paths::gui_socket_path(std::process::id())
            .to_string_lossy()
            .into_owned();

        apply_theme_css(&theme, height, &config.statusbar.position);

        let mut left_entries: Vec<ModuleEntry> = Vec::new();
        let mut center_entries: Vec<ModuleEntry> = Vec::new();
        let mut right_entries: Vec<ModuleEntry> = Vec::new();

        let modules: Rc<RefCell<Vec<ModuleHandle>>> = Rc::new(RefCell::new(Vec::new()));
        let labels: Rc<RefCell<HashMap<String, gtk4::Label>>> =
            Rc::new(RefCell::new(HashMap::new()));

        for plugin in plugins {
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
                    exec: module.exec.clone(),
                    interval: module.interval,
                    plugin_dir: plugin.dir.clone(),
                    socket_path: socket_path.clone(),
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

        // Re-collect modules with updated socket/plugin info
        let socket_path = nestty_core::paths::gui_socket_path(std::process::id())
            .to_string_lossy()
            .into_owned();

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

        for plugin in plugins {
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
                    exec: module.exec.clone(),
                    interval: module.interval,
                    plugin_dir: plugin.dir.clone(),
                    socket_path: socket_path.clone(),
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
            "[nestty] statusbar: module {} exec={} interval={}s",
            module.label.widget_name(),
            module.exec,
            module.interval,
        );
        let ctx = ModuleRunCtx {
            label: module.label.clone(),
            exec: module.exec.clone(),
            plugin_dir: module.plugin_dir.clone(),
            socket_path: module.socket_path.clone(),
            interval: module.interval,
        };
        run_and_schedule(ctx);
    }
}

#[derive(Clone)]
struct ModuleRunCtx {
    label: gtk4::Label,
    exec: String,
    plugin_dir: std::path::PathBuf,
    socket_path: String,
    interval: u64,
}

fn run_and_schedule(ctx: ModuleRunCtx) {
    let rx = run_module_exec(&ctx.exec, &ctx.plugin_dir, &ctx.socket_path);

    glib::timeout_add_local(Duration::from_millis(50), move || {
        match rx.try_recv() {
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

                // Schedule next run
                let next = ctx.clone();
                glib::timeout_add_local_once(Duration::from_secs(ctx.interval), move || {
                    run_and_schedule(next);
                });

                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        }
    });
}
