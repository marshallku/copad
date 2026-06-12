use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, gio};

use crate::window::CopadWindow;

const APP_ID: &str = "com.marshall.copad";

pub fn run() {
    let app = Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();

    app.connect_startup(|_| {
        if let Some(settings) = gtk4::Settings::default() {
            settings.set_gtk_application_prefer_dark_theme(true);
        }
        // Tell GTK which hicolor icon to use for window/taskbar art.
        // Belt-and-suspenders alongside the desktop entry: the entry
        // is named com.marshall.copad.desktop (matches application_id
        // so Wayland compositors map windows ↔ launcher) and points at
        // Icon=copad, but compositors that haven't read the entry
        // yet (e.g. before StartupNotify lands) still need GTK to
        // tell them which icon to paint.
        gtk4::Window::set_default_icon_name("copad");
    });

    app.connect_activate(|app| {
        let config = load_startup_config();
        let window = CopadWindow::new(app, &config);
        window.present();

        // SIGTERM/SIGINT close windows so `connect_destroy` runs
        // `ServiceSupervisor::shutdown_all` — without this, default
        // disposition would kill GTK before that callback fires and
        // orphan plugin children. SIGKILL/segfault are caught by
        // `PR_SET_PDEATHSIG` armed in each plugin's `pre_exec`.
        let signal_app = app.downgrade();
        glib::unix_signal_add_local(libc::SIGTERM, move || {
            if let Some(app) = signal_app.upgrade() {
                eprintln!("[copad] SIGTERM received — closing windows for graceful shutdown");
                close_all_windows(&app);
            }
            glib::ControlFlow::Continue
        });
        let signal_app = app.downgrade();
        glib::unix_signal_add_local(libc::SIGINT, move || {
            if let Some(app) = signal_app.upgrade() {
                eprintln!("[copad] SIGINT received — closing windows for graceful shutdown");
                close_all_windows(&app);
            }
            glib::ControlFlow::Continue
        });
    });

    app.run();
}

/// Load config for first window start. A parse error used to be swallowed by `unwrap_or_default()`,
/// silently dropping the entire user config — and a desktop-entry launch hides stderr, so the user
/// had no signal at all. Now we fall back to defaults loudly: stderr for terminal launches plus a
/// desktop toast that survives a GUI launch and names the offending file.
fn load_startup_config() -> copad_core::config::CopadConfig {
    let (config, warning) = copad_core::config::CopadConfig::load_or_default_warning();
    if let Some(warning) = warning {
        eprintln!("[copad] config error — {warning}");
        if let Some(notifier) = copad_core::notifier::platform_notifier() {
            let _ = notifier.notify(
                "copad config error",
                &format!("Using built-in defaults — {warning}"),
                copad_core::notifier::Level::Error,
            );
        }
    }
    config
}

/// `window.close()` (not `app.quit()`) so destroy signals fire — the
/// supervisor's `shutdown_all` hook is wired to window destroy.
fn close_all_windows(app: &Application) {
    for w in app.windows() {
        w.close();
    }
}
