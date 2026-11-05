//! Runs one scenario page of `scripts/e2e-graph-page.sh` in WebKitGTK with a real `comux` message
//! handler (the Linux twin of `scripts/lib/webkit-page-runner.swift`).
//!
//!   cargo run -q -p copad-linux --example graph_page_runner -- <scenario.html>
//!
//! Prints the scenario's result JSON with `posts` replaced by the messages the page really posted.
//! Needs a display (Broadway works).
use gtk4::glib;
use gtk4::prelude::*;
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use webkit6::prelude::*;

fn main() -> glib::ExitCode {
    let path = std::env::args()
        .nth(1)
        .expect("usage: graph_page_runner <scenario.html>");
    let html = std::fs::read_to_string(&path).expect("read scenario");
    let app = gtk4::Application::builder()
        .application_id("com.marshall.copad.graph-page-runner")
        .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_activate(move |app| {
        let ucm = webkit6::UserContentManager::new();
        ucm.register_script_message_handler("comux", None);
        let posts: Rc<RefCell<Vec<Value>>> = Rc::default();
        let sink = posts.clone();
        ucm.connect_script_message_received(Some("comux"), move |_, v| {
            if let Some(j) = v.to_json(0)
                && let Ok(val) = serde_json::from_str(&j)
            {
                sink.borrow_mut().push(val);
            }
        });
        let web = webkit6::WebView::builder()
            .user_content_manager(&ucm)
            .build();
        let window = gtk4::ApplicationWindow::builder()
            .application(app)
            .default_width(900)
            .default_height(640)
            .child(&web)
            .build();
        window.present();
        web.load_html(&html, None);
        let started = Instant::now();
        let app = app.clone();
        glib::timeout_add_local(Duration::from_millis(200), move || {
            let posts = posts.clone();
            let app2 = app.clone();
            web.evaluate_javascript(
                "(document.getElementById('e2e-out') || {}).textContent || ''",
                None,
                None,
                gtk4::gio::Cancellable::NONE,
                move |r| {
                    let text = r.map(|v| v.to_str().to_string()).unwrap_or_default();
                    let (Some(a), Some(b)) = (text.find("@@OUT@@"), text.find("@@END@@")) else {
                        return;
                    };
                    if let Ok(mut out) = serde_json::from_str::<Value>(&text[a + 7..b]) {
                        out["posts"] = Value::Array(posts.borrow().clone());
                        out["engine"] = Value::from("webkitgtk");
                        println!("{out}");
                    }
                    app2.quit();
                },
            );
            if started.elapsed() > Duration::from_secs(30) {
                eprintln!("timeout");
                std::process::exit(1);
            }
            glib::ControlFlow::Continue
        });
    });
    app.run_with_args::<&str>(&[])
}
