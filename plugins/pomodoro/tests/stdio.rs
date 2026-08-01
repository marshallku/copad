//! Integration test: drive the real `copad-plugin-pomodoro` binary over its
//! stdio JSON protocol (the wire contract copad's supervisor speaks).
//!
//! Covers the fast path — handshake, action invocation, `pomodoro.state`
//! emission, validation, and atomic persistence. The timer-expiry →
//! transition/toast path is a 60s wall-clock wait, so it stays in the
//! clock-injected unit tests (`state.rs`) rather than here.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_copad-plugin-pomodoro");

static SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_state_path() -> std::path::PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "copad-pomo-it-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("pomodoro.json")
}

struct Harness {
    child: Child,
    stdin: ChildStdin,
    lines: Arc<Mutex<Vec<String>>>,
    state_path: std::path::PathBuf,
}

impl Harness {
    fn start() -> Self {
        let state_path = unique_state_path();
        let mut child = Command::new(BIN)
            .env("COPAD_POMODORO_STATE", &state_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn pomodoro binary");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = lines.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                sink.lock().unwrap().push(line);
            }
        });
        Harness {
            child,
            stdin,
            lines,
            state_path,
        }
    }

    fn send(&mut self, frame: Value) {
        writeln!(self.stdin, "{frame}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn invoke(&mut self, id: &str, name: &str, params: Value) {
        self.send(json!({
            "id": id,
            "method": "action.invoke",
            "params": { "name": name, "params": params }
        }));
    }

    /// Block up to 2s for the first frame satisfying `pred`.
    fn wait_for(&self, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(v) = self
                .lines
                .lock()
                .unwrap()
                .iter()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .find(|v| pred(v))
            {
                return v;
            }
            if Instant::now() > deadline {
                panic!("timed out waiting; got: {:?}", self.lines.lock().unwrap());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The response frame for a given request id.
    fn response(&self, id: &str) -> Value {
        self.wait_for(|v| v.get("id").and_then(Value::as_str) == Some(id) && v.get("ok").is_some())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(dir) = self.state_path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

fn handshake(h: &mut Harness) {
    h.send(json!({ "id": "init", "method": "initialize", "params": { "protocol_version": 1 } }));
    let r = h.response("init");
    assert!(r["ok"].as_bool().unwrap(), "initialize ok");
    let provides = r["result"]["provides"].as_array().unwrap();
    assert!(
        provides.iter().any(|p| p == "pomodoro.start"),
        "advertises pomodoro.start"
    );
    h.send(json!({ "method": "initialized" }));
}

#[test]
fn start_status_pause_resume_reset() {
    let mut h = Harness::start();
    handshake(&mut h);

    h.invoke(
        "s1",
        "pomodoro.start",
        json!({ "minutes": 25, "label": "refactor" }),
    );
    let r = h.response("s1");
    assert_eq!(r["result"]["phase"], "work");
    assert_eq!(r["result"]["phase_total_ms"], 25 * 60_000);
    let text = r["result"]["text"].as_str().unwrap();
    assert!(text.starts_with("🍅 25:00"), "text was {text:?}");
    assert!(text.contains("refactor"));

    // start emitted a pomodoro.state event.
    h.wait_for(|v| {
        v.get("method").and_then(Value::as_str) == Some("event.publish")
            && v["params"]["kind"] == "pomodoro.state"
    });

    h.invoke("s2", "pomodoro.pause", json!({}));
    assert_eq!(h.response("s2")["result"]["paused"], true);

    h.invoke("s3", "pomodoro.resume", json!({}));
    assert_eq!(h.response("s3")["result"]["paused"], false);

    h.invoke("s4", "pomodoro.reset", json!({}));
    assert!(
        h.response("s4")["result"]["text"]
            .as_str()
            .unwrap()
            .contains("idle")
    );
}

#[test]
fn rejects_out_of_range_minutes_without_mutating() {
    let mut h = Harness::start();
    handshake(&mut h);
    h.invoke("bad", "pomodoro.start", json!({ "minutes": 9999 }));
    let r = h.response("bad");
    assert!(!r["ok"].as_bool().unwrap());
    assert_eq!(r["error"]["code"], "invalid_params");
    // Still idle.
    h.invoke("st", "pomodoro.status", json!({}));
    assert!(
        h.response("st")["result"]["text"]
            .as_str()
            .unwrap()
            .contains("idle")
    );
}

#[test]
fn set_durations_persists_to_state_file() {
    let mut h = Harness::start();
    handshake(&mut h);
    h.invoke(
        "d",
        "pomodoro.set_durations",
        json!({ "work_min": 50, "break_min": 10 }),
    );
    assert!(h.response("d")["ok"].as_bool().unwrap());

    // Give the atomic rename a moment, then read the file directly.
    let saved = h.wait_for_file();
    assert_eq!(saved["work_min"], 50);
    assert_eq!(saved["break_min"], 10);
    assert_eq!(saved["version"], 1);
}

#[test]
fn unknown_action_errors() {
    let mut h = Harness::start();
    handshake(&mut h);
    h.invoke("u", "pomodoro.nope", json!({}));
    let r = h.response("u");
    assert!(!r["ok"].as_bool().unwrap());
    assert_eq!(r["error"]["code"], "action_not_found");
}

impl Harness {
    fn wait_for_file(&self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(bytes) = std::fs::read(&self.state_path)
                && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
            {
                return v;
            }
            if Instant::now() > deadline {
                panic!("state file never appeared at {}", self.state_path.display());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
