use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use gtk4::glib;
use gtk4::prelude::*;

use copad_core::action_registry::ActionRegistry;
use copad_core::protocol::Request;
use copad_daemon::socket::{LEGACY_DISPATCH_METHODS, SocketCommand};

use crate::panel::Panel;
use crate::tabs::TabManager;

// Actions whose empty-param dispatch has irreversible side effects on
// the user's session. `tab.close` is the only one today: empty params
// close the active panel, which discards a live terminal's scrollback
// + running process. Confirmed before dispatch with a Cancel-default
// AlertDialog so a stray Enter (after the palette's Enter closes it)
// doesn't fall through into the destructive action.
const DESTRUCTIVE_ACTIONS: &[&str] = &["tab.close"];

pub fn open(
    window: &gtk4::ApplicationWindow,
    actions: &Arc<ActionRegistry>,
    dispatch_tx: &Sender<SocketCommand>,
    mgr: &Rc<TabManager>,
) {
    let all = collect_actions(actions);
    let restore_focus = mgr.active_panel();

    let palette = gtk4::Window::builder()
        .transient_for(window)
        .modal(true)
        .default_width(520)
        .default_height(400)
        .title("Command palette")
        .resizable(false)
        .build();

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
    content.set_margin_top(8);
    content.set_margin_bottom(8);
    content.set_margin_start(8);
    content.set_margin_end(8);

    let search = gtk4::SearchEntry::new();
    search.set_placeholder_text(Some("Type to filter actions…"));
    content.append(&search);

    let scroll = gtk4::ScrolledWindow::new();
    scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scroll.set_vexpand(true);

    let list = gtk4::ListBox::new();
    list.set_selection_mode(gtk4::SelectionMode::Browse);
    list.set_activate_on_single_click(false);
    scroll.set_child(Some(&list));
    content.append(&scroll);

    palette.set_child(Some(&content));

    let all_for_filter = all.clone();
    let list_for_filter = list.clone();
    let populate = move |query: &str| {
        while let Some(child) = list_for_filter.first_child() {
            list_for_filter.remove(&child);
        }
        for name in filter_actions(&all_for_filter, query) {
            let row = gtk4::Label::new(Some(&name));
            row.set_xalign(0.0);
            row.set_margin_start(8);
            row.set_margin_end(8);
            row.set_margin_top(4);
            row.set_margin_bottom(4);
            let lb_row = gtk4::ListBoxRow::new();
            lb_row.set_child(Some(&row));
            unsafe {
                lb_row.set_data("action-name", name.clone());
            }
            list_for_filter.append(&lb_row);
        }
        if let Some(first) = list_for_filter.row_at_index(0) {
            list_for_filter.select_row(Some(&first));
        }
    };
    populate("");

    let populate_for_change = populate.clone();
    search.connect_search_changed(move |entry| {
        populate_for_change(entry.text().as_str());
    });

    // Activate (Enter on the search entry): dispatch the currently
    // selected row.
    let palette_for_activate = palette.clone();
    let dispatch_tx_for_activate = dispatch_tx.clone();
    let restore_for_activate = restore_focus.clone();
    let window_for_activate = window.clone();
    let list_for_activate = list.clone();
    search.connect_activate(move |_| {
        let Some(row) = list_for_activate.selected_row() else {
            return;
        };
        let Some(name) = (unsafe { row.data::<String>("action-name") }) else {
            return;
        };
        let action_name = unsafe { name.as_ref() }.clone();
        on_select(
            &palette_for_activate,
            &window_for_activate,
            &dispatch_tx_for_activate,
            &restore_for_activate,
            &action_name,
        );
    });

    // Mouse double-click activation: same dispatch path.
    let palette_for_row = palette.clone();
    let dispatch_tx_for_row = dispatch_tx.clone();
    let restore_for_row = restore_focus.clone();
    let window_for_row = window.clone();
    list.connect_row_activated(move |_, row| {
        let Some(name) = (unsafe { row.data::<String>("action-name") }) else {
            return;
        };
        let action_name = unsafe { name.as_ref() }.clone();
        on_select(
            &palette_for_row,
            &window_for_row,
            &dispatch_tx_for_row,
            &restore_for_row,
            &action_name,
        );
    });

    // Key controller for Esc and Up/Down navigation while typing.
    // Capture phase so we run BEFORE the SearchEntry's built-in
    // stop-search handler eats Escape.
    let key = gtk4::EventControllerKey::new();
    key.set_propagation_phase(gtk4::PropagationPhase::Capture);
    let palette_for_key = palette.clone();
    let restore_for_key = restore_focus.clone();
    let list_for_key = list.clone();
    key.connect_key_pressed(move |_, keyval, _, _| match keyval {
        gtk4::gdk::Key::Escape => {
            palette_for_key.close();
            if let Some(panel) = &restore_for_key {
                panel.grab_focus();
            }
            glib::Propagation::Stop
        }
        gtk4::gdk::Key::Down => {
            move_selection(&list_for_key, 1);
            glib::Propagation::Stop
        }
        gtk4::gdk::Key::Up => {
            move_selection(&list_for_key, -1);
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
    palette.add_controller(key);

    // SearchEntry's own "stop-search" (emitted on Esc even after our
    // capture-phase handler, on some GTK versions) — backup close path
    // so the palette can never get stuck open.
    let palette_for_stop = palette.clone();
    let restore_for_stop = restore_focus.clone();
    search.connect_stop_search(move |_| {
        palette_for_stop.close();
        if let Some(panel) = &restore_for_stop {
            panel.grab_focus();
        }
    });

    palette.present();
    search.grab_focus();
}

fn on_select(
    palette: &gtk4::Window,
    window: &gtk4::ApplicationWindow,
    dispatch_tx: &Sender<SocketCommand>,
    restore: &Option<Rc<crate::panel::PanelVariant>>,
    action: &str,
) {
    palette.close();
    if DESTRUCTIVE_ACTIONS.contains(&action) {
        confirm_then_dispatch(window, dispatch_tx, restore.clone(), action.to_string());
    } else {
        dispatch_action(dispatch_tx, action);
        if let Some(panel) = restore {
            panel.grab_focus();
        }
    }
}

fn confirm_then_dispatch(
    window: &gtk4::ApplicationWindow,
    dispatch_tx: &Sender<SocketCommand>,
    restore: Option<Rc<crate::panel::PanelVariant>>,
    action: String,
) {
    // Cancel is index 0 = default button (a stray Enter cancels rather
    // than confirms). Confirm is index 1, also the explicit cancel
    // affordance (Esc) — separate from default to keep the keyboard UX
    // intuitive.
    let dialog = gtk4::AlertDialog::builder()
        .modal(true)
        .message(format!("Confirm action: {action}"))
        .detail("This is a destructive action. Cancel and re-run if unintended.")
        .buttons(["Cancel", "Confirm"])
        .default_button(0)
        .cancel_button(0)
        .build();
    let dispatch_tx = dispatch_tx.clone();
    dialog.choose(Some(window), gtk4::gio::Cancellable::NONE, move |result| {
        if matches!(result, Ok(1)) {
            dispatch_action(&dispatch_tx, &action);
        }
        if let Some(panel) = &restore {
            panel.grab_focus();
        }
    });
}

fn dispatch_action(dispatch_tx: &Sender<SocketCommand>, action: &str) {
    let (reply_tx, _reply_rx) = std::sync::mpsc::channel();
    let cmd = SocketCommand {
        request: Request::new(
            uuid::Uuid::new_v4().to_string(),
            action,
            serde_json::json!({}),
        ),
        reply: reply_tx,
        silent_completion: false,
    };
    let _ = dispatch_tx.send(cmd);
}

fn move_selection(list: &gtk4::ListBox, delta: i32) {
    // Crucially: do NOT call `row.grab_focus()`. The SearchEntry must
    // keep focus across Up/Down so the user can keep typing to refine
    // the filter. Auto-scrolling to keep the selection in view is a v2
    // follow-up — for ≤~10 rows the visible window covers the common
    // case; scrolling matters only when filter narrows past the visible
    // window during arrow navigation.
    let Some(current) = list.selected_row() else {
        if let Some(row) = list.row_at_index(0) {
            list.select_row(Some(&row));
        }
        return;
    };
    let idx = current.index();
    let next = idx + delta;
    if let Some(row) = list.row_at_index(next) {
        list.select_row(Some(&row));
    }
}

fn collect_actions(actions: &Arc<ActionRegistry>) -> Vec<String> {
    let mut out = actions.names();
    for legacy in LEGACY_DISPATCH_METHODS {
        if !out.contains(&(*legacy).to_string()) {
            out.push((*legacy).to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

pub fn filter_actions(entries: &[String], query: &str) -> Vec<String> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return entries.to_vec();
    }
    entries
        .iter()
        .filter(|name| name.to_ascii_lowercase().contains(&q))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<String> {
        vec![
            "background.set".to_string(),
            "system.log".to_string(),
            "system.ping".to_string(),
            "tab.new".to_string(),
            "tab.close".to_string(),
        ]
    }

    #[test]
    fn filter_empty_returns_all() {
        let all = sample();
        assert_eq!(filter_actions(&all, ""), all);
    }

    #[test]
    fn filter_substring_case_insensitive() {
        let all = sample();
        let got = filter_actions(&all, "SYSTEM");
        assert_eq!(
            got,
            vec!["system.log".to_string(), "system.ping".to_string()]
        );
    }

    #[test]
    fn filter_no_match_returns_empty() {
        let all = sample();
        assert!(filter_actions(&all, "nope").is_empty());
    }

    #[test]
    fn filter_trim_whitespace() {
        let all = sample();
        assert_eq!(filter_actions(&all, "  tab  ").len(), 2);
    }

    #[test]
    fn destructive_list_only_tab_close() {
        assert_eq!(DESTRUCTIVE_ACTIONS, &["tab.close"]);
    }
}
