//! `GET /v1/secrets/available` — operator surface that lists
//! the names of loaded JIT secrets.
//!
//! Values NEVER leave this endpoint. Operators verify their
//! `RELIX_*` env vars were picked up; tool-call dispatchers
//! rewrite `{{secret:<name>}}` placeholders at the moment of
//! call, never returning the value through any HTTP surface.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use crate::config::AppState;

#[derive(Debug, Serialize)]
pub struct SecretsAvailableResponse {
    pub keys: Vec<String>,
    pub count: usize,
}

pub(crate) fn available_logic(
    store: &relix_runtime::nodes::execution::secrets::SecretStore,
) -> SecretsAvailableResponse {
    let keys = store.available_keys();
    SecretsAvailableResponse {
        count: keys.len(),
        keys,
    }
}

pub async fn available(State(state): State<AppState>) -> Json<SecretsAvailableResponse> {
    Json(available_logic(state.jit_secrets.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_runtime::nodes::execution::secrets::SecretStore;
    use std::collections::BTreeMap;

    fn store_with(pairs: &[(&str, &str)]) -> SecretStore {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), (*v).to_string());
        }
        SecretStore::from_map(m)
    }

    #[test]
    fn available_logic_returns_names_only() {
        let store = store_with(&[
            ("github_token", "ghp_secretvalue"),
            ("openai_api_key", "sk-secretvalue"),
        ]);
        let resp = available_logic(&store);
        assert_eq!(resp.count, 2);
        assert!(resp.keys.iter().any(|k| k == "github_token"));
        assert!(resp.keys.iter().any(|k| k == "openai_api_key"));
        // Values must never appear in the response.
        for k in &resp.keys {
            assert!(!k.contains("ghp_secret"));
            assert!(!k.contains("sk-secret"));
        }
    }

    #[test]
    fn available_logic_handles_empty_store() {
        let store = store_with(&[]);
        let resp = available_logic(&store);
        assert_eq!(resp.count, 0);
        assert!(resp.keys.is_empty());
    }

    #[test]
    fn available_response_serialises_as_documented_shape() {
        let store = store_with(&[("github_token", "x")]);
        let resp = available_logic(&store);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"keys\":[\"github_token\"]"));
        assert!(json.contains("\"count\":1"));
        // Defense-in-depth: the value never reaches JSON.
        assert!(!json.contains("\"x\""));
    }
}
