//! `graph`: the work graph — what each live agent has read, edited, searched and run, its
//! subagents, and the order it did it in (decisions #204).
//!
//! The topic is a bounded OVERVIEW: per agent its counters, the newest files and steps, its
//! subagents. The full history of one agent comes page by page from the `graph` command
//! ([`GraphPage`]), which a host asks for when it shows that agent.
//!
//! **Identity.** File and subagent ids are minted per conversation graph in discovery order and
//! never reused; step `seq`s rise from 1. All three are scoped by `(conversation, gen)`: `gen`
//! changes whenever comux rebuilds a graph from scratch (a replaced transcript), so a host keys
//! its cache by it and drops everything when it changes. Every slice — topic or page — carries
//! the file and subagent records its steps reference, so a reference always resolves.

use serde::{Deserialize, Serialize};

/// The `graph` topic.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Graph {
    pub agents: Vec<AgentGraph>,
    /// Agents left out to keep the topic under its byte budget (least recently active first).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_agents: u32,
}

/// One live agent's graph, as the overview carries it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct AgentGraph {
    /// The pane (`$COPAD_MUX_PANE`), the join key with the `agents` topic.
    pub token: String,
    pub tool: String,
    /// The agent's conversation (Claude session id, Codex thread id).
    pub conversation: String,
    /// The graph's generation (see the module docs).
    #[serde(rename = "gen")]
    pub generation: String,
    /// The directory the agent works in; `display` paths are relative to it.
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub space_id: String,
    #[serde(default)]
    pub tab_id: String,
    /// UNIX milliseconds of the first and the newest step.
    #[serde(default)]
    pub started_ms: i64,
    #[serde(default)]
    pub updated_ms: i64,
    pub counts: GraphCounts,
    pub files: Vec<GraphFile>,
    pub subagents: Vec<GraphSub>,
    /// The newest steps, oldest first.
    pub steps: Vec<GraphStep>,
    /// Steps still waiting for their result (newest, at most 32).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<u64>,
    /// The newest settlements (at most 64), for steps a host loaded while they were pending.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settled: Vec<Settled>,
    /// The oldest step comux still keeps (older ones were dropped past its history cap).
    #[serde(default)]
    pub oldest_seq: u64,
    /// Steps dropped past the history cap.
    #[serde(default, skip_serializing_if = "is_zero64")]
    pub dropped_before: u64,
    /// Activity on files (subagents) that arrived after the graph was full: kept as steps
    /// without a file (attributed to the main agent), counted here.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_files: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub omitted_subagents: u32,
}

/// Totals over the whole conversation (not just what the overview carries).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphCounts {
    pub files: u32,
    pub reads: u32,
    pub edits: u32,
    pub writes: u32,
    pub searches: u32,
    pub runs: u32,
    pub prompts: u32,
    pub subagents: u32,
    pub failed: u32,
    /// The newest `seq`: how many steps there have been.
    pub steps: u64,
}

/// A file (or directory a search ran in) the agent touched.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphFile {
    pub id: u32,
    /// Absolute, lexically normalised, on the comux host. The identity across agents.
    pub abs: String,
    /// Relative to the agent's cwd when under it, else `~/…` or absolute.
    pub display: String,
    #[serde(default)]
    pub reads: u32,
    #[serde(default)]
    pub edits: u32,
    #[serde(default)]
    pub writes: u32,
    #[serde(default)]
    pub searches: u32,
    #[serde(default)]
    pub first_ms: i64,
    #[serde(default)]
    pub last_ms: i64,
}

/// A subagent the agent spawned (Claude `Agent`/`Task`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphSub {
    /// Never 0: 0 is the main agent.
    pub id: u32,
    /// `Explore`, `general-purpose`, … (the subagent type).
    pub kind: String,
    pub description: String,
    /// The agent that spawned it: 0 = the main agent, else another subagent's id.
    #[serde(default)]
    pub parent: u32,
    #[serde(default)]
    pub started_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<i64>,
    /// `running`, `completed`, `failed`, `killed`.
    pub status: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum StepKind {
    /// The user asked something (a turn begins).
    #[default]
    Prompt,
    Read,
    Edit,
    /// Created or overwrote a whole file.
    Write,
    Delete,
    /// A grep/glob/listing; `file` is where it looked, when it said.
    Search,
    /// A shell command.
    Run,
    /// A web fetch or search.
    Web,
    /// Started a subagent (`file` empty; the subagent is the step's own).
    Spawn,
    /// The turn ended (Codex `task_complete`).
    Done,
}

/// How a step ended.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// Its result has not arrived.
    Pending,
    #[default]
    Ok,
    Failed,
    /// Its result was never matched (comux stopped waiting for it).
    Unknown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GraphStep {
    pub seq: u64,
    pub at_ms: i64,
    pub kind: StepKind,
    /// Who did it: 0 = the main agent, else a subagent's id.
    #[serde(default)]
    pub agent: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<u32>,
    /// What it was: a prompt's first line, a command, a search pattern, a subagent's task.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(default)]
    pub outcome: Outcome,
    /// For a spawn: the subagent it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawned: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settled {
    pub seq: u64,
    pub outcome: Outcome,
}

/// One page of an agent's history: the `graph` command's answer for a target.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct GraphPage {
    pub token: String,
    pub tool: String,
    pub conversation: String,
    #[serde(rename = "gen")]
    pub generation: String,
    #[serde(default)]
    pub cwd: String,
    pub counts: GraphCounts,
    pub files: Vec<GraphFile>,
    pub subagents: Vec<GraphSub>,
    /// Oldest first.
    pub steps: Vec<GraphStep>,
    /// The oldest step comux keeps for this conversation.
    pub oldest_seq: u64,
    /// The first and last step of THIS page (0 when it is empty).
    pub page_first_seq: u64,
    pub page_last_seq: u64,
    /// Ask again with `before = next_before` for the steps before this page; absent when the
    /// page reaches `oldest_seq`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before: Option<u64>,
    /// Steps dropped past the history cap (older than `oldest_seq`).
    #[serde(default, skip_serializing_if = "is_zero64")]
    pub dropped_before: u64,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn is_zero64(n: &u64) -> bool {
    *n == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_outcomes_are_kebab_case() {
        assert_eq!(
            serde_json::to_string(&StepKind::Search).unwrap(),
            "\"search\""
        );
        assert_eq!(
            serde_json::to_string(&Outcome::Pending).unwrap(),
            "\"pending\""
        );
    }

    #[test]
    fn an_older_graph_value_still_decodes() {
        let v = r#"{"agents":[{"token":"t","tool":"claude","conversation":"c","gen":"g",
            "counts":{"files":0,"reads":0,"edits":0,"writes":0,"searches":0,"runs":0,
            "prompts":0,"subagents":0,"failed":0,"steps":0},
            "files":[],"subagents":[],"steps":[{"seq":1,"at_ms":5,"kind":"read"}]}]}"#;
        let g: Graph = serde_json::from_str(v).unwrap();
        assert_eq!(g.agents[0].steps[0].outcome, Outcome::Ok);
        assert_eq!(g.agents[0].steps[0].agent, 0);
    }
}
