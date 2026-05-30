//! `/v1/tools` and `/v1/tools/search` — operator surface
//! over the tool registry.
//!
//! Both endpoints are read-only. The registry is built once
//! at bridge startup from the discovered capability set; the
//! bridge does NOT mutate it at runtime — operators add or
//! remove tools by editing the tool-node config and
//! restarting.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use relix_runtime::nodes::tool::manifest::{SignedManifest, ToolManifest};
use relix_runtime::nodes::tool::registry::{ToolDefinition, ToolRegistry};
use serde::{Deserialize, Serialize};

use crate::config::AppState;

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

#[derive(Debug, Serialize)]
pub struct ToolsListResponse {
    pub tools: Vec<ToolDefinition>,
    pub count: usize,
}

#[derive(Debug, Deserialize)]
pub struct ToolSearchRequest {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    10
}

pub(crate) fn list_logic(registry: &ToolRegistry) -> ToolsListResponse {
    let tools = registry.all().to_vec();
    ToolsListResponse {
        count: tools.len(),
        tools,
    }
}

pub(crate) fn search_logic(
    registry: &ToolRegistry,
    req: &ToolSearchRequest,
) -> Result<ToolsListResponse, (StatusCode, Json<ApiError>)> {
    if req.query.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "query must be non-empty".into(),
            }),
        ));
    }
    let tools: Vec<ToolDefinition> = registry
        .keyword_search(&req.query, req.limit)
        .into_iter()
        .cloned()
        .collect();
    Ok(ToolsListResponse {
        count: tools.len(),
        tools,
    })
}

pub async fn list(State(state): State<AppState>) -> Json<ToolsListResponse> {
    Json(list_logic(state.tool_registry.as_ref()))
}

pub async fn search(
    State(state): State<AppState>,
    Json(req): Json<ToolSearchRequest>,
) -> Result<Json<ToolsListResponse>, (StatusCode, Json<ApiError>)> {
    search_logic(state.tool_registry.as_ref(), &req).map(Json)
}

/// Signed-manifest response. When the bridge has access to
/// the controller's signing key (the 32-byte `client_key`),
/// the manifest carries a real blake3 MAC and `warning` is
/// absent. When no key is available the manifest signs with
/// a zero key and we attach `warning: "unsigned"` so callers
/// don't mistake the placeholder signature for a real one.
#[derive(Debug, Serialize)]
pub struct ManifestResponse {
    pub signed: SignedManifest,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

pub(crate) fn manifest_logic(
    registry: &ToolRegistry,
    signing_key: Option<&[u8]>,
    signer: &str,
) -> ManifestResponse {
    let manifest = ToolManifest {
        version: 1,
        tools: registry.all().to_vec(),
        signed_at: unix_secs(),
        signer: signer.to_string(),
    };
    let (signed, warning) = match signing_key {
        Some(key) if !key.is_empty() => (SignedManifest::sign(manifest, key), None),
        _ => (
            SignedManifest::sign(manifest, &[0u8; 32]),
            Some("unsigned".to_string()),
        ),
    };
    ManifestResponse { signed, warning }
}

pub async fn manifest(State(state): State<AppState>) -> Json<ManifestResponse> {
    let key = state.client_key.clone();
    let signer = format!("{}:{}", state.bridge_host, state.bridge_port);
    Json(manifest_logic(
        state.tool_registry.as_ref(),
        Some(&*key),
        &signer,
    ))
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Helper used by `AppState::try_new` to build the default
/// registry. Keeps the in-test registry construction in one
/// place; production registry contents will land once the
/// tool-node side publishes its capability descriptors to
/// the bridge.
pub fn empty_registry() -> Arc<ToolRegistry> {
    Arc::new(ToolRegistry::new(Vec::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn tool(name: &str, description: &str, tags: &[&str]) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            input_schema: Value::Object(Default::default()),
            output_schema: Value::Object(Default::default()),
            reversible: true,
            rollback_hint: None,
            tags: tags.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn sample_registry() -> ToolRegistry {
        ToolRegistry::new(vec![
            tool(
                "tool.web_fetch",
                "Fetch the contents of a URL via HTTPS",
                &["network"],
            ),
            tool(
                "tool.fs.read_file",
                "Read text content from a file under the jailed root",
                &["filesystem"],
            ),
        ])
    }

    #[test]
    fn list_logic_returns_every_tool() {
        let registry = sample_registry();
        let resp = list_logic(&registry);
        assert_eq!(resp.count, 2);
        let names: Vec<&str> = resp.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"tool.web_fetch"));
        assert!(names.contains(&"tool.fs.read_file"));
    }

    #[test]
    fn search_logic_returns_keyword_hits() {
        let registry = sample_registry();
        let req = ToolSearchRequest {
            query: "fetch webpage".into(),
            limit: 5,
        };
        let resp = search_logic(&registry, &req).unwrap();
        assert!(resp.count >= 1);
        assert_eq!(resp.tools[0].name, "tool.web_fetch");
    }

    #[test]
    fn search_logic_rejects_empty_query() {
        let registry = sample_registry();
        let req = ToolSearchRequest {
            query: "   ".into(),
            limit: 5,
        };
        let err = search_logic(&registry, &req).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn list_response_serialises_to_documented_json_shape() {
        let registry = sample_registry();
        let resp = list_logic(&registry);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"count\":2"));
        assert!(json.contains("\"tool.web_fetch\""));
        assert!(json.contains("\"reversible\":true"));
    }

    #[test]
    fn manifest_logic_signs_with_supplied_key() {
        let registry = sample_registry();
        let key = b"controller-secret-key-32-bytes!!";
        let resp = manifest_logic(&registry, Some(key), "ctrl-1");
        assert_eq!(resp.signed.signature.len(), 64);
        assert!(resp.warning.is_none());
        assert_eq!(resp.signed.verify(key), Ok(()));
        // Tools came through.
        assert_eq!(resp.signed.manifest.tools.len(), 2);
        assert_eq!(resp.signed.manifest.signer, "ctrl-1");
    }

    #[test]
    fn manifest_logic_attaches_warning_when_no_key_available() {
        let registry = sample_registry();
        let resp = manifest_logic(&registry, None, "ctrl-1");
        assert_eq!(
            resp.warning.as_deref(),
            Some("unsigned"),
            "missing key should set the unsigned warning"
        );
        // Empty-slice key also triggers the warning so a
        // misconfigured controller doesn't ship a "signed"
        // manifest with an empty key.
        let resp = manifest_logic(&registry, Some(&[]), "ctrl-1");
        assert_eq!(resp.warning.as_deref(), Some("unsigned"));
    }
}
