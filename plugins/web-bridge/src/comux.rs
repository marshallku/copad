//! `comux` control-CLI client — the board's data source.
//!
//! This replaces [`crate::tmux`] as the surface the mobile board reads. That module's header
//! calls tmux "the primary data model for the dashboard", which stopped being true when the
//! user moved off tmux: with 10 comux sessions and 27 live agents on the machine,
//! `tmux list-panes` returns nothing and the phone renders an empty page.
//!
//! **Why shell out instead of speaking the socket.** Same rationale as [`crate::agents`]
//! (`tmx agents --json`): comux owns agent classification, the process sweep, status
//! inference and the session/tab model, and they evolve together. Re-implementing the wire
//! protocol here would couple this plugin to comux's internals and drift. The cost is one
//! fork per call, which is why the board makes exactly TWO per refresh and captures no panes.
//!
//! **Why `tokio::process` and not `std::process` in `spawn_blocking`.** `Command::output()`
//! cannot time out. comux's own client performs socket reads with no deadline
//! (`copad-mux/src/control.rs`), so a wedged or paused server makes the child hang forever —
//! and a polling phone would then accumulate one stuck child and one blocked task per refresh
//! until the plugin dies. Every call here runs under [`DEADLINE`] and the child is killed and
//! reaped when it expires.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Per-invocation wall-clock budget. A `comux` control call is a unix-socket round trip to a
/// process on this machine; anything beyond this is a wedged server, not a slow one.
const DEADLINE: Duration = Duration::from_secs(5);

/// One session (space) in a `comux list-sessions` response.
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
    /// How many of this session's panes run a classified agent.
    #[serde(default)]
    pub agents: usize,
}

/// One agent pane in a `comux list-agents` response.
///
/// `status` and `for_secs` are **cached and inferred** — comux documents this at the source:
/// Claude's status comes from `~/.claude/sessions/<pid>.json`, everything else falls back to
/// matching substrings on the pane's screen, so ordinary output can read as `blocked`, and
/// `idle` doubles as "no recognized UI". `for_secs` is time holding that inferred label, NOT
/// how long a task has run. The board must present both as a reading, never as fact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Agent {
    /// `$COPAD_MUX_PANE` identity — what `jump` / `capture-pane` address. May be empty for a
    /// pane spawned before pane tokens existed; `terminal` is the addressable fallback.
    #[serde(default)]
    pub token: String,
    pub terminal: String,
    #[serde(default)]
    pub space: String,
    /// Stable session id. Group by this; `space` is a display label that a rename changes
    /// and that two sessions may share.
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

/// Why a `comux` read did not produce data.
///
/// The split exists so the board can tell "there is nothing to show" from "we could not
/// look" — collapsing them would render an empty fleet as if every agent had vanished.
#[derive(Debug, Clone, PartialEq)]
pub enum ComuxError {
    /// The binary is not on the plugin's PATH.
    NotInstalled,
    /// No comux server is running. A legitimate empty state, not a fault.
    NoServer,
    /// The child outlived [`DEADLINE`] and was killed.
    Timeout,
    /// Ran, but reported failure (non-zero exit, or `ok: false`).
    Failed(String),
    /// Ran and succeeded, but the payload was not what this version expects. Deliberately
    /// NOT folded into an empty listing — a schema change must surface, not read as "no
    /// agents" (codex I2).
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
    /// A short stable tag for the API's `errors` array, so a client can branch without
    /// parsing prose.
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

/// Whether a non-zero exit means the command failed.
///
/// Most control verbs use the exit code the obvious way. `doctor` does not: it exits
/// `errors > 0 || warnings > 0`, so a server that is up and perfectly usable reports failure
/// merely because `mux.toml` carries a tolerated value or a process sweep once failed. Reading
/// its exit code as failure would make the phone refuse to open a terminal against a healthy
/// machine — so for `doctor` the JSON body IS the result, and the exit code is a diagnostic.
#[derive(Clone, Copy, PartialEq)]
enum ExitPolicy {
    /// Non-zero exit means the call failed.
    Strict,
    /// Non-zero exit is a diagnostic; trust the payload when there is one.
    PayloadWins,
}

/// Run one `comux <args> --json` under [`DEADLINE`], returning its stdout.
async fn run_json(args: &[&str]) -> Result<String, ComuxError> {
    run_json_with(args, ExitPolicy::Strict).await
}

async fn run_json_with(args: &[&str], policy: ExitPolicy) -> Result<String, ComuxError> {
    let mut cmd = tokio::process::Command::new("comux");
    cmd.args(args)
        .arg("--json")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    // `kill_on_drop` is what makes the timeout real: dropping the future on expiry sends
    // SIGKILL and tokio reaps the child, so a wedged server cannot leave one behind.
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
        // Under `PayloadWins` a non-zero exit with a body is still an answer; an EMPTY body is
        // not, and must not be smuggled through as a parse failure later.
        if policy == ExitPolicy::Strict || stdout.trim().is_empty() {
            return Err(classify_failure(&String::from_utf8_lossy(&out.stderr)));
        }
    }
    Ok(stdout)
}

/// Turn a failed `comux` invocation's stderr into an error kind.
///
/// Split out and tested because it rests on a STRING from another binary: comux signals
/// "there is no server" only in prose, so this match is a contract with a message that no
/// type checks. `no_server_is_recognised_from_the_real_message` pins the observed text, so a
/// reworded comux fails that test instead of silently downgrading an empty machine into a
/// scary `failed` badge on the phone.
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

/// Pull one named array out of a `{"ok":true, "<key>":[…]}` control response.
///
/// `ok: false` and a missing/!array key are distinct failures on purpose: the first is comux
/// refusing, the second means this plugin and that comux build disagree about the schema.
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

/// `comux list-sessions --json`.
pub async fn list_sessions() -> Result<Vec<Session>, ComuxError> {
    parse_envelope(&run_json(&["list-sessions"]).await?, "sessions")
}

/// `comux list-agents --json` — every agent pane across every session.
pub async fn list_agents() -> Result<Vec<Agent>, ComuxError> {
    parse_envelope(&run_json(&["list-agents"]).await?, "agents")
}

/// A pane token, as it may arrive from a browser.
///
/// Two separate jobs. The first is that `comux jump` takes its target positionally and skips any
/// argument starting with `-`, so a token of `--help` would silently become a usage error rather
/// than a jump. The second is that this string comes from a request body: the process is spawned
/// without a shell, so there is nothing to inject into, but a token is a `<nonce>-<counter>` or a
/// `termN`, and anything that is not one is a bug or an attack and gets a clean refusal either way.
pub fn is_pane_token(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 64
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !t.starts_with('-')
}

/// `comux jump <token> --no-raise --json` — move the mux's focus to that pane.
///
/// `--no-raise` is not optional here. The raise runs in the spawned CLI, which under the bridge is
/// a child of copadd: an environment with `DISPLAY`/`DBUS_SESSION_BUS_ADDRESS` scrubbed (#77), and
/// the wrong process for macOS to hang Automation permission on. There is also nothing to raise —
/// the person asking is holding a phone.
///
/// `PayloadWins`, because the interesting failure is a STRUCTURED one: an expired token answers
/// `{"ok":false,"error":"unknown pane: …"}` on stdout and then exits 1, and `Strict` would throw
/// that body away and report whatever stderr happened to hold. Pane tokens are qualified by server
/// incarnation, so "unknown pane" is the normal answer after `comux server restart` — the one case
/// where the phone MUST be told rather than quietly shown someone else's session.
pub async fn jump(token: &str) -> Result<(), ComuxError> {
    if !is_pane_token(token) {
        return Err(ComuxError::Failed(format!("not a pane token: {token:?}")));
    }
    let out = run_json_with(&["jump", token, "--no-raise"], ExitPolicy::PayloadWins).await?;
    parse_ok(&out)
}

/// Read a bare `{"ok": …}` control response — the shape a verb that returns no list answers with.
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
        // comux prints this on STDOUT and then exits 1. Strict policy would drop the body and
        // report stderr, which is empty — the phone would be told nothing at all.
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
        // The join key the board groups by — a rename must not move an agent.
        assert_eq!(got[0].space_id, "s1");
        assert_eq!(got[1].status, "working");
    }

    /// An older comux predates `space_id`; it must still list, just ungrouped, rather than
    /// failing the whole read.
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

    /// The failure this guards: a schema change silently reading as "the fleet is empty".
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

    /// Captured verbatim from `COPAD_MUX_SOCK=/tmp/absent.sock comux list-agents --json`
    /// (comux 1.2.0, exit 1). If comux rewords this, the board would start reporting a
    /// machine with no mux as a hard failure — this test is the tripwire.
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

/// What `comux doctor --json` says about the server: where its socket is, and whether it is up.
///
/// This is the ONE place the socket path comes from. Resolving it independently here would
/// duplicate `copad-mux/src/control.rs::socket_path`, which reads `COPAD_MUX_SOCK` else a
/// runtime dir derived from `XDG_RUNTIME_DIR`/`TMPDIR`/`USER` — so a divergence in any of
/// those between this process and the client it spawns would silently point the two at
/// different servers. Asking comux removes that class of bug, and the answer is then passed
/// explicitly to the client so both are pinned to the same socket.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerInfo {
    pub socket: String,
    pub running: bool,
}

/// Parse the `server` section out of a `comux doctor --json` document.
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
    // doctor reports the server as a finding, not a boolean. "running" is the only message
    // that means up; anything else (`not running`, a bind error) is down. Matching the
    // message keeps this honest about being a contract with another binary's prose — the
    // same coupling `classify_failure` carries, and pinned by a test the same way.
    let running = section
        .get("findings")
        .and_then(|f| f.as_array())
        .is_some_and(|fs| {
            fs.iter()
                .any(|f| f.get("message").and_then(|m| m.as_str()) == Some("running"))
        });
    Ok(ServerInfo { socket, running })
}

/// `comux doctor --json` — where the server's socket is and whether it is up.
pub async fn server_info() -> Result<ServerInfo, ComuxError> {
    parse_doctor(&run_json_with(&["doctor"], ExitPolicy::PayloadWins).await?)
}

#[cfg(test)]
mod doctor_tests {
    use super::*;

    /// Captured from `comux doctor --json` on this machine (comux 1.2.0).
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

    /// A down server still reports its path — that is what makes the path usable for a
    /// diagnostic message rather than only for a successful attach.
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

    /// The defect this guards: `comux doctor` exits 1 for any WARNING, so a running server on a
    /// machine with one tolerated config value would have made the phone refuse to open a
    /// terminal. The payload, not the exit code, is the answer.
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
