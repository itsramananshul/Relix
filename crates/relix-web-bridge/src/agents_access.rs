//! `GET /v1/agents/access` — operator surface that lists the
//! configured agent access policies and recent call counts.
//!
//! Read-only. Mutating the policies requires editing the
//! bridge TOML and restarting; we deliberately do not expose
//! a runtime mutation endpoint here because the policy is
//! security-critical and live edits create audit gaps.

use axum::Json;
use axum::extract::State;
use relix_runtime::nodes::execution::broker::{AgentAccessBroker, AgentAccessSnapshotEntry};
use serde::Serialize;

use crate::config::AppState;

#[derive(Debug, Serialize)]
pub struct AgentsAccessResponse {
    pub agents: Vec<AgentAccessSnapshotEntry>,
    pub count: usize,
}

pub(crate) fn agents_logic(broker: &AgentAccessBroker) -> AgentsAccessResponse {
    let agents = broker.snapshot();
    AgentsAccessResponse {
        count: agents.len(),
        agents,
    }
}

pub async fn agents(State(state): State<AppState>) -> Json<AgentsAccessResponse> {
    Json(agents_logic(state.access_broker.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_runtime::nodes::execution::broker::AccessPolicy;

    fn policy(agent: &str) -> AccessPolicy {
        AccessPolicy {
            agent: agent.to_string(),
            allowed_capabilities: vec!["ai.chat".into()],
            denied_capabilities: vec!["tool.terminal".into()],
            max_calls_per_minute: 30,
            max_cost_cents_per_hour: 500,
        }
    }

    #[test]
    fn agents_logic_returns_snapshot_with_count() {
        let broker = AgentAccessBroker::new(vec![policy("alice"), policy("bob")]);
        broker.record_call("alice");
        let resp = agents_logic(&broker);
        assert_eq!(resp.count, 2);
        // Sorted by agent name.
        assert_eq!(resp.agents[0].policy.agent, "alice");
        assert_eq!(resp.agents[1].policy.agent, "bob");
        assert_eq!(resp.agents[0].recent_calls_60s, 1);
        assert_eq!(resp.agents[1].recent_calls_60s, 0);
    }

    #[test]
    fn agents_logic_handles_empty_broker() {
        let broker = AgentAccessBroker::empty();
        let resp = agents_logic(&broker);
        assert_eq!(resp.count, 0);
        assert!(resp.agents.is_empty());
    }

    #[test]
    fn agents_response_serialises_to_documented_json_shape() {
        let broker = AgentAccessBroker::new(vec![policy("alice")]);
        let resp = agents_logic(&broker);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"count\":1"));
        assert!(json.contains("\"agent\":\"alice\""));
        assert!(json.contains("\"recent_calls_60s\":0"));
        assert!(json.contains("\"allowed_capabilities\":[\"ai.chat\"]"));
    }
}
