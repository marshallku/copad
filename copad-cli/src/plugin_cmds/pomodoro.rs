//! `coctl pomodoro` — ergonomic wrapper over the `pomodoro.*` action
//! surface exposed by `copad-plugin-pomodoro`.
//!
//! | CLI                                            | Action                  |
//! |------------------------------------------------|-------------------------|
//! | `pomodoro status`                              | `pomodoro.status`       |
//! | `pomodoro start [--minutes N] [--label TEXT]`  | `pomodoro.start`        |
//! | `pomodoro pause` / `resume` / `toggle` / `reset` | `pomodoro.{…}`         |
//! | `pomodoro set-durations [--work N] …`          | `pomodoro.set_durations`|
//!
//! `status` (and every mutating action) renders the one-line status text by
//! default — that is exactly what the plugin's `[[modules]]` entry shells
//! each second (`coctl pomodoro status`) to paint the status bar.

use clap::Subcommand;
use serde_json::{Value, json};

use crate::plugin_cmds::call_and_render;

#[derive(Subcommand, Debug)]
pub enum PomodoroCommand {
    /// Show the current timer state (prints the status-bar line)
    Status,
    /// Start (or restart) a work session
    Start {
        /// Override the work length in minutes for this session
        #[arg(long)]
        minutes: Option<u32>,
        /// Label shown next to the timer (e.g. what you're focusing on)
        #[arg(long)]
        label: Option<String>,
    },
    /// Freeze the countdown
    Pause,
    /// Resume a paused countdown
    Resume,
    /// Start if idle, pause if running, resume if paused
    Toggle,
    /// Stop and clear the timer
    Reset,
    /// Change the persisted durations (applies to future phases)
    #[command(name = "set-durations")]
    SetDurations {
        /// Work minutes
        #[arg(long)]
        work: Option<u32>,
        /// Short break minutes
        #[arg(long = "break")]
        break_min: Option<u32>,
        /// Long break minutes
        #[arg(long = "long-break")]
        long_break: Option<u32>,
        /// Work rounds before a long break
        #[arg(long)]
        rounds: Option<u32>,
    },
}

/// Print the `text` field the plugin renders; fall back to a compact
/// phase/remaining summary if it's somehow absent.
fn render_line(result: &Value) {
    if let Some(text) = result.get("text").and_then(Value::as_str) {
        println!("{text}");
    } else {
        let phase = result.get("phase").and_then(Value::as_str).unwrap_or("?");
        println!("pomodoro: {phase}");
    }
}

/// Insert an optional integer field into `obj` only when present, so the
/// plugin's "absent = keep current" semantics work.
fn put_opt(obj: &mut serde_json::Map<String, Value>, key: &str, val: Option<u32>) {
    if let Some(v) = val {
        obj.insert(key.to_string(), json!(v));
    }
}

pub fn dispatch(cmd: &PomodoroCommand, socket_path: &str, json_out: bool) -> i32 {
    let (method, params) = match cmd {
        PomodoroCommand::Status => ("pomodoro.status", json!({})),
        PomodoroCommand::Start { minutes, label } => {
            let mut obj = serde_json::Map::new();
            put_opt(&mut obj, "minutes", *minutes);
            if let Some(l) = label {
                obj.insert("label".to_string(), json!(l));
            }
            ("pomodoro.start", Value::Object(obj))
        }
        PomodoroCommand::Pause => ("pomodoro.pause", json!({})),
        PomodoroCommand::Resume => ("pomodoro.resume", json!({})),
        PomodoroCommand::Toggle => ("pomodoro.toggle", json!({})),
        PomodoroCommand::Reset => ("pomodoro.reset", json!({})),
        PomodoroCommand::SetDurations {
            work,
            break_min,
            long_break,
            rounds,
        } => {
            let mut obj = serde_json::Map::new();
            put_opt(&mut obj, "work_min", *work);
            put_opt(&mut obj, "break_min", *break_min);
            put_opt(&mut obj, "long_break_min", *long_break);
            put_opt(&mut obj, "rounds_before_long", *rounds);
            ("pomodoro.set_durations", Value::Object(obj))
        }
    };

    call_and_render(socket_path, method, params, json_out, render_line)
}
