//! Per-plugin CLI subcommands. Each module exposes a clap `Subcommand` enum and a `dispatch`
//! entrypoint that builds the action params, optionally resolves id shorthands with a preflight
//! call, and renders the response as JSON (`--json`) or human text. Everything is sugar over
//! `coctl call <action> --params '{...}'`.

pub mod bookmark;
pub mod calendar;
pub mod context;
pub mod git;
pub mod jira;
pub mod pomodoro;
pub mod recent;
pub mod slack;
pub mod todo;

use serde_json::Value;

use crate::client;

/// Shared call+render. `json_out` dumps `result` as pretty JSON;
/// otherwise the `human` callback renders. Returns 0 on success,
/// 1 on transport error / `ok: false`.
pub fn call_and_render(
    socket_path: &str,
    method: &str,
    params: Value,
    json_out: bool,
    human: impl FnOnce(&Value),
) -> i32 {
    let resp = match client::send_command(socket_path, method, params) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to connect: {e}");
            return 1;
        }
    };
    if !resp.ok {
        if let Some(err) = resp.error {
            eprintln!("Error [{}]: {}", err.code, err.message);
        }
        return 1;
    }
    let result = resp.result.unwrap_or(Value::Null);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    } else {
        human(&result);
    }
    0
}
