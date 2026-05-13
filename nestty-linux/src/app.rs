use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, gio};

use crate::window::NesttyWindow;

const APP_ID: &str = "com.marshall.nestty";

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
        // is named com.marshall.nestty.desktop (matches application_id
        // so Wayland compositors map windows ↔ launcher) and points at
        // Icon=nestty, but compositors that haven't read the entry
        // yet (e.g. before StartupNotify lands) still need GTK to
        // tell them which icon to paint.
        gtk4::Window::set_default_icon_name("nestty");
    });

    app.connect_activate(|app| {
        let config = nestty_core::config::NesttyConfig::load().unwrap_or_default();
        let window = NesttyWindow::new(app, &config);
        window.present();

        // Ctrl-C in the foreground or `kill <pid>` from another shell
        // would otherwise kill the GTK process *without* running the
        // window's `connect_destroy` callback, leaving the plugin
        // subprocesses orphaned to init. Handle SIGTERM and SIGINT
        // by closing all windows — that fires connect_destroy →
        // ServiceSupervisor::shutdown_all() through the existing
        // graceful path.
        //
        // SIGKILL / segfault: re-armed in step 2c of the daemon-first
        // migration via the long-lived `nestty-spawner` thread (see
        // `nestty-daemon::service_supervisor`). Every plugin spawn now
        // forks from the spawner, so `PR_SET_PDEATHSIG(SIGTERM)` armed
        // in the child's `pre_exec` fires only when nestty actually
        // dies. Cooperative shutdown still goes through `shutdown_all`
        // (this handler triggers it via `close_all_windows →
        // connect_destroy`); pdeathsig is the crash-safety net.
        let signal_app = app.downgrade();
        glib::unix_signal_add_local(libc::SIGTERM, move || {
            if let Some(app) = signal_app.upgrade() {
                eprintln!("[nestty] SIGTERM received — closing windows for graceful shutdown");
                close_all_windows(&app);
            }
            glib::ControlFlow::Continue
        });
        let signal_app = app.downgrade();
        glib::unix_signal_add_local(libc::SIGINT, move || {
            if let Some(app) = signal_app.upgrade() {
                eprintln!("[nestty] SIGINT received — closing windows for graceful shutdown");
                close_all_windows(&app);
            }
            glib::ControlFlow::Continue
        });
    });

    app.run();
}

/// `window.close()` (not `app.quit()`) so destroy signals fire — the
/// supervisor's `shutdown_all` hook is wired to window destroy.
fn close_all_windows(app: &Application) {
    for w in app.windows() {
        w.close();
    }
}
