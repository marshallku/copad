//! `comux` control-CLI client, the board's data source (replaces [`crate::tmux`] for the phone
//! board, since `tmux list-panes` returns nothing on a comux machine).
//!
//! Shells out instead of speaking the socket, like [`crate::agents`]: comux owns agent
//! classification, the process sweep and the session/tab model, and re-implementing its wire
//! protocol would drift. The cost is one fork per call, so the board makes exactly two per refresh.
//!
//! `tokio::process`, not `std::process` in `spawn_blocking`: `Command::output()` cannot time out,
//! and comux's own client reads the socket with no deadline (`copad-mux/src/control.rs`), so a
//! wedged server would leave one stuck child and blocked task per refresh. Every call runs under
//! [`DEADLINE`] and the child is killed and reaped on expiry.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Per-invocation wall-clock budget; beyond this is a wedged server, not a slow one.
const DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub index: u32,
    /// Stable id (`local`, `s1`, …). This is the join key, not `name`.
    pub id: String,
    /// Display name — what a rename changes.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub tabs: usize,
    #[serde(default)]
    pub panes: usize,
    #[serde(default)]
    pub agents: usize,
}

/// `status` and `for_secs` are cached and inferred: Claude's status comes from
/// `~/.claude/sessions/<pid>.json`, the rest from matching screen text, so ordinary output can
/// read as `blocked` and `idle` doubles as "no recognized UI". `for_secs` is time holding that
/// label, not task runtime. Present both as a reading, never as fact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Agent {
    /// May be empty for a pane spawned before pane tokens existed; `terminal` is the fallback.
    #[serde(default)]
    pub token: String,
    pub terminal: String,
    #[serde(default)]
    pub space: String,
    /// Group by this; `space` is a display label that a rename changes and two sessions may share.
    #[serde(default)]
    pub space_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub tool: String,
    pub status: String,
    pub for_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The split lets the board tell "nothing to show" from "could not look"; collapsing them would
/// render an empty fleet as if every agent vanished.
#[derive(Debug, Clone, PartialEq)]
pub enum ComuxError {
    NotInstalled,
    /// No comux server is running. A legitimate empty state, not a fault.
    NoServer,
    Timeout,
    Failed(String),
    /// Ran and succeeded, but the payload was not what this version expects. Not folded into an
    /// empty listing: a schema change must surface, not read as "no agents".
    Malformed(String),
}

impl std::fmt::Display for ComuxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => write!(f, "comux is not installed or not on PATH"),
            Self::NoServer => write!(f, "no comux server is running"),
            Self::Timeout => write!(f, "comux did not answer within {DEADLINE:?}"),
            Self::Failed(m) => write!(f, "comux failed: {m}"),
            Self::Malformed(m) => write!(f, "comux returned an unexpected payload: {m}"),
        }
    }
}

impl ComuxError {
    /// A short stable tag for the API's `errors` array.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotInstalled => "not_installed",
            Self::NoServer => "no_server",
            Self::Timeout => "timeout",
            Self::Failed(_) => "failed",
            Self::Malformed(_) => "malformed",
        }
    }
}

/// Whether a non-zero exit means the command failed. `doctor` exits `errors > 0 || warnings > 0`,
/// so a healthy usable server reports failure for a tolerated `mux.toml` value; for it the JSON
/// body is the result and the exit code a diagnostic.
#[derive(Clone, Copy, PartialEq)]
enum ExitPolicy {
    Strict,
    PayloadWins,
}

async fn run_json(args: &[&str]) -> Result<String, ComuxError> {
    run_json_with(args, ExitPolicy::Strict).await
}

async fn run_json_with(args: &[&str], policy: ExitPolicy) -> Result<String, ComuxError> {
    let mut cmd = tokio::process::Command::new("comux");
    cmd.args(args)
        .arg("--json")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    // `kill_on_drop` makes the timeout real: dropping the future on expiry SIGKILLs the child and
    // tokio reaps it.
    let out = match tokio::time::timeout(DEADLINE, cmd.output()).await {
        Err(_) => return Err(ComuxError::Timeout),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ComuxError::NotInstalled);
        }
        Ok(Err(e)) => return Err(ComuxError::Failed(format!("spawn comux: {e}"))),
        Ok(Ok(o)) => o,
    };
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        // Under `PayloadWins` a non-zero exit with a body is still an answer; an empty body is not.
        if policy == ExitPolicy::Strict || stdout.trim().is_empty() {
            return Err(classify_failure(&String::from_utf8_lossy(&out.stderr)));
        }
    }
    Ok(stdout)
}

/// Classify a failed invocation's stderr. comux signals "no server" only in prose, so this
/// matches a string from another binary; `no_server_is_recognised_from_the_real_message` pins
/// the observed text so a reworded comux fails that test instead of showing a `failed` badge.
fn classify_failure(stderr: &str) -> ComuxError {
    if stderr.contains("no running comux") {
        return ComuxError::NoServer;
    }
    ComuxError::Failed(first_line(stderr))
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Pull one named array out of a `{"ok":true, "<key>":[...]}` response. `ok: false` (comux
/// refusing) and a missing/non-array key (schema disagreement) are distinct failures.
pub fn parse_envelope<T: serde::de::DeserializeOwned>(
    stdout: &str,
    key: &str,
) -> Result<Vec<T>, ComuxError> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| ComuxError::Malformed(format!("not JSON: {e}")))?;
    if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
        let msg = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("ok was not true");
        return Err(ComuxError::Failed(msg.to_string()));
    }
    let Some(arr) = v.get(key) else {
        return Err(ComuxError::Malformed(format!("no `{key}` in the response")));
    };
    serde_json::from_value(arr.clone())
        .map_err(|e| ComuxError::Malformed(format!("`{key}` did not match this build: {e}")))
}

pub async fn list_sessions() -> Result<Vec<Session>, ComuxError> {
    parse_envelope(&run_json(&["list-sessions"]).await?, "sessions")
}

pub async fn list_agents() -> Result<Vec<Agent>, ComuxError> {
    parse_envelope(&run_json(&["list-agents"]).await?, "agents")
}

/// A pane token as it may arrive from a browser. `comux jump` skips positional arguments that
/// start with `-`, so `--help` would become a usage error. The process is spawned without a
/// shell, but a token is `<nonce>-<counter>` or `termN`, and anything else is refused cleanly.
pub fn is_pane_token(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 64
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !t.starts_with('-')
}

/// `comux jump <token> --no-raise --json`: move the mux's focus to that pane. `--no-raise` is
/// required: the raise would run in a CLI that is a child of copadd, whose environment has
/// `DISPLAY`/`DBUS_SESSION_BUS_ADDRESS` scrubbed (#77) and which is the wrong process for macOS to
/// hang Automation permission on; besides, the person asking holds a phone.
///
/// `PayloadWins`: an expired token answers `{"ok":false,"error":"unknown pane: ..."}` on stdout
/// then exits 1, and `Strict` would discard that body. Tokens are qualified by server
/// incarnation, so "unknown pane" is the normal answer after `comux server restart`, and the
/// phone must be told rather than shown someone else's session.
pub async fn jump(token: &str) -> Result<(), ComuxError> {
    if !is_pane_token(token) {
        return Err(ComuxError::Failed(format!("not a pane token: {token:?}")));
    }
    let out = run_json_with(&["jump", token, "--no-raise"], ExitPolicy::PayloadWins).await?;
    parse_ok(&out)
}

pub fn parse_ok(stdout: &str) -> Result<(), ComuxError> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| ComuxError::Malformed(format!("not JSON: {e}")))?;
    if v.get("ok").and_then(|b| b.as_bool()) == Some(true) {
        return Ok(());
    }
    Err(ComuxError::Failed(
        v.get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("ok was not true")
            .to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_token_keeps_its_reason() {
        // comux prints this on stdout then exits 1; Strict would drop the body and report empty
        // stderr.
        let err = parse_ok(r#"{"ok":false,"error":"unknown pane: 17bca6ab617c6-4"}"#).unwrap_err();
        assert!(
            matches!(&err, ComuxError::Failed(m) if m.contains("unknown pane")),
            "{err:?}"
        );
    }

    #[test]
    fn a_successful_jump_needs_no_payload() {
        assert!(parse_ok(r#"{"ok":true}"#).is_ok());
    }

    #[test]
    fn a_token_that_would_be_read_as_a_flag_is_refused() {
        assert!(is_pane_token("17bca6ab617c6-4"));
        assert!(is_pane_token("term4"));
        assert!(!is_pane_token("--no-raise"));
        assert!(!is_pane_token(""));
        assert!(!is_pane_token("a b"));
        assert!(!is_pane_token(&"a".repeat(65)));
    }

    const SESSIONS: &str = r#"{"ok":true,"sessions":[
        {"index":0,"id":"local","name":"blog","active":false,"tabs":4,"panes":4,"agents":0},
        {"index":1,"id":"s1","name":"copad","active":true,"tabs":9,"panes":9,"agents":4}]}"#;

    const AGENTS: &str = r#"{"ok":true,"agents":[
        {"token":"949c6aa42bc1-4","terminal":"term4","space":"copad","space_id":"s1",
         "title":"tab 1","tool":"claude","status":"ready","for_secs":793964},
        {"token":"949c6aa42bc1-8","terminal":"term8","space":"copad","space_id":"s1",
         "title":"tab 5","tool":"claude","status":"working","for_secs":297}]}"#;

    #[test]
    fn sessions_parse_from_a_real_response() {
        let got: Vec<Session> = parse_envelope(SESSIONS, "sessions").expect("must parse");
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].id, "s1");
        assert!(got[1].active);
        assert_eq!(got[1].agents, 4);
    }

    #[test]
    fn agents_parse_and_carry_the_stable_session_id() {
        let got: Vec<Agent> = parse_envelope(AGENTS, "agents").expect("must parse");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].space_id, "s1");
        assert_eq!(got[1].status, "working");
    }

    /// An older comux predates `space_id`; it must still list, just ungrouped.
    #[test]
    fn a_response_without_space_id_still_parses() {
        let older = r#"{"ok":true,"agents":[{"token":"t-1","terminal":"term1",
            "space":"copad","title":"tab 1","tool":"claude","status":"ready","for_secs":5}]}"#;
        let got: Vec<Agent> = parse_envelope(older, "agents").expect("must parse");
        assert_eq!(got[0].space_id, "");
    }

    #[test]
    fn ok_false_is_a_failure_not_an_empty_listing() {
        let err = parse_envelope::<Agent>(r#"{"ok":false,"error":"unknown pane 'x'"}"#, "agents")
            .expect_err("must not succeed");
        assert_eq!(err, ComuxError::Failed("unknown pane 'x'".into()));
    }

    /// Guards a schema change silently reading as "the fleet is empty".
    #[test]
    fn a_missing_collection_is_malformed_not_empty() {
        let err = parse_envelope::<Agent>(r#"{"ok":true,"panes":[]}"#, "agents")
            .expect_err("must not succeed");
        assert!(matches!(err, ComuxError::Malformed(_)), "got {err:?}");
    }

    #[test]
    fn a_wrongly_shaped_collection_is_malformed_not_empty() {
        let err = parse_envelope::<Agent>(r#"{"ok":true,"agents":[{"nope":1}]}"#, "agents")
            .expect_err("must not succeed");
        assert!(matches!(err, ComuxError::Malformed(_)), "got {err:?}");
    }

    /// Captured verbatim from `COPAD_MUX_SOCK=/tmp/absent.sock comux list-agents --json` (comux
    /// 1.2.0, exit 1). If comux rewords it, this test is the tripwire.
    #[test]
    fn no_server_is_recognised_from_the_real_message() {
        let real = "comux: no running comux at /tmp/absent.sock \
                    (No such file or directory (os error 2)). Start one, or set COPAD_MUX_SOCK.";
        assert_eq!(classify_failure(real), ComuxError::NoServer);
    }

    #[test]
    fn any_other_stderr_is_a_plain_failure_carrying_its_first_line() {
        let err = classify_failure("\ncomux: unknown command 'list-agent'\nusage: ...\n");
        assert_eq!(
            err,
            ComuxError::Failed("comux: unknown command 'list-agent'".into())
        );
    }

    #[test]
    fn non_json_output_is_malformed() {
        let err = parse_envelope::<Session>("comux: usage ...", "sessions")
            .expect_err("must not succeed");
        assert!(matches!(err, ComuxError::Malformed(_)), "got {err:?}");
    }
}

/// What `comux doctor --json` says about the server: its socket path and whether it is up. The
/// one place the socket path comes from: resolving it here would duplicate
/// `copad-mux/src/control.rs::socket_path`, and a divergence in `XDG_RUNTIME_DIR`/`TMPDIR`/`USER`
/// would point the two at different servers. The answer is passed to the client explicitly.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerInfo {
    pub socket: String,
    pub running: bool,
}

pub fn parse_doctor(stdout: &str) -> Result<ServerInfo, ComuxError> {
    let v: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| ComuxError::Malformed(format!("doctor is not JSON: {e}")))?;
    let section = v
        .get("sections")
        .and_then(|s| s.as_array())
        .and_then(|a| {
            a.iter()
                .find(|s| s.get("title").and_then(|t| t.as_str()) == Some("server"))
        })
        .ok_or_else(|| ComuxError::Malformed("doctor has no `server` section".into()))?;
    let socket = section
        .get("path")
        .and_then(|p| p.as_str())
        .ok_or_else(|| ComuxError::Malformed("the `server` section has no `path`".into()))?
        .to_string();
    // doctor reports the server as a finding. "running" is the only message that means up. Like
    // `classify_failure`, this is a contract with another binary's prose, pinned by a test.
    let running = section
        .get("findings")
        .and_then(|f| f.as_array())
        .is_some_and(|fs| {
            fs.iter()
                .any(|f| f.get("message").and_then(|m| m.as_str()) == Some("running"))
        });
    Ok(ServerInfo { socket, running })
}

pub async fn server_info() -> Result<ServerInfo, ComuxError> {
    parse_doctor(&run_json_with(&["doctor"], ExitPolicy::PayloadWins).await?)
}

#[cfg(test)]
mod doctor_tests {
    use super::*;

    const DOCTOR: &str = r#"{"errors":0,"warnings":0,"sections":[
        {"title":"config.toml","path":"/Users/u/.config/copad/config.toml",
         "findings":[{"level":"ok","message":"parses"}]},
        {"title":"server","path":"/var/folders/T/copad-mux-u/sock",
         "findings":[{"level":"ok","message":"running"},
                     {"level":"note","message":"56 panes, 56 labeled"}]}]}"#;

    #[test]
    fn the_socket_path_and_running_state_come_from_the_server_section() {
        let got = parse_doctor(DOCTOR).expect("must parse");
        assert_eq!(got.socket, "/var/folders/T/copad-mux-u/sock");
        assert!(got.running);
    }

    /// A down server still reports its path, so it can appear in a diagnostic message.
    #[test]
    fn a_server_that_is_not_running_is_reported_as_down_with_its_path() {
        let down = DOCTOR.replace(
            r#"{"level":"ok","message":"running"}"#,
            r#"{"level":"warn","message":"not running"}"#,
        );
        let got = parse_doctor(&down).expect("must parse");
        assert_eq!(got.socket, "/var/folders/T/copad-mux-u/sock");
        assert!(!got.running, "only the literal `running` finding means up");
    }

    /// `comux doctor` exits 1 for any warning, so a running server with one tolerated config value
    /// must not make the phone refuse to open a terminal; the payload is the answer.
    #[test]
    fn a_warning_only_doctor_still_reports_a_running_server() {
        let warned = DOCTOR
            .replace(r#""warnings":0"#, r#""warnings":1"#)
            .replace(
                r#"{"level":"ok","message":"parses"}"#,
                r#"{"level":"warn","message":"unknown key `nope`"}"#,
            );
        let got = parse_doctor(&warned).expect("must parse");
        assert!(
            got.running,
            "a config warning must not read as a down server"
        );
    }

    #[test]
    fn a_doctor_without_a_server_section_is_malformed() {
        let err = parse_doctor(r#"{"sections":[{"title":"mux.toml","path":"/x"}]}"#)
            .expect_err("must not succeed");
        assert!(matches!(err, ComuxError::Malformed(_)), "got {err:?}");
    }
}
