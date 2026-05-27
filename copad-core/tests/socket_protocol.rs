use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use copad_core::protocol::{Request, Response};
use serde_json::json;

/// Minimal socket server matching copad-linux/src/socket.rs logic
fn start_test_server(socket_path: &str) -> mpsc::Receiver<(Request, mpsc::Sender<Response>)> {
    let (tx, rx) = mpsc::channel();
    let listener = UnixListener::bind(socket_path).expect("bind failed");

    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = stream.unwrap();
            let tx = tx.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                for line in reader.lines() {
                    let line = line.unwrap();
                    if line.is_empty() {
                        continue;
                    }
                    let req: Request = serde_json::from_str(&line).unwrap();
                    let (reply_tx, reply_rx) = mpsc::channel();
                    tx.send((req, reply_tx)).unwrap();
                    let resp = reply_rx.recv().unwrap();
                    writeln!(writer, "{}", serde_json::to_string(&resp).unwrap()).unwrap();
                    writer.flush().unwrap();
                }
            });
        }
    });

    rx
}

/// Minimal dispatch matching copad-linux/src/socket.rs dispatch()
fn dispatch(req: &Request) -> Response {
    match req.method.as_str() {
        "system.ping" => Response::success(req.id.clone(), json!({"status": "ok"})),
        "background.set" => {
            let path = req.params.get("path").and_then(|v| v.as_str());
            match path {
                Some(_) => Response::success(req.id.clone(), json!({"status": "ok"})),
                None => Response::error(req.id.clone(), "invalid_params", "Missing 'path' param"),
            }
        }
        "background.set_tint" => {
            let opacity = req.params.get("opacity").and_then(|v| v.as_f64());
            match opacity {
                Some(_) => Response::success(req.id.clone(), json!({"status": "ok"})),
                None => {
                    Response::error(req.id.clone(), "invalid_params", "Missing 'opacity' param")
                }
            }
        }
        "tab.list" => Response::success(req.id.clone(), json!({"count": 1, "current": 0})),
        _ => Response::error(
            req.id.clone(),
            "unknown_method",
            &format!("Unknown method: {}", req.method),
        ),
    }
}

/// Client helper matching copad-cli/src/client.rs
fn send_command(socket_path: &str, method: &str, params: serde_json::Value) -> Response {
    let stream = UnixStream::connect(socket_path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();

    let request = Request::new("test-id", method, params);
    writeln!(writer, "{}", serde_json::to_string(&request).unwrap()).unwrap();
    writer.flush().unwrap();

    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.unwrap();
        if line.is_empty() {
            continue;
        }
        return serde_json::from_str(&line).unwrap();
    }
    panic!("no response");
}

struct TestServer {
    socket_path: String,
    _dispatch_handle: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn new(name: &str) -> Self {
        let socket_path = format!("/tmp/copad-test-{name}-{}.sock", std::process::id());
        let _ = std::fs::remove_file(&socket_path);

        let rx = start_test_server(&socket_path);

        let handle = thread::spawn(move || {
            loop {
                match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok((req, reply_tx)) => {
                        let resp = dispatch(&req);
                        let _ = reply_tx.send(resp);
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });

        // Wait for server to be ready
        thread::sleep(Duration::from_millis(100));

        Self {
            socket_path,
            _dispatch_handle: Some(handle),
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[test]
fn test_ping() {
    let server = TestServer::new("ping");
    let resp = send_command(&server.socket_path, "system.ping", json!({}));
    assert!(resp.ok);
    assert_eq!(resp.result.unwrap()["status"], "ok");
}

#[test]
fn test_unknown_method_returns_error() {
    let server = TestServer::new("unknown");
    let resp = send_command(&server.socket_path, "bogus.method", json!({}));
    assert!(!resp.ok);
    let err = resp.error.unwrap();
    assert_eq!(err.code, "unknown_method");
    assert!(err.message.contains("bogus.method"));
}

#[test]
fn test_background_set_missing_path() {
    let server = TestServer::new("bg-no-path");
    let resp = send_command(&server.socket_path, "background.set", json!({}));
    assert!(!resp.ok);
    assert_eq!(resp.error.unwrap().code, "invalid_params");
}

#[test]
fn test_background_set_with_path() {
    let server = TestServer::new("bg-path");
    let resp = send_command(
        &server.socket_path,
        "background.set",
        json!({"path": "/tmp/test.jpg"}),
    );
    assert!(resp.ok);
}

#[test]
fn test_background_set_tint() {
    let server = TestServer::new("tint");
    let resp = send_command(
        &server.socket_path,
        "background.set_tint",
        json!({"opacity": 0.5}),
    );
    assert!(resp.ok);
}

#[test]
fn test_background_set_tint_missing_param() {
    let server = TestServer::new("tint-miss");
    let resp = send_command(&server.socket_path, "background.set_tint", json!({}));
    assert!(!resp.ok);
    assert_eq!(resp.error.unwrap().code, "invalid_params");
}

#[test]
fn test_tab_list() {
    let server = TestServer::new("tab-list");
    let resp = send_command(&server.socket_path, "tab.list", json!({}));
    assert!(resp.ok);
    let result = resp.result.unwrap();
    assert_eq!(result["count"], 1);
    assert_eq!(result["current"], 0);
}

#[test]
fn test_multiple_commands_one_connection() {
    let server = TestServer::new("multi");
    let stream = UnixStream::connect(&server.socket_path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);

    for i in 0..3 {
        let req = Request::new(format!("req-{i}"), "system.ping", json!({}));
        writeln!(writer, "{}", serde_json::to_string(&req).unwrap()).unwrap();
        writer.flush().unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let resp: Response = serde_json::from_str(&line).unwrap();
        assert!(resp.ok, "failed on iteration {i}");
    }
}

#[test]
fn test_response_id_matches_request() {
    let server = TestServer::new("id-match");
    let stream = UnixStream::connect(&server.socket_path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);

    let req = Request::new("unique-id-42", "system.ping", json!({}));
    writeln!(writer, "{}", serde_json::to_string(&req).unwrap()).unwrap();
    writer.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let resp: Response = serde_json::from_str(&line).unwrap();
    assert_eq!(resp.id, "unique-id-42");
}
