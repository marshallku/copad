//! Thin wrapper over the `csd` CLI (claude/codex session driver). Every
//! call is an argv-vector `Command` — never a shell string — so a goal's
//! cwd / instruction can't inject extra flags. `csd` prints JSON on
//! stdout on success and `{"error": "..."}` on stderr with a non-zero
//! exit on failure; those CLI failures are out-of-band from the `state`
//! status enum (codex C7), so every call also enforces a wall-clock
//! timeout and surfaces missing-binary / tmux / hung-command failures as
//! `CsdError`.

use std::fmt;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

#[derive(Debug)]
pub struct CsdError {
    pub message: String,
}

impl CsdError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for CsdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug)]
pub struct SpawnInfo {
    pub session_id: Option<String>,
    pub jsonl_path: Option<String>,
}

/// One verdict from `csd state` (and the `state` field of `csd ps`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsdState {
    Spawning,
    Working,
    AwaitingAnswer {
        question: String,
    },
    PlanReady {
        plan_file: Option<String>,
        plan: Option<String>,
    },
    Blocked {
        gate: String,
        prompt: Option<String>,
        options: Option<String>,
    },
    IdleDone {
        text: String,
    },
    Dead,
    Unknown,
}

impl CsdState {
    pub fn from_json(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        match v.get("status").and_then(Value::as_str) {
            Some("spawning") => CsdState::Spawning,
            Some("working") => CsdState::Working,
            Some("awaiting_answer") => CsdState::AwaitingAnswer {
                question: s("question").unwrap_or_default(),
            },
            Some("plan_ready") => CsdState::PlanReady {
                plan_file: s("plan_file"),
                plan: s("plan"),
            },
            Some("blocked") => CsdState::Blocked {
                gate: s("gate").unwrap_or_else(|| "permission".into()),
                prompt: s("prompt"),
                options: v.get("options").map(|o| o.to_string()),
            },
            Some("idle_done") => CsdState::IdleDone {
                text: s("text").unwrap_or_default(),
            },
            Some("dead") => CsdState::Dead,
            _ => CsdState::Unknown,
        }
    }
}

#[derive(Clone)]
pub struct Csd {
    bin: String,
    timeout: Duration,
}

impl Csd {
    pub fn from_env() -> Self {
        let bin = std::env::var("COPAD_PILOT_CSD_BIN").unwrap_or_else(|_| "csd".to_string());
        let timeout = std::env::var("COPAD_PILOT_CSD_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60);
        Self {
            bin,
            timeout: Duration::from_secs(timeout),
        }
    }

    fn run(&self, args: &[String]) -> Result<Value, CsdError> {
        let mut child = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                CsdError::new(format!(
                    "cannot run `{}` (is csd on PATH? set COPAD_PILOT_CSD_BIN): {e}",
                    self.bin
                ))
            })?;

        // Drain both pipes on their own threads so a large `csd` stderr
        // (a panic / backtrace) can't fill the pipe buffer and wedge the
        // child before it exits — which would turn the real error into a
        // bare timeout.
        let mut out_pipe = child.stdout.take();
        let mut err_pipe = child.stderr.take();
        let out_h = thread::spawn(move || {
            let mut s = String::new();
            if let Some(p) = out_pipe.as_mut() {
                let _ = p.read_to_string(&mut s);
            }
            s
        });
        let err_h = thread::spawn(move || {
            let mut s = String::new();
            if let Some(p) = err_pipe.as_mut() {
                let _ = p.read_to_string(&mut s);
            }
            s
        });

        let deadline = Instant::now() + self.timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let out = out_h.join().unwrap_or_default();
                    let err = err_h.join().unwrap_or_default();
                    if status.success() {
                        return serde_json::from_str(out.trim()).map_err(|e| {
                            CsdError::new(format!(
                                "csd {} returned unparseable stdout ({e}): {}",
                                args.first().cloned().unwrap_or_default(),
                                out.trim()
                            ))
                        });
                    }
                    return Err(CsdError::new(extract_error(&err, status.code())));
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(CsdError::new(format!(
                            "csd {} timed out after {:?}",
                            args.first().cloned().unwrap_or_default(),
                            self.timeout
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(CsdError::new(format!("waiting on csd failed: {e}"))),
            }
        }
    }

    pub fn spawn(&self, cwd: &str, name: &str, posture: &str) -> Result<SpawnInfo, CsdError> {
        // `--cwd=<path>` / `--name=<name>` (attached form) so a value that
        // starts with `-` can't be reparsed as a flag.
        let mut args = vec![
            "spawn".into(),
            format!("--cwd={cwd}"),
            format!("--name={name}"),
        ];
        for flag in posture_flags(posture) {
            args.push(flag.into());
        }
        let v = self.run(&args)?;
        Ok(SpawnInfo {
            session_id: v
                .get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            jsonl_path: v
                .get("jsonl_path")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    pub fn send(&self, name: &str, prompt: &str) -> Result<(), CsdError> {
        // `--` ends option parsing so a prompt starting with `-` is taken
        // as text, not a flag (csd's `send` positional is `trailing_var_arg`
        // but still parses a leading `-foo` as CLI syntax without this).
        self.run(&["send".into(), name.into(), "--".into(), prompt.into()])?;
        Ok(())
    }

    pub fn state(&self, name: &str) -> Result<CsdState, CsdError> {
        let v = self.run(&["state".into(), name.into()])?;
        Ok(CsdState::from_json(&v))
    }

    pub fn approve(&self, name: &str, option: u32) -> Result<(), CsdError> {
        self.run(&[
            "approve".into(),
            name.into(),
            "--option".into(),
            option.to_string(),
        ])?;
        Ok(())
    }

    pub fn kill(&self, name: &str) -> Result<(), CsdError> {
        self.run(&["kill".into(), name.into()])?;
        Ok(())
    }
}

/// Map a pilot posture keyword to `csd spawn` flags. Default (`trust`)
/// auto-clears only the one-time folder-trust gate so unattended spawns
/// are driveable; permission gates still surface as `AwaitingGate`.
fn posture_flags(posture: &str) -> Vec<&'static str> {
    match posture {
        "yolo" => vec!["--yolo"],
        "bypass" => vec!["--bypass-permissions", "--trust"],
        "auto-accept" => vec!["--auto-accept", "--trust"],
        "default" => vec![],
        _ => vec!["--trust"],
    }
}

fn extract_error(stderr: &str, code: Option<i32>) -> String {
    let trimmed = stderr.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed)
        && let Some(msg) = v.get("error").and_then(Value::as_str)
    {
        return msg.to_string();
    }
    if trimmed.is_empty() {
        format!("csd exited with status {code:?}")
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_each_state_variant() {
        assert_eq!(
            CsdState::from_json(&json!({"status": "spawning"})),
            CsdState::Spawning
        );
        assert_eq!(
            CsdState::from_json(&json!({"status": "working", "tools": ["Bash"]})),
            CsdState::Working
        );
        assert_eq!(
            CsdState::from_json(&json!({"status": "awaiting_answer", "question": "which dir?"})),
            CsdState::AwaitingAnswer {
                question: "which dir?".into()
            }
        );
        assert_eq!(
            CsdState::from_json(&json!({"status": "idle_done", "text": "DONE:g:1"})),
            CsdState::IdleDone {
                text: "DONE:g:1".into()
            }
        );
        assert_eq!(
            CsdState::from_json(&json!({"status": "dead"})),
            CsdState::Dead
        );
        // Unrecognized / missing status → Unknown (driver waits, never false-completes).
        assert_eq!(
            CsdState::from_json(&json!({"status": "wat"})),
            CsdState::Unknown
        );
        assert_eq!(CsdState::from_json(&json!({})), CsdState::Unknown);
    }

    #[test]
    fn blocked_defaults_gate_and_keeps_options() {
        let s = CsdState::from_json(
            &json!({"status": "blocked", "gate": "permission", "prompt": "ok?", "options": ["Yes", "No"]}),
        );
        match s {
            CsdState::Blocked {
                gate,
                prompt,
                options,
            } => {
                assert_eq!(gate, "permission");
                assert_eq!(prompt.as_deref(), Some("ok?"));
                assert!(options.unwrap().contains("Yes"));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    #[test]
    fn posture_flags_map_correctly() {
        assert_eq!(posture_flags("trust"), vec!["--trust"]);
        assert_eq!(posture_flags(""), vec!["--trust"]);
        assert_eq!(posture_flags("yolo"), vec!["--yolo"]);
        assert_eq!(
            posture_flags("auto-accept"),
            vec!["--auto-accept", "--trust"]
        );
        assert!(posture_flags("default").is_empty());
    }

    #[test]
    fn extract_error_prefers_json_error_field() {
        assert_eq!(
            extract_error("{\"error\": \"no such session\"}", Some(1)),
            "no such session"
        );
        assert_eq!(extract_error("raw boom", Some(2)), "raw boom");
        assert_eq!(
            extract_error("", Some(127)),
            "csd exited with status Some(127)"
        );
    }

    #[test]
    fn missing_binary_surfaces_as_error() {
        let csd = Csd {
            bin: "definitely-not-a-real-binary-xyz".into(),
            timeout: Duration::from_secs(5),
        };
        let err = csd.state("whatever").unwrap_err();
        assert!(err.message.contains("cannot run"), "got {}", err.message);
    }
}
