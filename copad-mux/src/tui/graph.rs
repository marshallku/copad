use super::*;
use crate::control::Resp;
use crate::graphpoll;
use crate::workgraph::ConvGraph;
use copad_mux_proto::graph as g;

/// The `graph` topic's byte budget (well under the host API's 1 MiB topic cap).
const TOPIC_BUDGET: usize = 256 << 10;
/// Per-agent overview sizes tried in turn until the topic fits: (files, steps, subagents).
const LEVELS: [(usize, usize, usize); 4] = [(32, 24, 8), (12, 8, 4), (4, 2, 2), (0, 0, 0)];
pub const PAGE_LIMIT: usize = 1000;
const PAGE_BUDGET: usize = 512 << 10;

type CacheKey = (u64, Vec<(String, u32, String, String)>);

#[derive(Default)]
pub struct GraphCache {
    key: Option<CacheKey>,
    json: String,
}

impl App {
    pub fn start_graph_poll(&mut self) {
        self.graph_poll = graphpoll::spawn();
    }

    pub fn graph_dir() -> PathBuf {
        crate::persist::state_path().with_file_name("graphs")
    }

    pub(super) fn set_graph_wants(&self, agents: &[(u32, String, Option<PathBuf>)]) {
        let wants = if self.cfg.graph {
            agents
                .iter()
                .filter(|(_, tool, _)| graphpoll::tracks(tool))
                .filter_map(|(pid, tool, cwd)| {
                    let (conversation, transcript) =
                        agentpoll::conversation(&self.agent_poll, *pid)?;
                    Some(graphpoll::Want {
                        pid: *pid,
                        tool: tool.clone(),
                        conversation,
                        transcript,
                        cwd: cwd
                            .as_ref()
                            .map(|c| c.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        graphpoll::set_wanted(&self.graph_poll, wants, self.graph_settings());
    }

    fn graph_settings(&self) -> graphpoll::Settings {
        graphpoll::Settings {
            enabled: self.cfg.graph,
            persist: self.cfg.graph_persist,
            retain_days: self.cfg.graph_retain_days,
            dir: Self::graph_dir(),
        }
    }

    /// A reloaded config reaches the poller at once: `graph_persist = false` must stop the
    /// very next save, not one that lands before the next sweep.
    pub(super) fn push_graph_settings(&self) {
        graphpoll::set_settings(&self.graph_poll, self.graph_settings());
    }

    fn graphs_by_pane(&self) -> Vec<(String, String, String, std::sync::Arc<ConvGraph>)> {
        let (_, graphs) = graphpoll::snapshot(&self.graph_poll);
        let mut out: Vec<_> = self
            .agent_rows()
            .into_iter()
            .filter_map(|r| {
                let pid = self.labels.get(&r.term)?.pid;
                let graph = graphs.get(&pid)?.clone();
                let token = self.token_of(&r.term)?;
                let tab = self
                    .tab_location(&r.term)
                    .map(|l| l.0.to_string())
                    .unwrap_or_default();
                Some((token, r.space_id.to_string(), tab, graph))
            })
            .collect();
        out.sort_by_key(|(.., g)| std::cmp::Reverse(g.overview(0, 0, 0).updated_ms));
        out
    }

    pub fn host_graph_json(&self) -> String {
        let rev = graphpoll::snapshot(&self.graph_poll).0;
        let rows: Vec<(String, u32, String, String)> = self
            .agent_rows()
            .into_iter()
            .filter_map(|r| {
                let pid = self.labels.get(&r.term)?.pid;
                let tab = self
                    .tab_location(&r.term)
                    .map(|l| l.0.to_string())
                    .unwrap_or_default();
                Some((self.token_of(&r.term)?, pid, r.space_id.to_string(), tab))
            })
            .collect();
        let key: CacheKey = (rev, rows);
        let mut cache = self.graph_cache.borrow_mut();
        if cache.key.as_ref() != Some(&key) {
            cache.json = crate::hostapi::ser(&self.host_graph());
            cache.key = Some(key);
        }
        cache.json.clone()
    }

    pub fn host_graph(&self) -> g::Graph {
        fit_overview(&self.graphs_by_pane(), TOPIC_BUDGET)
    }

    /// The `graph` command: no target = the overview; a target = a page of that agent's
    /// history (`before` = continue from a page's `next_before`).
    pub(super) fn graph_request(
        &self,
        target: Option<&str>,
        before: Option<u64>,
        limit: Option<usize>,
    ) -> Resp {
        let Some(target) = target else {
            return Resp::graph(serde_json::to_value(self.host_graph()).unwrap_or_default());
        };
        let Some(term) = self.resolve_pane_target(target) else {
            return Resp::err_code("not_found", format!("unknown pane '{target}'"));
        };
        let token = self.token_of(&term).unwrap_or_default();
        let Some((.., graph)) = self
            .graphs_by_pane()
            .into_iter()
            .find(|(t, ..)| *t == token)
        else {
            return Resp::err_code(
                "no-graph",
                "no work graph for that pane (not a Claude or Codex agent comux can read, or graph = false)",
            );
        };
        let mut page = graph.page(
            before,
            limit.unwrap_or(PAGE_LIMIT).min(PAGE_LIMIT),
            PAGE_BUDGET,
        );
        page.token = token;
        Resp::graph(serde_json::to_value(page).unwrap_or_default())
    }
}

/// Within `budget` bytes: every agent at the richest level that fits, then less, then agents
/// dropped from the least active end.
pub(super) fn fit_overview(
    graphs: &[(String, String, String, std::sync::Arc<ConvGraph>)],
    budget: usize,
) -> g::Graph {
    let build = |level: (usize, usize, usize), n: usize| {
        let agents = graphs
            .iter()
            .take(n)
            .map(|(token, space, tab, graph)| {
                let mut o = graph.overview(level.0, level.1, level.2);
                o.token = token.clone();
                o.space_id = space.clone();
                o.tab_id = tab.clone();
                o
            })
            .collect();
        g::Graph {
            agents,
            omitted_agents: (graphs.len() - n) as u32,
        }
    };
    let size = |v: &g::Graph| serde_json::to_vec(v).map_or(usize::MAX, |b| b.len());
    for level in LEVELS {
        let v = build(level, graphs.len());
        if size(&v) <= budget {
            return v;
        }
    }
    let mut n = graphs.len();
    loop {
        n = n.saturating_sub((n / 8).max(1));
        let v = build(LEVELS[3], n);
        if n == 0 || size(&v) <= budget {
            return v;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn graph_with(n: usize) -> Arc<ConvGraph> {
        let mut c = ConvGraph::new("claude", "c", "/r", PathBuf::from("/nonexistent"));
        for i in 0..n {
            let line = format!(
                r#"{{"type":"assistant","timestamp":"2026-11-04T00:00:00Z","message":{{"content":[{{"type":"tool_use","id":"t{i}","name":"Read","input":{{"file_path":"/r/{}/f{i}.rs"}}}}]}}}}"#,
                "d".repeat(200)
            );
            c.ingest(&line, 0);
        }
        Arc::new(c)
    }

    #[test]
    fn the_overview_sheds_detail_then_agents_to_fit() {
        let graphs: Vec<_> = (0..40)
            .map(|i| {
                (
                    format!("tok{i}"),
                    "s1".to_string(),
                    "s1:t1".to_string(),
                    graph_with(60),
                )
            })
            .collect();
        let rich = fit_overview(&graphs[..1], TOPIC_BUDGET);
        assert_eq!(rich.agents[0].files.len(), 32);
        assert_eq!(rich.agents[0].token, "tok0");
        let fitted = fit_overview(&graphs, 64 << 10);
        assert!(serde_json::to_vec(&fitted).unwrap().len() <= 64 << 10);
        assert_eq!(fitted.agents.len() + fitted.omitted_agents as usize, 40);
        let tiny = fit_overview(&graphs, 2 << 10);
        assert!(tiny.omitted_agents > 0);
        assert!(serde_json::to_vec(&tiny).unwrap().len() <= 2 << 10);
    }
}
