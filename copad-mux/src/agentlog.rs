//! `comux agent-log`: the last few entries of another agent's OWN conversation log.
//!
//! `capture-pane` is a poor way to follow an agent: the bottom of its screen is its input box,
//! status line and session title, and the rest is whatever its TUI chose to redraw. The
//! transcript the agent writes for itself (Claude's `~/.claude/projects/…/<id>.jsonl`, Codex's
//! rollout) is the actual conversation. This reads its TAIL — never the whole file, which
//! runs to hundreds of megabytes — and normalizes both formats into one entry shape.
//!
//! Both formats are undocumented and move. Anything unrecognized is skipped, never an error:
//! a log with nothing readable yields no entries, which the caller reports as such.

use serde::Serialize;
use std::path::Path;

/// How much of the log's end is read. Claude interleaves large attachment and tool-result
/// records with the conversation, so this has to cover far more than `--last` entries.
const TAIL_BUDGET: u64 = 8 * 1024 * 1024;

/// One conversation entry, in either tool's log.
#[derive(Debug, Serialize, PartialEq)]
pub struct Entry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    /// `user`, `assistant`, or `tool` (a tool's result, fed back to the model).
    pub role: &'static str,
    /// `text`, `tool_use`, `tool_result`, or `thinking`.
    pub kind: &'static str,
    /// The tool's name, on a `tool_use`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub text: String,
}

/// What [`read`] found.
pub struct Log {
    /// Oldest first.
    pub entries: Vec<Entry>,
    /// The read stopped short of the file's start, so there may be older entries — and fewer
    /// than asked for does NOT mean the conversation is that short.
    pub partial: bool,
}

/// The last `last` entries of the `tool` log at `path`, each text cut to `max_chars` chars (0
/// = uncut). `last` counts ENTRIES, not log records: one assistant turn that says something
/// and calls a tool is two.
pub fn read(path: &Path, tool: &str, last: usize, max_chars: usize) -> Result<Log, String> {
    let parse: fn(&str) -> Vec<Entry> = match tool {
        "claude" => claude_line,
        "codex" => codex_line,
        "grok" => grok_line,
        "agy" => agy_line,
        other => return Err(format!("no log reader for '{other}'")),
    };
    let (text, partial) = crate::agentpoll::tail_from(path, TAIL_BUDGET)
        .ok_or_else(|| format!("cannot read {}", path.display()))?;
    let mut entries: Vec<Entry> = text.lines().flat_map(parse).collect();
    let skip = entries.len().saturating_sub(last);
    entries.drain(..skip);
    for e in &mut entries {
        e.text = cut(&e.text, max_chars);
    }
    Ok(Log { entries, partial })
}

/// One entry as a line (or a few) for a human: `12:43:01 assistant tool_use Bash: …`.
///
/// The text is the agent's and its tools' OUTPUT — untrusted — so every control character is
/// replaced before it reaches the terminal: an escape sequence in a tool result could
/// otherwise rewrite the screen, retitle the window or set the clipboard. Newlines survive,
/// indented under the entry.
pub fn render(e: &Entry) -> String {
    // EVERY transcript-derived field, not just the text: a tool name or timestamp is just as
    // writable by whoever wrote the log.
    let time =
        e.ts.as_deref()
            .and_then(|t| t.get(11..19))
            .map_or_else(|| "--:--:--".to_string(), |t| inert(t, false));
    let name = e
        .name
        .as_deref()
        .map(|n| format!(" {}", inert(n, false)))
        .unwrap_or_default();
    let text = inert(&e.text, true);
    let text = text.trim_end().replace('\n', "\n    ");
    format!("{time} {:<9} {}{name}: {text}", e.role, e.kind)
}

/// `s` with every control character replaced — except newline and tab when `multiline`.
pub(crate) fn inert(s: &str, multiline: bool) -> String {
    s.chars()
        .map(|c| match c {
            '\n' | '\t' if multiline => c,
            c if c.is_control() => char::REPLACEMENT_CHARACTER,
            c => c,
        })
        .collect()
}

/// One Claude transcript record → its entries (an assistant turn may hold several).
fn claude_line(line: &str) -> Vec<Entry> {
    // Most records are attachments and bookkeeping; skip them before paying for JSON.
    if !line.contains("\"message\"") {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let role = match v.get("type").and_then(|t| t.as_str()) {
        Some("user") => "user",
        Some("assistant") => "assistant",
        _ => return Vec::new(),
    };
    // Injected context (skill bodies, image notes) and subagent side-chains are not the
    // conversation being followed.
    if v.get("isMeta").and_then(|m| m.as_bool()) == Some(true)
        || v.get("isSidechain").and_then(|m| m.as_bool()) == Some(true)
    {
        return Vec::new();
    }
    let ts = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .map(str::to_string);
    let entry = |role, kind, name: Option<String>, text: String| Entry {
        ts: ts.clone(),
        role,
        kind,
        name,
        text,
    };
    let Some(content) = v.pointer("/message/content") else {
        return Vec::new();
    };
    if let Some(s) = content.as_str() {
        return vec![entry(role, "text", None, s.to_string())];
    }
    let Some(parts) = content.as_array() else {
        return Vec::new();
    };
    parts
        .iter()
        .filter_map(|c| match c.get("type").and_then(|t| t.as_str())? {
            "text" => Some(entry(role, "text", None, str_at(c, "text")?)),
            "thinking" => Some(entry(role, "thinking", None, str_at(c, "thinking")?)),
            "tool_use" => {
                let name = c.get("name").and_then(|n| n.as_str())?.to_string();
                Some(entry(
                    role,
                    "tool_use",
                    Some(name),
                    tool_input(c.get("input")),
                ))
            }
            "tool_result" => Some(entry(
                "tool",
                "tool_result",
                None,
                flatten(c.get("content")?),
            )),
            _ => None,
        })
        .filter(|e| !e.text.trim().is_empty() || e.kind == "tool_use")
        .collect()
}

/// One Codex rollout record → its entry, if it is part of the conversation.
fn codex_line(line: &str) -> Vec<Entry> {
    if !line.contains("\"response_item\"") {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return Vec::new();
    }
    let ts = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .map(str::to_string);
    let Some(p) = v.get("payload") else {
        return Vec::new();
    };
    let entry = |role, kind, name: Option<String>, text: String| Entry {
        ts: ts.clone(),
        role,
        kind,
        name,
        text,
    };
    let e = match p.get("type").and_then(|t| t.as_str()) {
        Some("message") => {
            let role = match p.get("role").and_then(|r| r.as_str()) {
                Some("user") => "user",
                Some("assistant") => "assistant",
                // `developer` is the harness's own instructions.
                _ => return Vec::new(),
            };
            let text = flatten(p.get("content").unwrap_or(&serde_json::Value::Null));
            // Codex injects AGENTS.md and its environment as user messages.
            if role == "user" && is_injected(&text) {
                return Vec::new();
            }
            entry(role, "text", None, text)
        }
        Some("function_call" | "custom_tool_call" | "local_shell_call") => {
            let name = p
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("shell")
                .to_string();
            let input = p
                .get("arguments")
                .or_else(|| p.get("input"))
                .or_else(|| p.get("action"));
            entry("assistant", "tool_use", Some(name), tool_input(input))
        }
        Some("function_call_output" | "custom_tool_call_output" | "local_shell_call_output") => {
            entry(
                "tool",
                "tool_result",
                None,
                flatten(p.get("output").unwrap_or(&serde_json::Value::Null)),
            )
        }
        Some("reasoning") => entry(
            "assistant",
            "thinking",
            None,
            flatten(p.get("summary").unwrap_or(&serde_json::Value::Null)),
        ),
        _ => return Vec::new(),
    };
    if e.text.trim().is_empty() && e.kind != "tool_use" {
        return Vec::new();
    }
    vec![e]
}

/// One grok `chat_history.jsonl` record → its entries. grok records carry no timestamps.
///
/// Only a user record with `prompt_index` is something the user typed; the rest of the user
/// role is grok's own injection (`<user_info>`, skill lists, hook feedback marked
/// `synthetic_reason`).
fn grok_line(line: &str) -> Vec<Entry> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let entry = |role, kind, name: Option<String>, text: String| Entry {
        ts: None,
        role,
        kind,
        name,
        text,
    };
    let out = match v.get("type").and_then(|t| t.as_str()) {
        Some("user") if v.get("prompt_index").is_some() => vec![entry(
            "user",
            "text",
            None,
            crate::agentsessions::grok_user_text(&v),
        )],
        Some("assistant") => {
            let mut out = Vec::new();
            if let Some(text) = v.get("content").and_then(|c| c.as_str()) {
                out.push(entry("assistant", "text", None, text.to_string()));
            }
            for call in v
                .get("tool_calls")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
            {
                let Some(name) = call.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                out.push(entry(
                    "assistant",
                    "tool_use",
                    Some(name.to_string()),
                    tool_input(call.get("arguments")),
                ));
            }
            out
        }
        Some("reasoning") => vec![entry(
            "assistant",
            "thinking",
            None,
            flatten(v.get("summary").unwrap_or(&serde_json::Value::Null)),
        )],
        Some("tool_result") => vec![entry(
            "tool",
            "tool_result",
            None,
            flatten(v.get("content").unwrap_or(&serde_json::Value::Null)),
        )],
        _ => Vec::new(),
    };
    out.into_iter()
        .filter(|e| !e.text.trim().is_empty() || e.kind == "tool_use")
        .collect()
}

/// One agy `transcript.jsonl` step → its entries.
///
/// `USER_INPUT` wraps what was typed in `<USER_REQUEST>`, followed by agy's own metadata
/// blocks; `GENERIC` steps from the model are tool results; `SYSTEM_MESSAGE` is agy talking to
/// the model and is skipped.
fn agy_line(line: &str) -> Vec<Entry> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let ts = v
        .get("created_at")
        .and_then(|t| t.as_str())
        .map(str::to_string);
    let entry = |role, kind, name: Option<String>, text: String| Entry {
        ts: ts.clone(),
        role,
        kind,
        name,
        text,
    };
    let content = || {
        v.get("content")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let out = match v.get("type").and_then(|t| t.as_str()) {
        Some("USER_INPUT") => {
            let text = content();
            let typed = match (text.find("<USER_REQUEST>"), text.find("</USER_REQUEST>")) {
                (Some(a), Some(b)) if b > a => {
                    text[a + "<USER_REQUEST>".len()..b].trim().to_string()
                }
                _ => text,
            };
            vec![entry("user", "text", None, typed)]
        }
        Some("PLANNER_RESPONSE") => {
            let mut out = Vec::new();
            if let Some(t) = v.get("thinking").and_then(|t| t.as_str()) {
                out.push(entry("assistant", "thinking", None, t.to_string()));
            }
            out.push(entry("assistant", "text", None, content()));
            for call in v
                .get("tool_calls")
                .and_then(|c| c.as_array())
                .into_iter()
                .flatten()
            {
                let Some(name) = call.get("name").and_then(|n| n.as_str()) else {
                    continue;
                };
                let args = call.get("args");
                let text = ["toolSummary", "CommandLine", "TargetFile", "AbsolutePath"]
                    .iter()
                    .find_map(|k| args?.get(*k).and_then(crate::agentpoll::agy_arg))
                    .unwrap_or_else(|| args.map(|a| a.to_string()).unwrap_or_default());
                out.push(entry("assistant", "tool_use", Some(name.to_string()), text));
            }
            out
        }
        Some("GENERIC") if v.get("source").and_then(|s| s.as_str()) == Some("MODEL") => {
            vec![entry("tool", "tool_result", None, content())]
        }
        _ => Vec::new(),
    };
    out.into_iter()
        .filter(|e| !e.text.trim().is_empty() || e.kind == "tool_use")
        .collect()
}

/// The context Codex prepends to a thread as user messages.
fn is_injected(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("# AGENTS.md instructions")
        || t.starts_with("<environment_context>")
        || t.starts_with("<user_instructions>")
}

/// `c[key]` as a string.
fn str_at(c: &serde_json::Value, key: &str) -> Option<String> {
    c.get(key).and_then(|t| t.as_str()).map(str::to_string)
}

/// A tool's input as one readable line: the field a human would want first (the command,
/// the file, the description), else the whole input as compact JSON.
fn tool_input(input: Option<&serde_json::Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    // Codex sends `arguments` as a JSON string.
    let parsed;
    let input = match input.as_str() {
        Some(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(v) if v.is_object() => {
                parsed = v;
                &parsed
            }
            _ => return s.to_string(),
        },
        None => input,
    };
    for k in [
        "description",
        "command",
        "cmd",
        "file_path",
        "pattern",
        "prompt",
    ] {
        match input.get(k) {
            Some(serde_json::Value::String(s)) => return s.clone(),
            Some(serde_json::Value::Array(a)) => {
                return a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            _ => {}
        }
    }
    input.to_string()
}

/// Text out of a content value: a string, or an array of `{text}`-bearing parts.
fn flatten(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|p| match p {
                serde_json::Value::String(s) => Some(s.clone()),
                _ => p.get("text").and_then(|t| t.as_str()).map(str::to_string),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// At most `max` chars (0 = no limit), with an ellipsis when cut.
fn cut(s: &str, max: usize) -> String {
    if max == 0 || s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", head.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grok_turns_tools_and_results_are_read_and_injections_are_not() {
        // Verbatim-shaped grok 1.0.70 chat_history.jsonl records (long fields trimmed).
        let lines = [
            r#"{"type":"system","content":"You are Grok 4.7"}"#,
            r#"{"type":"user","content":[{"type":"text","text":"<user_info>\nOS Version: macos\n</user_info>"}]}"#,
            r#"{"type":"user","content":[{"type":"text","text":"<system-reminder>skills</system-reminder>"}],"synthetic_reason":"skills"}"#,
            r#"{"type":"user","content":[{"type":"text","text":"<user_query>\nRun the shell command: touch grok-probe.txt   then say done\n</user_query>"}],"prompt_index":0}"#,
            r#"{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"Simple request."}],"encrypted_content":"x","status":"completed"}"#,
            r#"{"type":"assistant","content":"I'll create it.","tool_calls":[{"id":"c0","name":"run_terminal_command","arguments":"{\"command\":\"touch grok-probe.txt\",\"description\":\"Create empty grok-probe.txt file\"}"}]}"#,
            r#"{"type":"tool_result","tool_call_id":"c0","content":"exit: 0\n"}"#,
            r#"{"type":"assistant","content":"Done.","model_id":"grok-4.7"}"#,
        ];
        let got: Vec<Entry> = lines.iter().flat_map(|l| grok_line(l)).collect();
        let shape: Vec<(&str, &str, Option<&str>, &str)> = got
            .iter()
            .map(|e| (e.role, e.kind, e.name.as_deref(), e.text.trim()))
            .collect();
        assert_eq!(
            shape,
            [
                (
                    "user",
                    "text",
                    None,
                    "Run the shell command: touch grok-probe.txt   then say done"
                ),
                ("assistant", "thinking", None, "Simple request."),
                ("assistant", "text", None, "I'll create it."),
                (
                    "assistant",
                    "tool_use",
                    Some("run_terminal_command"),
                    "Create empty grok-probe.txt file"
                ),
                ("tool", "tool_result", None, "exit: 0"),
                ("assistant", "text", None, "Done."),
            ]
        );
    }

    #[test]
    fn agy_steps_are_read_and_system_messages_are_not() {
        // Verbatim agy 1.4.7 transcript.jsonl steps (long fields trimmed).
        let lines = [
            r#"{"step_index": 0, "source": "USER_EXPLICIT", "type": "USER_INPUT", "status": "DONE", "created_at": "2026-10-24T13:01:30Z", "content": "<USER_REQUEST>\nRun the shell command: touch approval-probe.txt   then say done\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-10-24T22:01:30+09:00.\n</ADDITIONAL_METADATA>"}"#,
            r#"{"step_index": 1, "source": "MODEL", "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-10-24T13:01:30Z", "tool_calls": [{"name": "run_command", "args": {"CommandLine": "\"touch approval-probe.txt\"", "toolSummary": "\"Touch approval-probe.txt\""}}]}"#,
            r#"{"step_index": 2, "source": "MODEL", "type": "GENERIC", "status": "DONE", "created_at": "2026-10-24T13:01:33Z", "content": "Command exited with code 0."}"#,
            r#"{"step_index": 5, "source": "SYSTEM", "type": "SYSTEM_MESSAGE", "status": "DONE", "created_at": "2026-10-24T13:02:43Z", "content": "The following is a <SYSTEM_MESSAGE>"}"#,
            r#"{"step_index": 3, "source": "MODEL", "type": "PLANNER_RESPONSE", "status": "DONE", "created_at": "2026-10-24T13:02:08Z", "content": "done"}"#,
        ];
        let got: Vec<Entry> = lines.iter().flat_map(|l| agy_line(l)).collect();
        let shape: Vec<(&str, &str, Option<&str>, &str)> = got
            .iter()
            .map(|e| (e.role, e.kind, e.name.as_deref(), e.text.as_str()))
            .collect();
        assert_eq!(
            shape,
            [
                (
                    "user",
                    "text",
                    None,
                    "Run the shell command: touch approval-probe.txt   then say done"
                ),
                (
                    "assistant",
                    "tool_use",
                    Some("run_command"),
                    "Touch approval-probe.txt"
                ),
                ("tool", "tool_result", None, "Command exited with code 0."),
                ("assistant", "text", None, "done"),
            ]
        );
        assert_eq!(got[0].ts.as_deref(), Some("2026-10-24T13:01:30Z"));
    }

    #[test]
    fn claude_turns_tools_and_results_are_read_and_noise_is_not() {
        let lines = [
            r#"{"type":"attachment","attachment":{"type":"hook_success"}}"#,
            r#"{"type":"user","timestamp":"T1","message":{"role":"user","content":"fix the bug"}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"skill body"}]}}"#,
            r#"{"type":"assistant","timestamp":"T2","message":{"role":"assistant","content":[{"type":"thinking","thinking":""},{"type":"text","text":"Looking."},{"type":"tool_use","name":"Bash","input":{"command":"cargo test","description":"run the tests"}}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":[{"type":"text","text":"ok: 3 passed"}]}]}}"#,
            r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"subagent"}]}}"#,
        ];
        let got: Vec<Entry> = lines.iter().flat_map(|l| claude_line(l)).collect();
        let shape: Vec<_> = got
            .iter()
            .map(|e| (e.role, e.kind, e.name.as_deref(), e.text.as_str()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("user", "text", None, "fix the bug"),
                ("assistant", "text", None, "Looking."),
                ("assistant", "tool_use", Some("Bash"), "run the tests"),
                ("tool", "tool_result", None, "ok: 3 passed"),
            ]
        );
        assert_eq!(got[0].ts.as_deref(), Some("T1"));
    }

    #[test]
    fn codex_messages_calls_and_outputs_are_read_and_injected_context_is_not() {
        let lines = [
            r#"{"type":"session_meta","payload":{}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"rules"}]}}"#,
            r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\n..."}]}}"##,
            r#"{"type":"response_item","timestamp":"T","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"review this"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":[\"git\",\"status\"]}"}}"#,
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","output":[{"type":"input_text","text":"clean"}]}}"#,
            r#"{"type":"response_item","payload":{"type":"reasoning","summary":[],"encrypted_content":"x"}}"#,
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Looks fine."}]}}"#,
        ];
        let got: Vec<_> = lines
            .iter()
            .flat_map(|l| codex_line(l))
            .map(|e| (e.role, e.kind, e.name, e.text))
            .collect();
        assert_eq!(
            got,
            vec![
                ("user", "text", None, "review this".to_string()),
                (
                    "assistant",
                    "tool_use",
                    Some("exec_command".to_string()),
                    "git status".to_string()
                ),
                ("tool", "tool_result", None, "clean".to_string()),
                ("assistant", "text", None, "Looks fine.".to_string()),
            ]
        );
    }

    #[test]
    fn render_neutralizes_control_characters_and_indents_lines() {
        let e = Entry {
            ts: Some("2026-10-15T12:43:01.409Z".into()),
            role: "tool",
            kind: "tool_result",
            name: None,
            text: "ok\x1b]52;c;cHduZWQ=\x07\nline two\u{9b}2J\n".into(),
        };
        let out = render(&e);
        assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "{out:?}");
        let named = render(&Entry {
            ts: Some("2026-10-15T1\x1b[2J:00.0Z".into()),
            role: "assistant",
            kind: "tool_use",
            name: Some("Bash\x1b]52;c;eA==\x07\nx".into()),
            text: String::new(),
        });
        assert!(!named.chars().any(|c| c.is_control()), "{named:?}");
        assert_eq!(
            out,
            "12:43:01 tool      tool_result: ok\u{fffd}]52;c;cHduZWQ=\u{fffd}\n    line two\u{fffd}2J"
        );
    }

    #[test]
    fn read_keeps_the_last_entries_and_cuts_their_text() {
        let dir = std::env::temp_dir().join(format!("comux-agentlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let mut body = String::new();
        for i in 0..5 {
            body.push_str(&format!(
                r#"{{"type":"user","message":{{"role":"user","content":"message number {i}"}}}}"#
            ));
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        let got = read(&path, "claude", 2, 10).unwrap();
        assert_eq!(
            got.entries
                .iter()
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>(),
            vec!["message n…", "message n…"]
        );
        assert!(!got.partial, "the whole file fit");
        let all = read(&path, "claude", 100, 0).unwrap().entries;
        assert_eq!(all.len(), 5);
        assert_eq!(all[4].text, "message number 4");
        assert!(read(&path, "aider", 1, 0).is_err());
        assert!(read(&dir.join("missing"), "claude", 1, 0).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
