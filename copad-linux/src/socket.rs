use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;

use gtk4::ApplicationWindow;
use serde_json::json;

use copad_core::action_registry::ActionRegistry;
use copad_core::event_bus::Event as BusEvent;
use copad_core::protocol::{Event, Request, Response};

use vte4::prelude::*;

use crate::background::BackgroundLayer;
use crate::panel::Panel;
use crate::tabs::{FocusDirection, TabManager};

const BUS_SOURCE_COPAD_LINUX: &str = "copad-linux";

pub use copad_daemon::socket::{EventBus, SocketCommand, new_event_bus};

pub fn broadcast(bus: &EventBus, event: &Event) {
    bus.publish(BusEvent::new(
        event.event_type.clone(),
        BUS_SOURCE_COPAD_LINUX,
        event.data.clone(),
    ));
}

pub fn start_server(socket_path: &str, event_bus: EventBus) -> mpsc::Receiver<SocketCommand> {
    let (tx, rx) = mpsc::channel();

    // Reuse `copadd`'s hardened prep+bind: atomic 0700 parent with an ownership check
    // (DirBuilder::mode only sets perms on fresh creates, so an attacker-precreated lax dir
    // would slip through), and chmod 0600 on the bound socket.
    let path = std::path::Path::new(socket_path);
    match copad_daemon::socket::prepare_socket_path(path) {
        copad_daemon::socket::SocketPrep::Fresh
        | copad_daemon::socket::SocketPrep::StaleCleared => {}
        copad_daemon::socket::SocketPrep::InUse => {
            eprintln!("[copad] gui socket {socket_path} already in use by another instance");
            return rx;
        }
        copad_daemon::socket::SocketPrep::NotSocket => {
            eprintln!("[copad] gui socket path {socket_path} exists but is not a socket");
            return rx;
        }
        copad_daemon::socket::SocketPrep::Error(msg) => {
            eprintln!("[copad] gui socket prep failed: {msg}");
            return rx;
        }
    }

    let listener = match copad_daemon::socket::bind_listener(path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[copad] failed to bind socket at {socket_path}: {e}");
            return rx;
        }
    };

    eprintln!("[copad] socket server listening at {socket_path}");

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[copad] socket accept error: {e}");
                    continue;
                }
            };

            let tx = tx.clone();
            let event_bus = event_bus.clone();
            std::thread::spawn(move || {
                let mut reader = match stream.try_clone() {
                    Ok(s) => BufReader::new(s),
                    Err(e) => {
                        eprintln!("[copad] socket clone error: {e}");
                        return;
                    }
                };
                let mut writer = stream;
                let mut frame_buf: Vec<u8> = Vec::with_capacity(8192);

                loop {
                    let line =
                        match copad_daemon::socket::read_line_capped(&mut reader, &mut frame_buf) {
                            Ok(Some(l)) => l,
                            Ok(None) => break,
                            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                                let err = Response::error(
                                    String::new(),
                                    "frame_too_large",
                                    &e.to_string(),
                                );
                                let _ =
                                    writeln!(writer, "{}", serde_json::to_string(&err).unwrap());
                                let _ = writer.flush();
                                // Fail-fast: the helper never consumed the cap-overflow bytes,
                                // so the next call would loop on them. Close the connection.
                                break;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {
                                let err =
                                    Response::error(String::new(), "invalid_utf8", &e.to_string());
                                let _ =
                                    writeln!(writer, "{}", serde_json::to_string(&err).unwrap());
                                let _ = writer.flush();
                                break;
                            }
                            Err(_) => break,
                        };
                    if line.is_empty() {
                        continue;
                    }

                    let request: Request = match serde_json::from_str(&line) {
                        Ok(r) => r,
                        Err(e) => {
                            let err = Response::error(
                                String::new(),
                                "parse_error",
                                &format!("Invalid JSON: {e}"),
                            );
                            let _ = writeln!(writer, "{}", serde_json::to_string(&err).unwrap());
                            let _ = writer.flush();
                            continue;
                        }
                    };

                    if request.method == "event.subscribe" {
                        let resp = Response::success(
                            request.id.clone(),
                            json!({ "status": "subscribed" }),
                        );
                        let _ = writeln!(writer, "{}", serde_json::to_string(&resp).unwrap());
                        let _ = writer.flush();

                        // Unbounded: the external wire contract must not drop events on slow
                        // clients.
                        let rx = event_bus.subscribe_unbounded("*");
                        while let Some(ev) = rx.recv() {
                            let wire = Event::new(ev.kind, ev.payload).with_source(ev.source);
                            let json = match serde_json::to_string(&wire) {
                                Ok(j) => j,
                                Err(_) => continue,
                            };
                            if writeln!(writer, "{json}").is_err() {
                                break;
                            }
                            if writer.flush().is_err() {
                                break;
                            }
                        }
                        return;
                    }

                    let (reply_tx, reply_rx) = mpsc::channel();
                    let cmd = SocketCommand {
                        request,
                        reply: reply_tx,
                        silent_completion: false,
                    };

                    if tx.send(cmd).is_err() {
                        break;
                    }

                    match reply_rx.recv() {
                        Ok(response) => {
                            let _ =
                                writeln!(writer, "{}", serde_json::to_string(&response).unwrap());
                            let _ = writer.flush();
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    });

    rx
}

pub fn dispatch(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    window: &ApplicationWindow,
    statusbar: &Rc<crate::statusbar::StatusBar>,
    background: &Rc<BackgroundLayer>,
    actions: &Arc<ActionRegistry>,
    event_bus: &EventBus,
) {
    let req = &cmd.request;

    // Action Registry first; legacy commands stay in the match below until migrated.
    // `try_dispatch` runs synchronous handlers inline and spawns blocking ones (service-plugin
    // RPC) onto a worker thread, so a slow plugin can't stall the socket-server thread or the
    // GTK timer that pumps it. Either way `cmd.reply.send` lands exactly once.
    //
    // Browser Workbench gate: runs BEFORE `try_dispatch`, not before the legacy match; a browser
    // method registered as an action would otherwise route around it. The decision is
    // `copad_core::browser::authorize` (see `crate::browser`).
    let browser_ctx = match crate::browser::gate(req, mgr) {
        crate::browser::Gate::NotBrowser => None,
        crate::browser::Gate::Refused(err) => {
            cmd.reply_with_completion(
                event_bus,
                Response::error(req.id.clone(), &err.code, &err.message),
            );
            return;
        }
        crate::browser::Gate::Allowed(ctx) => Some(ctx),
    };

    let req_id_for_reply = req.id.clone();
    let reply = cmd.reply.clone();
    if actions.try_dispatch(
        &req.method,
        req.params.clone(),
        Box::new(move |result| {
            let resp = match result {
                Ok(value) => Response::success(req_id_for_reply, value),
                Err(err) => Response {
                    id: req_id_for_reply,
                    ok: false,
                    result: None,
                    error: Some(err),
                },
            };
            let _ = reply.send(resp);
        }),
    ) {
        return;
    }

    // Legacy match-arm path: every reply goes through `cmd.reply_with_completion` so chained
    // triggers see `<action>.completed` / `.failed` uniformly. `silent_completion = true` (set
    // by `gui_client::handle_invoke` for daemon-proxied Invokes) suppresses the local publish;
    // the daemon publishes on its own bus.

    match req.method.as_str() {
        "background.set" => {
            let resp = handle_bg_set(req, background);
            cmd.reply_with_completion(event_bus, resp);
        }

        "background.clear" => {
            let resp = handle_bg_clear(req, background);
            cmd.reply_with_completion(event_bus, resp);
        }

        "background.next" => {
            let resp = handle_bg_next(req, background);
            cmd.reply_with_completion(event_bus, resp);
        }

        "background.delete_current" => {
            let resp = handle_bg_delete_current(req, background);
            cmd.reply_with_completion(event_bus, resp);
        }

        "background.toggle" => {
            let resp = handle_bg_toggle(req, background);
            cmd.reply_with_completion(event_bus, resp);
        }

        "background.set_tint" => {
            let resp = handle_bg_set_tint(req, background);
            cmd.reply_with_completion(event_bus, resp);
        }

        "tab.new" => {
            mgr.add_tab(window);
            let _ = cmd
                .reply
                .send(Response::success(req.id.clone(), json!({ "status": "ok" })));
        }

        "comux.graph" => {
            let open = req.params.get("open").and_then(|v| v.as_bool());
            let mode = req
                .params
                .get("mode")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let width = req
                .params
                .get("width")
                .and_then(|v| v.as_i64())
                .map(|w| w as i32);
            let reply = cmd.reply.clone();
            let id = req.id.clone();
            let snapshot = req
                .params
                .get("snapshot")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            crate::comux_panel::graph_global(open, mode, width, snapshot, move |v| {
                let _ = reply.send(Response::success(id, v));
            });
        }

        "comux.notifications" => {
            let open = req.params.get("open").and_then(|v| v.as_bool());
            let v = crate::comux_panel::notifications_global(open);
            let _ = cmd.reply.send(Response::success(req.id.clone(), v));
        }

        "comux.open_remote" => {
            let comux = req
                .params
                .get("comux")
                .and_then(|v| v.as_str())
                .unwrap_or("comux")
                .to_string();
            let resp = match req.params.get("dest").and_then(|v| v.as_str()) {
                Some(dest) if !dest.is_empty() => {
                    match crate::comux_panel::open_remote(mgr, window, dest, &comux) {
                        Ok(()) => Response::success(req.id.clone(), json!({ "ok": true })),
                        Err(e) => Response::error(req.id.clone(), "refused", &e),
                    }
                }
                _ => {
                    crate::comux_panel::prompt_open_remote(mgr, window);
                    Response::success(req.id.clone(), json!({ "ok": true, "prompted": true }))
                }
            };
            let _ = cmd.reply.send(resp);
        }

        "tab.close" => {
            mgr.close_focused(window);
            let _ = cmd
                .reply
                .send(Response::success(req.id.clone(), json!({ "status": "ok" })));
        }

        "tab.list" => {
            let count = mgr.tab_count();
            let current = mgr.current_tab();
            cmd.reply_with_completion(
                event_bus,
                Response::success(
                    req.id.clone(),
                    json!({ "count": count, "current": current }),
                ),
            );
        }

        "tab.info" => {
            let _ = cmd
                .reply
                .send(Response::success(req.id.clone(), mgr.tab_info()));
        }

        "split.horizontal" => {
            mgr.split_focused(gtk4::Orientation::Horizontal, window);
            let _ = cmd
                .reply
                .send(Response::success(req.id.clone(), json!({ "status": "ok" })));
        }

        "split.vertical" => {
            mgr.split_focused(gtk4::Orientation::Vertical, window);
            let _ = cmd
                .reply
                .send(Response::success(req.id.clone(), json!({ "status": "ok" })));
        }

        "session.list" => {
            cmd.reply_with_completion(
                event_bus,
                Response::success(req.id.clone(), json!(mgr.all_panels_info())),
            );
        }

        "session.info" => {
            let resp = match req.params.get("id").and_then(|v| v.as_str()) {
                Some(id) => match mgr.panel_info_by_id(id) {
                    Some(info) => Response::success(req.id.clone(), info),
                    None => Response::error(
                        req.id.clone(),
                        "not_found",
                        &format!("Panel not found: {id}"),
                    ),
                },
                None => Response::error(req.id.clone(), "invalid_params", "Missing 'id' param"),
            };
            cmd.reply_with_completion(event_bus, resp);
        }

        // `browser_ctx` is `Some` for exactly the methods `browser::classify` knows, which is
        // exactly the arms below; `None` here would mean the gate and this match disagree.
        "webview.open" => {
            let ctx = browser_ctx.as_ref().expect("gate classified webview.open");
            let resp = handle_webview_open(req, mgr, window);
            browser_reply(&cmd, event_bus, mgr, ctx, resp);
        }

        "webview.navigate" => {
            let ctx = browser_ctx
                .as_ref()
                .expect("gate classified webview.navigate");
            let resp = handle_webview_navigate(req, mgr);
            browser_reply(&cmd, event_bus, mgr, ctx, resp);
        }

        "webview.back" => {
            let ctx = browser_ctx.as_ref().expect("gate classified webview.back");
            let resp = with_webview_panel(req, mgr, |wv| {
                wv.go_back();
                Response::success(req.id.clone(), json!({ "status": "ok" }))
            });
            browser_reply(&cmd, event_bus, mgr, ctx, resp);
        }

        "webview.forward" => {
            let ctx = browser_ctx
                .as_ref()
                .expect("gate classified webview.forward");
            let resp = with_webview_panel(req, mgr, |wv| {
                wv.go_forward();
                Response::success(req.id.clone(), json!({ "status": "ok" }))
            });
            browser_reply(&cmd, event_bus, mgr, ctx, resp);
        }

        "webview.reload" => {
            let ctx = browser_ctx
                .as_ref()
                .expect("gate classified webview.reload");
            let resp = with_webview_panel(req, mgr, |wv| {
                wv.reload();
                Response::success(req.id.clone(), json!({ "status": "ok" }))
            });
            browser_reply(&cmd, event_bus, mgr, ctx, resp);
        }

        "webview.execute_js" => {
            let ctx = browser_ctx
                .as_ref()
                .expect("gate classified webview.execute_js");
            handle_webview_execute_js(cmd, mgr, event_bus, ctx);
        }

        "webview.get_content" => {
            let ctx = browser_ctx
                .as_ref()
                .expect("gate classified webview.get_content");
            handle_webview_get_content(cmd, mgr, event_bus, ctx);
        }

        "webview.screenshot" => {
            let ctx = browser_ctx
                .as_ref()
                .expect("gate classified webview.screenshot");
            handle_webview_screenshot(cmd, mgr, event_bus, ctx);
        }

        "webview.query" => {
            let ctx = browser_ctx.as_ref().expect("gate classified webview.query");
            handle_webview_query(cmd, mgr, event_bus, ctx);
        }

        "webview.query_all" => {
            let ctx = browser_ctx
                .as_ref()
                .expect("gate classified webview.query_all");
            handle_webview_query_all(cmd, mgr, event_bus, ctx);
        }

        "webview.get_styles" => {
            let ctx = browser_ctx
                .as_ref()
                .expect("gate classified webview.get_styles");
            handle_webview_get_styles(cmd, mgr, event_bus, ctx);
        }

        "webview.click" => {
            let ctx = browser_ctx.as_ref().expect("gate classified webview.click");
            handle_webview_click(cmd, mgr, event_bus, ctx);
        }

        "webview.fill" => {
            let ctx = browser_ctx.as_ref().expect("gate classified webview.fill");
            handle_webview_fill(cmd, mgr, event_bus, ctx);
        }

        "webview.scroll" => {
            let ctx = browser_ctx
                .as_ref()
                .expect("gate classified webview.scroll");
            handle_webview_scroll(cmd, mgr, event_bus, ctx);
        }

        "webview.page_info" => {
            let ctx = browser_ctx
                .as_ref()
                .expect("gate classified webview.page_info");
            handle_webview_page_info(cmd, mgr, event_bus, ctx);
        }

        // Requires `id`, unlike macOS which falls back to the active webview, for consistency
        // with every other Linux `webview.*` method.
        "webview.state" => {
            use webkit6::prelude::WebViewExt;
            let ctx = browser_ctx.as_ref().expect("gate classified webview.state");
            let resp = with_webview_panel(req, mgr, |wv| {
                Response::success(
                    req.id.clone(),
                    json!({
                        "url": wv.current_url(),
                        "title": wv.title(),
                        "can_go_back": wv.webview.can_go_back(),
                        "can_go_forward": wv.webview.can_go_forward(),
                        "is_loading": wv.webview.is_loading(),
                    }),
                )
            });
            browser_reply(&cmd, event_bus, mgr, ctx, resp);
        }

        "webview.devtools" => {
            let ctx = browser_ctx
                .as_ref()
                .expect("gate classified webview.devtools");
            let resp = handle_webview_devtools(req, mgr);
            browser_reply(&cmd, event_bus, mgr, ctx, resp);
        }

        // Zero-based, as `tab.info` reports. Errors on a bad index (macOS silently no-ops)
        // because a coctl caller has no other channel to learn it was wrong.
        "tab.switch" => {
            let resp = match req.params.get("index").and_then(|v| v.as_u64()) {
                Some(index) => {
                    if mgr.switch_tab(index as usize) {
                        Response::success(req.id.clone(), json!({ "status": "ok" }))
                    } else {
                        Response::error(
                            req.id.clone(),
                            "not_found",
                            &format!("No tab at index {index}"),
                        )
                    }
                }
                None => Response::error(
                    req.id.clone(),
                    "invalid_params",
                    "Missing or non-integer 'index' param",
                ),
            };
            cmd.reply_with_completion(event_bus, resp);
        }

        // Focus one pane by id and bring the window forward: the comux notification-jump path
        // (decision #115). `activate_panel` is what a cockpit row click calls.
        "panel.focus" => {
            let resp = match req.params.get("panel_id").and_then(|v| v.as_str()) {
                Some(panel_id) if !panel_id.is_empty() => {
                    let focused = mgr.activate_panel(panel_id);
                    if focused {
                        // Only on a hit: raising after a miss would bring forward an instance that
                        // did not satisfy the request. `raised: false` regardless: a Wayland client
                        // without an activation token has its `present()` accepted and IGNORED
                        // (measured on Hyprland) and X11 focus-stealing prevention can refuse it,
                        // so this asks and reports it cannot promise; the caller then runs the
                        // compositor-level raise.
                        window.present();
                    }
                    // `focused: false` is a SUCCESSFUL call that found nothing, not an error: the
                    // id may belong to another copad instance, and comux branches on it to decide
                    // whether to fall back (`copad-mux/src/copadlink.rs`).
                    Response::success(
                        req.id.clone(),
                        json!({ "ok": true, "focused": focused, "raised": false }),
                    )
                }
                _ => Response::error(
                    req.id.clone(),
                    "invalid_params",
                    "panel.focus requires a non-empty `panel_id` string",
                ),
            };
            cmd.reply_with_completion(event_bus, resp);
        }

        "pane.focus_next" => {
            mgr.focus_direction(FocusDirection::Next);
            cmd.reply_with_completion(
                event_bus,
                Response::success(req.id.clone(), json!({ "status": "ok" })),
            );
        }

        "pane.focus_prev" => {
            mgr.focus_direction(FocusDirection::Prev);
            cmd.reply_with_completion(
                event_bus,
                Response::success(req.id.clone(), json!({ "status": "ok" })),
            );
        }

        "tabs.toggle_bar" => {
            let visible = mgr.toggle_tab_bar();
            cmd.reply_with_completion(
                event_bus,
                Response::success(req.id.clone(), json!({ "visible": visible })),
            );
        }

        "tab.rename" => {
            let resp = match (
                req.params.get("id").and_then(|v| v.as_str()),
                req.params.get("title").and_then(|v| v.as_str()),
            ) {
                (Some(id), Some(title)) => {
                    if mgr.rename_tab(id, title) {
                        Response::success(req.id.clone(), json!({ "status": "ok" }))
                    } else {
                        Response::error(
                            req.id.clone(),
                            "not_found",
                            &format!("Panel not found: {id}"),
                        )
                    }
                }
                _ => Response::error(
                    req.id.clone(),
                    "invalid_params",
                    "Missing 'id' and/or 'title' param",
                ),
            };
            cmd.reply_with_completion(event_bus, resp);
        }

        "terminal.read" => {
            let resp = handle_terminal_read(req, mgr);
            cmd.reply_with_completion(event_bus, resp);
        }

        "terminal.state" => {
            let resp = handle_terminal_state(req, mgr);
            cmd.reply_with_completion(event_bus, resp);
        }

        "terminal.exec" => {
            let resp = handle_terminal_exec(req, mgr);
            cmd.reply_with_completion(event_bus, resp);
        }

        "terminal.feed" => {
            let resp = handle_terminal_feed(req, mgr);
            cmd.reply_with_completion(event_bus, resp);
        }

        "terminal.history" => {
            let resp = handle_terminal_history(req, mgr);
            cmd.reply_with_completion(event_bus, resp);
        }

        "terminal.context" => {
            let resp = handle_terminal_context(req, mgr);
            cmd.reply_with_completion(event_bus, resp);
        }

        "agent.approve" => {
            handle_agent_approve(cmd, window, event_bus);
        }

        "claude.start" => {
            let resp = handle_claude_start(req, mgr, window);
            cmd.reply_with_completion(event_bus, resp);
        }

        "workflow.run" => {
            let resp = handle_workflow_run(req, mgr, window, event_bus);
            cmd.reply_with_completion(event_bus, resp);
        }

        "theme.list" => {
            let themes: Vec<&str> = copad_core::theme::Theme::list().to_vec();
            let current = mgr.current_theme_name();
            cmd.reply_with_completion(
                event_bus,
                Response::success(
                    req.id.clone(),
                    json!({ "themes": themes, "current": current }),
                ),
            );
        }

        "plugin.list" => {
            let plugins: Vec<serde_json::Value> = mgr
                .plugins()
                .iter()
                .map(|p| {
                    let m = &p.manifest;
                    json!({
                        "name": m.plugin.name,
                        "title": m.plugin.title,
                        "version": m.plugin.version,
                        "description": m.plugin.description,
                        "panels": m.panels.iter().map(|pd| json!({
                            "name": pd.name,
                            "title": pd.title,
                        })).collect::<Vec<_>>(),
                        "commands": m.commands.iter().map(|cd| json!({
                            "name": cd.name,
                            "description": cd.description,
                        })).collect::<Vec<_>>(),
                        "modules": m.modules.iter().map(|md| json!({
                            "name": md.name,
                            "exec": md.exec,
                            "interval": md.interval,
                            "position": md.position,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            cmd.reply_with_completion(
                event_bus,
                Response::success(req.id.clone(), json!({ "plugins": plugins })),
            );
        }

        "plugin.open" => {
            let resp = handle_plugin_open(req, mgr);
            cmd.reply_with_completion(event_bus, resp);
        }

        // Opens the cockpit, or focuses it. The Ctrl+Shift+Y default can be shadowed by a user
        // `[keybindings]` entry (checked first), so this keeps the panel reachable and
        // scriptable.
        "cockpit.open" => {
            mgr.toggle_cockpit();
            cmd.reply_with_completion(
                event_bus,
                Response::success(req.id.clone(), json!({ "status": "ok" })),
            );
        }

        "statusbar.show" => {
            statusbar.set_visible(true);
            cmd.reply_with_completion(
                event_bus,
                Response::success(req.id.clone(), json!({ "visible": true })),
            );
        }

        "statusbar.hide" => {
            statusbar.set_visible(false);
            cmd.reply_with_completion(
                event_bus,
                Response::success(req.id.clone(), json!({ "visible": false })),
            );
        }

        "statusbar.toggle" => {
            let visible = statusbar.toggle();
            cmd.reply_with_completion(
                event_bus,
                Response::success(req.id.clone(), json!({ "visible": visible })),
            );
        }

        _ => {
            crate::daemon_forward::forward(cmd.request.clone(), cmd.reply.clone());
        }
    }
}

fn handle_bg_set(req: &Request, bg: &Rc<BackgroundLayer>) -> Response {
    let path = req.params.get("path").and_then(|v| v.as_str());
    match path {
        Some(p) => {
            let path = Path::new(p);
            if !path.exists() {
                return Response::error(
                    req.id.clone(),
                    "not_found",
                    &format!("File not found: {p}"),
                );
            }
            // A directory is a valid config source but cannot be decoded; without this it
            // fails in the decoder with an opaque pixbuf error.
            if path.is_dir() {
                return Response::error(
                    req.id.clone(),
                    "invalid_params",
                    &format!(
                        "{p} is a directory — set `[background] image` to it in config.toml to \
                         use it as the rotation source"
                    ),
                );
            }
            bg.set_image(path);
            // A manual pick restarts the rotation countdown.
            bg.arm_rotation();
            Response::success(req.id.clone(), json!({ "status": "ok" }))
        }
        None => Response::error(req.id.clone(), "invalid_params", "Missing 'path' param"),
    }
}

fn handle_bg_clear(req: &Request, bg: &Rc<BackgroundLayer>) -> Response {
    bg.clear_image();
    Response::success(req.id.clone(), json!({ "status": "ok" }))
}

fn handle_bg_next(req: &Request, bg: &Rc<BackgroundLayer>) -> Response {
    if !bg.is_active() {
        return Response::success(
            req.id.clone(),
            json!({ "status": "ok", "mode": "deactive" }),
        );
    }
    // `pick` already skips vanished entries, so `None` means genuinely empty, not stale.
    match bg.pick() {
        Some(img) => {
            bg.set_image_from_list(Path::new(&img));
            bg.arm_rotation();
            Response::success(req.id.clone(), json!({ "status": "ok", "path": img }))
        }
        None => Response::error(
            req.id.clone(),
            "no_images",
            "No images available from the configured background source",
        ),
    }
}

fn handle_bg_toggle(req: &Request, bg: &Rc<BackgroundLayer>) -> Response {
    let now_active = bg.toggle_mode();
    // Mark before the watcher's echo of our own write arrives.
    bg.note_mode_applied(now_active);
    if now_active {
        if let Some(img) = bg.pick() {
            bg.set_image_from_list(Path::new(&img));
        }
    } else {
        bg.clear_image();
    }
    bg.arm_rotation();
    let mode = if now_active { "active" } else { "deactive" };
    Response::success(req.id.clone(), json!({ "status": "ok", "mode": mode }))
}

/// Delete the currently displayed wallpaper from disk AND the list file, then rotate to the
/// next pick. Only list-picked images (rotation / `background.next` / `toggle`); a manually
/// `set` file or a config `[background] image` is never deleted.
fn handle_bg_delete_current(req: &Request, bg: &Rc<BackgroundLayer>) -> Response {
    let Some(img) = bg.current_list_image() else {
        return Response::error(
            req.id.clone(),
            "no_current",
            "No list-picked background to delete (manual/static images are never deleted)",
        );
    };
    if let Err(e) = std::fs::remove_file(&img)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        return Response::error(
            req.id.clone(),
            "io_error",
            &format!("delete {}: {e}", img.display()),
        );
    }
    if let Err(e) = bg.drop_from_source(&img) {
        return Response::error(
            req.id.clone(),
            "io_error",
            &format!("rewrite wallpaper list: {e}"),
        );
    }
    let next = bg.pick();
    match &next {
        Some(n) => {
            bg.set_image_from_list(Path::new(n));
            bg.arm_rotation();
        }
        None => bg.clear_image(),
    }
    Response::success(
        req.id.clone(),
        json!({ "status": "ok", "deleted": img.to_string_lossy(), "next": next }),
    )
}

fn handle_bg_set_tint(req: &Request, bg: &Rc<BackgroundLayer>) -> Response {
    let opacity = req.params.get("opacity").and_then(|v| v.as_f64());
    match opacity {
        Some(o) => {
            bg.set_tint(o);
            Response::success(req.id.clone(), json!({ "status": "ok" }))
        }
        None => Response::error(req.id.clone(), "invalid_params", "Missing 'opacity' param"),
    }
}

/// Reply to a browser method, applying the delivery-time half of the gate. Every browser reply
/// goes through here, including early `invalid_params` / `not_found` errors: a protected write's
/// ERROR leaks the same bit its result does. `finalize` collapses both into one fixed response,
/// only while the profile is protected.
fn browser_reply(
    cmd: &SocketCommand,
    event_bus: &EventBus,
    mgr: &Rc<TabManager>,
    ctx: &crate::browser::GateCtx,
    resp: Response,
) {
    cmd.reply_with_completion(event_bus, crate::browser::finalize(mgr, ctx, resp));
}

fn handle_webview_open(
    req: &Request,
    mgr: &Rc<TabManager>,
    window: &ApplicationWindow,
) -> Response {
    let url = match req.params.get("url").and_then(|v| v.as_str()) {
        Some(u) => u,
        None => return Response::error(req.id.clone(), "invalid_params", "Missing 'url' param"),
    };
    let mode = req
        .params
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("tab");

    let panel_id = match mode {
        "split_h" => match mgr.split_focused_webview(url, gtk4::Orientation::Horizontal, window) {
            Some(id) => id,
            None => {
                return Response::error(req.id.clone(), "no_panel", "No focused panel to split");
            }
        },
        "split_v" => match mgr.split_focused_webview(url, gtk4::Orientation::Vertical, window) {
            Some(id) => id,
            None => {
                return Response::error(req.id.clone(), "no_panel", "No focused panel to split");
            }
        },
        _ => mgr.add_webview_tab(url, window),
    };

    Response::success(req.id.clone(), json!({ "panel_id": panel_id }))
}

fn handle_webview_navigate(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return Response::error(req.id.clone(), "invalid_params", "Missing 'id' param"),
    };
    let url = match req.params.get("url").and_then(|v| v.as_str()) {
        Some(u) => u,
        None => return Response::error(req.id.clone(), "invalid_params", "Missing 'url' param"),
    };

    match mgr.find_panel_by_id(id) {
        Some(panel) => match panel.as_webview() {
            Some(wv) => {
                wv.navigate(url);
                Response::success(req.id.clone(), json!({ "status": "ok" }))
            }
            None => Response::error(req.id.clone(), "wrong_panel_type", "Panel is not a webview"),
        },
        None => Response::error(
            req.id.clone(),
            "not_found",
            &format!("Panel not found: {id}"),
        ),
    }
}

fn with_webview_panel(
    req: &Request,
    mgr: &Rc<TabManager>,
    f: impl FnOnce(&crate::webview::WebViewPanel) -> Response,
) -> Response {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id,
        None => return Response::error(req.id.clone(), "invalid_params", "Missing 'id' param"),
    };
    match mgr.find_panel_by_id(id) {
        Some(panel) => match panel.as_webview() {
            Some(wv) => f(wv),
            None => Response::error(req.id.clone(), "wrong_panel_type", "Panel is not a webview"),
        },
        None => Response::error(
            req.id.clone(),
            "not_found",
            &format!("Panel not found: {id}"),
        ),
    }
}

fn handle_webview_execute_js(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    event_bus: &EventBus,
    ctx: &crate::browser::GateCtx,
) {
    let req = &cmd.request;
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(req.id.clone(), "invalid_params", "Missing 'id' param"),
            );
            return;
        }
    };
    let code = match req.params.get("code").and_then(|v| v.as_str()) {
        Some(c) => c.to_string(),
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(req.id.clone(), "invalid_params", "Missing 'code' param"),
            );
            return;
        }
    };

    let panel = match mgr.find_panel_by_id(&id) {
        Some(p) => p,
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(
                    req.id.clone(),
                    "not_found",
                    &format!("Panel not found: {id}"),
                ),
            );
            return;
        }
    };
    let wv = match panel.as_webview() {
        Some(wv) => wv,
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(req.id.clone(), "wrong_panel_type", "Panel is not a webview"),
            );
            return;
        }
    };

    let req_id = req.id.clone();
    let silent = cmd.silent_completion;
    let bus = event_bus.clone();
    let reply = cmd.reply;
    // Delivery-time half of the gate: re-asks `authorize` against live state, so a read in
    // flight when the tab entered `Protected` is suppressed.
    let out = crate::browser::BrowserReply::new(reply, bus, silent, ctx.clone(), mgr.clone());
    wv.execute_js(&code, move |result| {
        let resp = match result {
            Ok(value) => Response::success(req_id, json!({ "result": value })),
            Err(e) => Response::error(req_id, "js_error", &e),
        };
        out.send(resp);
    });
}

fn handle_webview_get_content(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    event_bus: &EventBus,
    ctx: &crate::browser::GateCtx,
) {
    let req = &cmd.request;
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(req.id.clone(), "invalid_params", "Missing 'id' param"),
            );
            return;
        }
    };
    let format = req
        .params
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("text");
    let js_code = match format {
        "html" => "document.documentElement.outerHTML".to_string(),
        _ => "document.body.innerText".to_string(),
    };

    let panel = match mgr.find_panel_by_id(&id) {
        Some(p) => p,
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(
                    req.id.clone(),
                    "not_found",
                    &format!("Panel not found: {id}"),
                ),
            );
            return;
        }
    };
    let wv = match panel.as_webview() {
        Some(wv) => wv,
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(req.id.clone(), "wrong_panel_type", "Panel is not a webview"),
            );
            return;
        }
    };

    let req_id = req.id.clone();
    let silent = cmd.silent_completion;
    let bus = event_bus.clone();
    let reply = cmd.reply;
    // Delivery-time half of the gate: re-asks `authorize` against live state, so a read in
    // flight when the tab entered `Protected` is suppressed.
    let out = crate::browser::BrowserReply::new(reply, bus, silent, ctx.clone(), mgr.clone());
    wv.execute_js(&js_code, move |result| {
        let resp = match result {
            Ok(content) => Response::success(req_id, json!({ "content": content })),
            Err(e) => Response::error(req_id, "js_error", &e),
        };
        out.send(resp);
    });
}

fn handle_webview_screenshot(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    event_bus: &EventBus,
    ctx: &crate::browser::GateCtx,
) {
    let req = &cmd.request;
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(req.id.clone(), "invalid_params", "Missing 'id' param"),
            );
            return;
        }
    };

    let panel = match mgr.find_panel_by_id(&id) {
        Some(p) => p,
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(
                    req.id.clone(),
                    "not_found",
                    &format!("Panel not found: {id}"),
                ),
            );
            return;
        }
    };
    let wv = match panel.as_webview() {
        Some(wv) => wv,
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(req.id.clone(), "wrong_panel_type", "Panel is not a webview"),
            );
            return;
        }
    };

    let req_id = req.id.clone();
    let silent = cmd.silent_completion;
    let bus = event_bus.clone();
    let reply = cmd.reply;
    // Delivery-time half of the gate: re-asks `authorize` against live state, so a read in
    // flight when the tab entered `Protected` is suppressed.
    let out = crate::browser::BrowserReply::new(reply, bus, silent, ctx.clone(), mgr.clone());
    let path = req
        .params
        .get("path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    wv.snapshot(move |result| {
        let resp = match result {
            Ok(base64_png) => {
                if let Some(path) = path {
                    match gtk4::glib::base64_decode(&base64_png) {
                        data if !data.is_empty() => match std::fs::write(&path, &data) {
                            Ok(_) => Response::success(req_id, json!({ "path": path })),
                            Err(e) => Response::error(req_id, "io_error", &e.to_string()),
                        },
                        _ => Response::error(req_id, "decode_error", "Failed to decode PNG"),
                    }
                } else {
                    Response::success(req_id, json!({ "image": base64_png }))
                }
            }
            Err(e) => Response::error(req_id, "snapshot_error", &e),
        };
        out.send(resp);
    });
}

fn run_js_command(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    js_code: String,
    event_bus: &EventBus,
    ctx: &crate::browser::GateCtx,
) {
    let req = &cmd.request;
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(id) => id.to_string(),
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(req.id.clone(), "invalid_params", "Missing 'id' param"),
            );
            return;
        }
    };

    let panel = match mgr.find_panel_by_id(&id) {
        Some(p) => p,
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(
                    req.id.clone(),
                    "not_found",
                    &format!("Panel not found: {id}"),
                ),
            );
            return;
        }
    };
    let wv = match panel.as_webview() {
        Some(wv) => wv,
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(req.id.clone(), "wrong_panel_type", "Panel is not a webview"),
            );
            return;
        }
    };

    let req_id = req.id.clone();
    let silent = cmd.silent_completion;
    let bus = event_bus.clone();
    let reply = cmd.reply;
    // Delivery-time half of the gate: re-asks `authorize` against live state, so a read in
    // flight when the tab entered `Protected` is suppressed.
    let out = crate::browser::BrowserReply::new(reply, bus, silent, ctx.clone(), mgr.clone());
    wv.execute_js(&js_code, move |result| {
        let resp = match result {
            Ok(json_str) => match serde_json::from_str::<serde_json::Value>(&json_str) {
                Ok(val) => Response::success(req_id, json!({ "result": val })),
                Err(_) => Response::success(req_id, json!({ "result": json_str })),
            },
            Err(e) => Response::error(req_id, "js_error", &e),
        };
        out.send(resp);
    });
}

fn handle_webview_query(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    event_bus: &EventBus,
    ctx: &crate::browser::GateCtx,
) {
    let selector = match cmd.request.params.get("selector").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(
                    cmd.request.id.clone(),
                    "invalid_params",
                    "Missing 'selector' param",
                ),
            );
            return;
        }
    };
    let js = crate::webview::js::query_selector(&selector);
    run_js_command(cmd, mgr, js, event_bus, ctx);
}

fn handle_webview_query_all(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    event_bus: &EventBus,
    ctx: &crate::browser::GateCtx,
) {
    let selector = match cmd.request.params.get("selector").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(
                    cmd.request.id.clone(),
                    "invalid_params",
                    "Missing 'selector' param",
                ),
            );
            return;
        }
    };
    let limit = cmd
        .request
        .params
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(50) as u32;
    let js = crate::webview::js::query_selector_all(&selector, limit);
    run_js_command(cmd, mgr, js, event_bus, ctx);
}

fn handle_webview_get_styles(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    event_bus: &EventBus,
    ctx: &crate::browser::GateCtx,
) {
    let selector = match cmd.request.params.get("selector").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(
                    cmd.request.id.clone(),
                    "invalid_params",
                    "Missing 'selector' param",
                ),
            );
            return;
        }
    };
    let properties: Vec<&str> = cmd
        .request
        .params
        .get("properties")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let js = crate::webview::js::get_styles(&selector, &properties);
    run_js_command(cmd, mgr, js, event_bus, ctx);
}

fn handle_webview_click(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    event_bus: &EventBus,
    ctx: &crate::browser::GateCtx,
) {
    let selector = match cmd.request.params.get("selector").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(
                    cmd.request.id.clone(),
                    "invalid_params",
                    "Missing 'selector' param",
                ),
            );
            return;
        }
    };
    let js = crate::webview::js::click(&selector);
    run_js_command(cmd, mgr, js, event_bus, ctx);
}

fn handle_webview_fill(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    event_bus: &EventBus,
    ctx: &crate::browser::GateCtx,
) {
    let selector = match cmd.request.params.get("selector").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(
                    cmd.request.id.clone(),
                    "invalid_params",
                    "Missing 'selector' param",
                ),
            );
            return;
        }
    };
    let value = match cmd.request.params.get("value").and_then(|v| v.as_str()) {
        Some(v) => v.to_string(),
        None => {
            browser_reply(
                &cmd,
                event_bus,
                mgr,
                ctx,
                Response::error(
                    cmd.request.id.clone(),
                    "invalid_params",
                    "Missing 'value' param",
                ),
            );
            return;
        }
    };
    let js = crate::webview::js::fill(&selector, &value);
    run_js_command(cmd, mgr, js, event_bus, ctx);
}

fn handle_webview_scroll(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    event_bus: &EventBus,
    ctx: &crate::browser::GateCtx,
) {
    let selector = cmd
        .request
        .params
        .get("selector")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let x = cmd
        .request
        .params
        .get("x")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let y = cmd
        .request
        .params
        .get("y")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let js = crate::webview::js::scroll(selector.as_deref(), x, y);
    run_js_command(cmd, mgr, js, event_bus, ctx);
}

fn handle_webview_page_info(
    cmd: SocketCommand,
    mgr: &Rc<TabManager>,
    event_bus: &EventBus,
    ctx: &crate::browser::GateCtx,
) {
    let js = crate::webview::js::page_info();
    run_js_command(cmd, mgr, js, event_bus, ctx);
}

fn handle_webview_devtools(req: &Request, mgr: &Rc<TabManager>) -> Response {
    use webkit6::prelude::WebViewExt;
    let action = req
        .params
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("show");
    with_webview_panel(req, mgr, |wv| {
        if let Some(inspector) = wv.webview.inspector() {
            match action {
                "show" => inspector.show(),
                "close" => inspector.close(),
                "attach" => inspector.attach(),
                "detach" => inspector.detach(),
                other => {
                    return Response::error(
                        req.id.clone(),
                        "invalid_params",
                        &format!("Unknown action: {other}. Use show/close/attach/detach"),
                    );
                }
            }
            Response::success(req.id.clone(), json!({ "status": "ok" }))
        } else {
            Response::error(req.id.clone(), "no_inspector", "Inspector not available")
        }
    })
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// Resolve the cwd a `workflow.run` spawns its tab in: a resolved project's workspace path, else
/// the active pane's cwd, else `$HOME`, so project-agnostic workflows (e.g. `/catchup`) can
/// launch from a panel whose own pane carries no cwd.
fn resolve_workflow_workspace(
    project: Option<&copad_core::project::Project>,
    active_cwd: Option<PathBuf>,
) -> PathBuf {
    match project {
        Some(p) => p.workspace_path(),
        None => active_cwd.unwrap_or_else(home_dir),
    }
}

pub fn cleanup(socket_path: &str) {
    let _ = std::fs::remove_file(socket_path);
}

fn resolve_terminal(
    req: &Request,
    mgr: &Rc<TabManager>,
) -> Result<Rc<crate::panel::PanelVariant>, Response> {
    if let Some(id) = req.params.get("id").and_then(|v| v.as_str()) {
        let panel = mgr.find_panel_by_id(id).ok_or_else(|| {
            Response::error(
                req.id.clone(),
                "not_found",
                &format!("Panel not found: {id}"),
            )
        })?;
        if panel.as_terminal().is_none() {
            return Err(Response::error(
                req.id.clone(),
                "wrong_panel_type",
                "Panel is not a terminal",
            ));
        }
        return Ok(panel);
    }

    if let Some(panel) = mgr.active_panel()
        && panel.as_terminal().is_some()
    {
        return Ok(panel);
    }

    mgr.find_first_terminal()
        .ok_or_else(|| Response::error(req.id.clone(), "no_terminal", "No terminal panel found"))
}

fn handle_terminal_read(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let panel = match resolve_terminal(req, mgr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let term = panel.as_terminal().unwrap();

    let has_range = req.params.get("start_row").is_some();
    let text = if has_range {
        let start_row = req
            .params
            .get("start_row")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let start_col = req
            .params
            .get("start_col")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let end_row = req
            .params
            .get("end_row")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| term.terminal.row_count() - 1);
        let end_col = req
            .params
            .get("end_col")
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| term.terminal.column_count() - 1);
        term.read_range(start_row, start_col, end_row, end_col)
    } else {
        term.read_screen()
    };

    let (col, row) = term.terminal.cursor_position();
    Response::success(
        req.id.clone(),
        json!({
            "text": text,
            "cursor": [row, col],
            "rows": term.terminal.row_count(),
            "cols": term.terminal.column_count(),
        }),
    )
}

fn handle_terminal_state(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let panel = match resolve_terminal(req, mgr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let term = panel.as_terminal().unwrap();
    Response::success(req.id.clone(), term.state())
}

fn handle_terminal_exec(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let panel = match resolve_terminal(req, mgr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let command = match req.params.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => {
            return Response::error(req.id.clone(), "invalid_params", "Missing 'command' param");
        }
    };
    let term = panel.as_terminal().unwrap();
    term.feed_input(&format!("{command}\n"));
    Response::success(req.id.clone(), json!({ "status": "ok" }))
}

fn handle_terminal_feed(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let panel = match resolve_terminal(req, mgr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let text = match req.params.get("text").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return Response::error(req.id.clone(), "invalid_params", "Missing 'text' param"),
    };
    let term = panel.as_terminal().unwrap();
    term.feed_input(text);
    Response::success(req.id.clone(), json!({ "status": "ok" }))
}

fn handle_terminal_history(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let panel = match resolve_terminal(req, mgr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let term = panel.as_terminal().unwrap();

    let lines = req
        .params
        .get("lines")
        .and_then(|v| v.as_i64())
        .unwrap_or(100);

    let row_count = term.terminal.row_count();
    let col_count = term.terminal.column_count();

    let top = term.screen_top_row();
    let start_row = (top - lines).max(0);
    let end_row = top + row_count - 1;

    let text = term.read_range(start_row, 0, end_row, col_count - 1);
    Response::success(
        req.id.clone(),
        json!({
            "text": text,
            "lines_requested": lines,
            "rows": row_count,
            "cols": col_count,
        }),
    )
}

fn handle_terminal_context(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let panel = match resolve_terminal(req, mgr) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let term = panel.as_terminal().unwrap();

    let state = term.state();
    let screen = term.read_screen();

    let history_lines = req
        .params
        .get("history_lines")
        .and_then(|v| v.as_i64())
        .unwrap_or(50);
    let col_count = term.terminal.column_count();
    let top = term.screen_top_row();
    let history = if top > 0 {
        term.read_range((top - history_lines).max(0), 0, top - 1, col_count - 1)
    } else {
        String::new()
    };

    Response::success(
        req.id.clone(),
        json!({
            "state": state,
            "screen": screen,
            "history": history,
        }),
    )
}

/// Spawns a tab at `workspace_path` and runs `tmux new-session -A -s <name> 'claude [...]'`.
/// `-A` attaches to an existing same-name session (re-running on a worktree re-attaches live
/// claude instead of stacking duplicates) or creates one. `prompt` (mutually exclusive with
/// `resume_session`) is paste-seeded via `spawn_claude_prompt_seeder` once two readiness checks
/// confirm the pane is claude; failures log but don't propagate.
fn handle_claude_start(
    req: &Request,
    mgr: &Rc<TabManager>,
    window: &ApplicationWindow,
) -> Response {
    let workspace_path_str = match req.params.get("workspace_path") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        Some(_) => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                "'workspace_path' must be a non-empty string",
            );
        }
        None => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                "Missing 'workspace_path' param",
            );
        }
    };
    let raw_path = std::path::Path::new(&workspace_path_str);
    let canon = match std::fs::canonicalize(raw_path) {
        Ok(p) => p,
        Err(e) => {
            return Response::error(
                req.id.clone(),
                "not_found",
                &format!("workspace_path {workspace_path_str:?}: {e}"),
            );
        }
    };
    if !canon.is_dir() {
        return Response::error(
            req.id.clone(),
            "invalid_params",
            &format!("workspace_path {} is not a directory", canon.display()),
        );
    }

    // `fresh_session`: side-effecting flows force a brand-new tmux session so they never attach
    // onto a stale one, by suffixing the session name (see the uniquifier below).
    let fresh_session = match req.params.get("fresh_session") {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::Null) | None => false,
        Some(other) => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                &format!("'fresh_session' must be a bool, got {other}"),
            );
        }
    };

    // session_name: explicit or derived. tmux forbids `:` and `.`; we restrict to ASCII
    // alphanumeric + `-_` so the value is safe to embed in shell commands unescaped.
    let session_name = match req.params.get("session_name") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => {
            if let Err(e) = validate_tmux_session_name(s) {
                return Response::error(
                    req.id.clone(),
                    "invalid_params",
                    &format!("session_name: {e}"),
                );
            }
            s.clone()
        }
        Some(serde_json::Value::Null) | None => derive_session_name(&canon),
        Some(other) => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                &format!("'session_name' must be a string, got {other}"),
            );
        }
    };

    // Uniquify AFTER session_name resolution so explicit and derived names share the suffix
    // logic; the digits-only suffix passes `validate_tmux_session_name`. Micros alone leave a
    // 1us window where two concurrent dispatches in a workspace get identical names and `tmux
    // new-session -A` attaches the second to the first, so a process-local atomic counter is
    // combined with the wall-clock micros.
    let session_name = if fresh_session {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0);
        let seq = next_fresh_session_seq();
        format!("{session_name}-{ts}-{seq}")
    } else {
        session_name
    };

    // resume_session: optional claude session id, validated permissively (non-empty) and
    // single-quote-escaped before embedding in the tmux command.
    let resume_session = match req.params.get("resume_session") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => {
            for c in s.chars() {
                if c.is_control() || c == '\0' {
                    return Response::error(
                        req.id.clone(),
                        "invalid_params",
                        "resume_session contains control characters",
                    );
                }
            }
            Some(s.clone())
        }
        Some(serde_json::Value::Null) | None => None,
        Some(other) => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                &format!("'resume_session' must be a string, got {other}"),
            );
        }
    };

    // Prompt seeding via tmux paste-buffer. `prompt` and `resume_session` are mutually
    // exclusive: `--resume` restores an existing conversation, and seeding on top would
    // confuse claude.
    let prompt_to_seed = match req.params.get("prompt") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(serde_json::Value::String(_)) | Some(serde_json::Value::Null) | None => None,
        Some(other) => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                &format!("'prompt' must be a string, got {other}"),
            );
        }
    };
    if prompt_to_seed.is_some() && resume_session.is_some() {
        return Response::error(
            req.id.clone(),
            "invalid_params",
            "'prompt' and 'resume_session' are mutually exclusive — \
             resume restores existing context; prompt seeds a new conversation",
        );
    }
    if fresh_session && resume_session.is_some() {
        return Response::error(
            req.id.clone(),
            "invalid_params",
            "'fresh_session' and 'resume_session' are mutually exclusive — \
             fresh forces a new session; resume requires an existing one",
        );
    }

    let claude_cmd = match &resume_session {
        Some(id) => format!("claude --resume {}", shell_single_quote(id)),
        None => "claude".to_string(),
    };
    let tmux_command = format!(
        "tmux new-session -A -s {} {}\n",
        shell_single_quote(&session_name),
        shell_single_quote(&claude_cmd),
    );

    // Passed as `initial_input` so it is fed from VTE's spawn_async success callback, avoiding a
    // write to a PTY whose child shell isn't attached yet.
    let (panel, tab_index) =
        mgr.add_tab_with_cwd_and_initial_input(window, Some(&canon), Some(tmux_command));
    let panel_id = panel.id().to_string();
    if panel.as_terminal().is_none() {
        // Always a terminal panel today; fail loudly if that changes.
        return Response::error(
            req.id.clone(),
            "internal_error",
            "claude.start expected a terminal panel",
        );
    }

    // Seed the prompt in a background thread once claude's REPL is up; polling `tmux
    // capture-pane` avoids a fixed sleep that is either racy or wasteful.
    if let Some(prompt) = prompt_to_seed {
        spawn_claude_prompt_seeder(session_name.clone(), prompt);
    }

    Response::success(
        req.id.clone(),
        json!({
            "panel_id": panel_id,
            "tab": tab_index,
            "tmux_session": session_name,
            "workspace_path": canon.display().to_string(),
        }),
    )
}

/// `workflow.run`: resolves the spec, validates form values, substitutes the prompt template,
/// then reuses `handle_claude_start` in-process with a synthetic `Request`. Emits
/// `workflow.started` on the bus; `workflow.timed_out` is a one-shot timer when `timeout_secs`
/// is set (event only, it does not kill the subprocess).
fn handle_workflow_run(
    req: &Request,
    mgr: &Rc<TabManager>,
    window: &ApplicationWindow,
    event_bus: &EventBus,
) -> Response {
    let id = match req.params.get("id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                "workflow.run requires non-empty `id` string",
            );
        }
    };

    let workflow_registry = mgr.workflow_registry();
    let spec = match workflow_registry.get(&id) {
        Some(s) => s.clone(),
        None => {
            return Response::error(
                req.id.clone(),
                "not_found",
                &format!("workflow id '{id}' not found"),
            );
        }
    };

    let values: std::collections::HashMap<String, String> = match req.params.get("values") {
        None | Some(serde_json::Value::Null) => std::collections::HashMap::new(),
        Some(serde_json::Value::Object(map)) => {
            let mut out = std::collections::HashMap::new();
            for (k, v) in map {
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => String::new(),
                    other => other.to_string(),
                };
                out.insert(k.clone(), s);
            }
            out
        }
        Some(other) => {
            return Response::error(
                req.id.clone(),
                "invalid_params",
                &format!("'values' must be an object, got {other}"),
            );
        }
    };

    if let Err(e) = copad_core::workflow::validate_values(&spec, &values) {
        return Response::error(req.id.clone(), "invalid_params", &e.to_string());
    }

    let explicit_project = req.params.get("project").and_then(|v| v.as_str());
    let registry = mgr.project_registry().lock().unwrap();
    let resolved_project: Option<copad_core::project::Project> =
        if let Some(name) = explicit_project {
            match registry.resolve_by_name(name) {
                Some(p) => Some(p.clone()),
                None => {
                    return Response::error(
                        req.id.clone(),
                        "not_found",
                        &format!("project '{name}' not found"),
                    );
                }
            }
        } else {
            registry.resolve_active(&mgr.context().snapshot()).cloned()
        };
    drop(registry);

    if spec.require_project && resolved_project.is_none() {
        return Response::error(
            req.id.clone(),
            "project_required",
            &format!(
                "workflow '{id}' requires a project — pass `project: \"<name>\"` or focus a \
                 pane whose pane_context.git_remote or active_cwd matches a configured project"
            ),
        );
    }

    // A resolved project wins; otherwise the active pane's cwd; otherwise $HOME so
    // project-agnostic workflows still get a usable directory instead of `no_workspace`.
    let workspace_path = resolve_workflow_workspace(
        resolved_project.as_ref(),
        mgr.context().snapshot().active_cwd,
    );

    // Merge unfilled optional fields as their `default` or "" before substitute, so
    // `cross-review` without `intent_brief` doesn't fail with `template_error: unknown
    // placeholder`. Required fields are validated above.
    let mut effective_values = values.clone();
    for field in &spec.form_fields {
        if !effective_values.contains_key(&field.name) {
            let default = field.default.clone().unwrap_or_default();
            effective_values.insert(field.name.clone(), default);
        }
    }
    let prompt = match copad_core::workflow::substitute(
        &spec.prompt,
        &effective_values,
        resolved_project.as_ref(),
        &workspace_path,
    ) {
        Ok(p) => p,
        Err(e) => {
            return Response::error(
                req.id.clone(),
                "template_error",
                &format!("workflow '{id}' prompt: {e}"),
            );
        }
    };

    if spec.default_team.is_some() {
        log::info!(
            "workflow.run: spec '{id}' has default_team set but it is inert — the pipeline \
             router was removed in Phase 24.7 (decision #51); dispatching via claude.start"
        );
    }
    if spec.default_model.is_some() {
        log::info!(
            "workflow.run: spec '{id}' has default_model set but it is inert — the Brain \
             dispatcher was removed in Phase 24.7 (decision #51); dispatching to claude"
        );
    }

    // Micros alone collide under same-microsecond concurrent dispatches; the seq lets
    // workflow.started / workflow.timed_out be correlated under load.
    let run_id = format!(
        "wfr-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0),
        next_fresh_session_seq()
    );

    let claude_params = json!({
        "workspace_path": workspace_path.display().to_string(),
        "prompt": prompt,
        "fresh_session": spec.fresh_session,
    });
    let claude_req = Request {
        id: req.id.clone(),
        method: "claude.start".to_string(),
        params: claude_params,
        target_client_id: req.target_client_id.clone(),
    };
    let claude_resp = handle_claude_start(&claude_req, mgr, window);
    if !claude_resp.ok {
        // Propagate verbatim: same shape as calling claude.start directly, with workflow.run's id.
        return claude_resp;
    }

    let claude_result = claude_resp
        .result
        .clone()
        .unwrap_or(serde_json::Value::Null);
    let panel_id = claude_result
        .get("panel_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tab = claude_result
        .get("tab")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    let project_name = resolved_project.as_ref().map(|p| p.name.clone());
    event_bus.publish(BusEvent::new(
        "workflow.started".to_string(),
        BUS_SOURCE_COPAD_LINUX,
        json!({
            "run_id": run_id,
            "workflow_id": spec.id,
            "project": project_name,
            "workspace_path": workspace_path.display().to_string(),
            "panel_id": panel_id,
            "tab": tab,
            "started_at_ms": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        }),
    ));

    if let Some(secs) = spec.timeout_secs {
        let bus_clone = event_bus.clone();
        let run_id_clone = run_id.clone();
        let workflow_id = spec.id.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(secs));
            // Emit-only: does NOT kill the claude subprocess.
            bus_clone.publish(BusEvent::new(
                "workflow.timed_out".to_string(),
                BUS_SOURCE_COPAD_LINUX,
                json!({
                    "run_id": run_id_clone,
                    "workflow_id": workflow_id,
                    "timeout_secs": secs,
                }),
            ));
        });
    }

    Response::success(
        req.id.clone(),
        json!({
            "run_id": run_id,
            "panel_id": panel_id,
            "tab": tab,
            "workspace_path": workspace_path.display().to_string(),
        }),
    )
}

/// Process-local counter breaking ties on `fresh_session` and `wfr-<...>` run ids when two calls
/// land in the same wall microsecond.
fn next_fresh_session_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Last two path components, so `<root>/feature/foo` does not collapse to `foo` and collide with
/// siblings sharing a leaf name.
fn derive_session_name(path: &std::path::Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for comp in path.components().rev() {
        if let std::path::Component::Normal(seg) = comp
            && let Some(s) = seg.to_str()
        {
            parts.push(s.to_string());
            if parts.len() == 2 {
                break;
            }
        }
    }
    parts.reverse();
    let joined = parts.join("-");
    sanitize_session_name(&joined)
}

fn sanitize_session_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let safe = if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        out.push(safe);
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "claude".to_string()
    } else {
        trimmed
    }
}

fn validate_tmux_session_name(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("cannot be empty".to_string());
    }
    if s.starts_with('-') {
        return Err("cannot start with '-' (would look like a flag)".to_string());
    }
    for c in s.chars() {
        if !(c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(format!(
                "invalid character {c:?} (allowed: ASCII alphanumeric and - _)"
            ));
        }
    }
    Ok(())
}

/// Waits for claude's REPL in `session_name`'s tmux pane, then pastes `prompt` via tmux
/// load-buffer + paste-buffer + Enter.
///
/// Trust boundary: `tmux new-session -A` attaches to an existing session and ignores our
/// `claude` command, and pasting into a pre-existing shell pane would EXECUTE the prompt as a
/// shell command. The seeder pastes only when BOTH gates pass:
/// 1. `pane_current_command` is claude (or the node binary it runs on); shells hard-skip.
/// 2. `capture-pane` shows claude-specific markers (banner / `Try "`); generic `> ` or
///    box-drawing is insufficient since shells emit those.
///
/// Failures log and are never propagated (claude.start already returned).
fn spawn_claude_prompt_seeder(session_name: String, prompt: String) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut saw_claude_marker = false;
        loop {
            if std::time::Instant::now() >= deadline {
                break;
            }
            if let Ok(out) = std::process::Command::new("tmux")
                .args(["capture-pane", "-p", "-t", &session_name])
                .output()
            {
                let s = String::from_utf8_lossy(&out.stdout);
                // Claude-specific markers only: generic "> " / box-drawing also match shells.
                if s.contains("Anthropic")
                    || s.contains("Try \"")
                    || s.contains("claude --")
                    || s.to_ascii_lowercase().contains("claude code")
                {
                    saw_claude_marker = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        // `pane_current_command` is the kernel's view, not the rendered text, so it survives
        // claude's banner scrolling off.
        let current_cmd = std::process::Command::new("tmux")
            .args([
                "display-message",
                "-p",
                "-t",
                &session_name,
                "#{pane_current_command}",
            ])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_lowercase())
            .unwrap_or_default();
        let pane_is_claude = matches!(current_cmd.as_str(), "claude" | "node");

        if !saw_claude_marker || !pane_is_claude {
            eprintln!(
                "[claude.start] refusing to paste prompt into session {session_name:?}: \
                 saw_claude_marker={saw_claude_marker}, pane_current_command={current_cmd:?}. \
                 Pre-existing tmux session may be a shell or a non-claude process; user \
                 can paste the prompt manually."
            );
            return;
        }

        // Through a temp file -> load-buffer -> paste-buffer, which keeps multi-line and
        // special-char payloads safe.
        let mut tmpf = match tempfile::NamedTempFile::new() {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[claude.start] tempfile failed: {e}");
                return;
            }
        };
        if let Err(e) = std::io::Write::write_all(&mut tmpf, prompt.as_bytes()) {
            eprintln!("[claude.start] write prompt failed: {e}");
            return;
        }
        let buf_name = format!("copad-claude-{}", uuid::Uuid::new_v4());
        let path_str = match tmpf.path().to_str() {
            Some(s) => s,
            None => {
                eprintln!("[claude.start] tempfile path is not UTF-8 — aborting seed");
                return;
            }
        };
        let load = std::process::Command::new("tmux")
            .args(["load-buffer", "-b", &buf_name, path_str])
            .status();
        if !matches!(load, Ok(s) if s.success()) {
            eprintln!("[claude.start] tmux load-buffer failed: {load:?}");
            return;
        }
        let paste = std::process::Command::new("tmux")
            .args(["paste-buffer", "-t", &session_name, "-b", &buf_name, "-d"])
            .status();
        if !matches!(paste, Ok(s) if s.success()) {
            eprintln!("[claude.start] tmux paste-buffer failed: {paste:?}");
            return;
        }
        // Submit. Claude's REPL needs TWO Enters for long pastes: a paste over its inline
        // threshold collapses to `[Pasted text #1 +N lines]`, the first Enter expands it and the
        // second sends it. For short pastes the second Enter hits an empty input and no-ops.
        let _ = std::process::Command::new("tmux")
            .args(["send-keys", "-t", &session_name, "Enter"])
            .status();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let _ = std::process::Command::new("tmux")
            .args(["send-keys", "-t", &session_name, "Enter"])
            .status();
    });
}

/// POSIX single-quote escape: wrap in `'...'`, replace embedded `'` with `'\''`.
fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn handle_agent_approve(cmd: SocketCommand, window: &ApplicationWindow, event_bus: &EventBus) {
    let req = &cmd.request;
    let title = req
        .params
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Agent Action");
    let message = match req.params.get("message").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => {
            cmd.reply_with_completion(
                event_bus,
                Response::error(req.id.clone(), "invalid_params", "Missing 'message' param"),
            );
            return;
        }
    };
    let actions = req
        .params
        .get("actions")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec!["Approve".to_string(), "Deny".to_string()]);

    let dialog = gtk4::AlertDialog::builder()
        .modal(true)
        .message(title)
        .detail(message)
        .buttons(actions.iter().map(|s| s.as_str()).collect::<Vec<_>>())
        .default_button(0)
        .cancel_button(actions.len() as i32 - 1)
        .build();

    let req_id = req.id.clone();
    let method = req.method.clone();
    let silent = cmd.silent_completion;
    let bus = event_bus.clone();
    let reply = cmd.reply;
    let actions_clone = actions.clone();
    dialog.choose(Some(window), gtk4::gio::Cancellable::NONE, move |result| {
        let resp = match result {
            Ok(idx) => {
                let action = actions_clone
                    .get(idx as usize)
                    .cloned()
                    .unwrap_or_else(|| "unknown".to_string());
                let approved = idx == 0;
                Response::success(
                    req_id.clone(),
                    json!({
                        "approved": approved,
                        "action": action,
                        "index": idx,
                    }),
                )
            }
            Err(_) => Response::success(
                req_id.clone(),
                json!({
                    "approved": false,
                    "action": "cancelled",
                    "index": -1,
                }),
            ),
        };
        copad_daemon::socket::publish_legacy_completion(&bus, &method, silent, &resp);
        let _ = reply.send(resp);
    });
}

fn handle_plugin_open(req: &Request, mgr: &Rc<TabManager>) -> Response {
    let plugin_name = match req.params.get("plugin").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return Response::error(req.id.clone(), "invalid_params", "Missing 'plugin' param"),
    };
    let panel_name = req
        .params
        .get("panel")
        .and_then(|v| v.as_str())
        .unwrap_or("main");

    let plugin = match copad_core::plugin::resolve_by_name(mgr.plugins(), plugin_name) {
        Some(p) => p.clone(),
        None => {
            return Response::error(
                req.id.clone(),
                "not_found",
                &format!("Plugin not found: {plugin_name}"),
            );
        }
    };

    match mgr.add_plugin_tab(&plugin, panel_name) {
        Some(panel_id) => Response::success(req.id.clone(), json!({ "panel_id": panel_id })),
        None => Response::error(
            req.id.clone(),
            "not_found",
            &format!("Panel '{panel_name}' not found in plugin '{plugin_name}'"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_tmux_session_name_accepts_normal() {
        for s in ["main", "feat-foo", "user_team", "release-1-2", "abc123"] {
            validate_tmux_session_name(s).unwrap_or_else(|e| panic!("rejected {s:?}: {e}"));
        }
    }

    #[test]
    fn validate_tmux_session_name_rejects_bad() {
        for s in [
            "",
            "-flag",
            "feat:foo",
            "feat.foo",
            "has space",
            "foo/bar",
            "x\0y",
        ] {
            assert!(
                validate_tmux_session_name(s).is_err(),
                "should reject {s:?}"
            );
        }
    }

    #[test]
    fn sanitize_session_name_lowercases_and_replaces_bad_chars() {
        assert_eq!(sanitize_session_name("Feature/Foo"), "feature-foo");
        assert_eq!(sanitize_session_name("v1.2.3"), "v1-2-3");
        assert_eq!(sanitize_session_name("ALL-CAPS"), "all-caps");
        assert_eq!(sanitize_session_name("---trim---"), "trim");
        // Empty after sanitizing falls back to a usable default.
        assert_eq!(sanitize_session_name("///"), "claude");
    }

    #[test]
    fn derive_session_name_uses_last_two_path_components() {
        let p = std::path::Path::new("/home/user/dev/copad-worktrees/feature/foo");
        assert_eq!(derive_session_name(p), "feature-foo");
        let p2 = std::path::Path::new("/home/user/dev/myrepo");
        assert_eq!(derive_session_name(p2), "dev-myrepo");
    }

    #[test]
    fn derive_session_name_sanitizes_uppercase_and_dots() {
        let p = std::path::Path::new("/x/Feature.Branch/PROJ-456");
        assert_eq!(derive_session_name(p), "feature-branch-proj-456");
    }

    #[test]
    fn resolve_workflow_workspace_prefers_project_then_cwd_then_home() {
        use copad_core::project::Project;
        let proj = Project {
            name: "copad".into(),
            path: PathBuf::from("/home/u/dev/copad"),
            subpath: None,
            description: None,
            aliases: vec![],
            git_remote: None,
        };
        assert_eq!(
            resolve_workflow_workspace(Some(&proj), Some(PathBuf::from("/elsewhere"))),
            PathBuf::from("/home/u/dev/copad")
        );
        let sub = Project {
            subpath: Some(PathBuf::from("crates/inner")),
            ..proj.clone()
        };
        assert_eq!(
            resolve_workflow_workspace(Some(&sub), None),
            PathBuf::from("/home/u/dev/copad/crates/inner")
        );
        assert_eq!(
            resolve_workflow_workspace(None, Some(PathBuf::from("/work/here"))),
            PathBuf::from("/work/here")
        );
        assert_eq!(resolve_workflow_workspace(None, None), home_dir());
    }

    #[test]
    fn shell_single_quote_round_trips() {
        assert_eq!(shell_single_quote("simple"), "'simple'");
        assert_eq!(shell_single_quote("foo'bar"), "'foo'\\''bar'");
        assert_eq!(shell_single_quote(""), "''");
        assert_eq!(shell_single_quote("a; b $C"), "'a; b $C'");
    }
}
