//! `GET /v1/capabilities` — JSON projection of the bridge's
//! `ManifestCache`.
//!
//! Translation-only by design: this is a thin projector over the
//! already-discovered manifests the bridge keeps for capability
//! routing (M10/A.4). No discovery is triggered by these endpoints;
//! they read whatever the latest refresh produced.
//!
//! Endpoints:
//!
//! - `GET /v1/capabilities` — every capability known to the bridge,
//!   one JSON entry per `(peer, capability)` pair. Optional
//!   `?category=` and `?tag=` filters.
//! - `GET /v1/capabilities/:method` — the same shape, scoped to
//!   capabilities whose `method_name` matches exactly.
//!
//! Auth: none at the HTTP layer (consistent with the rest of the
//! bridge). Put a reverse proxy in front before exposing beyond
//! loopback.
//!
//! Architectural note: this surface is **purely read-only** and
//! exposes mesh state that is already visible to any peer via
//! `node.manifest`. No invariant is changed by serving it.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};

use crate::config::AppState;

/// One row of `/v1/capabilities`. Includes the peer that serves the
/// capability so operators can answer "where is this?" without a
/// second lookup.
#[derive(Debug, Serialize)]
pub struct CapabilityEntry {
    /// Peer alias the operator configured (`memory`, `ai`, `tool`,
    /// `coordinator`, etc.). `None` for peers discovered without an
    /// alias — today the bridge only adds aliased peers, but the
    /// field is honest about future channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Hex NodeId of the peer.
    pub node_id: String,
    /// Node type discriminator: `memory`, `ai`, `tool`, `coordinator`, ...
    pub node_type: String,
    /// Capability method name.
    pub method_name: String,
    pub major_version: u32,
    /// `unary` or `stream_out`.
    pub kind: String,
    /// `idempotent` / `at_most_once` / `at_least_once_safe`.
    pub idempotency: String,
    /// `cheap` / `expensive` / `external_paid`.
    pub cost_class: String,
    pub sensitivity_tags: Vec<String>,
    pub requires_groups: Vec<String>,
    pub policy_attachment_point: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub environment_requirements: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ApiError {
    pub error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        Json(self).into_response()
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct ListQuery {
    /// Filter to capabilities whose `categories` includes this
    /// value. Empty = no filter.
    #[serde(default)]
    pub category: Option<String>,
    /// Filter to capabilities whose `sensitivity_tags` includes
    /// this value. Empty = no filter.
    #[serde(default)]
    pub tag: Option<String>,
}

/// `GET /v1/capabilities` — list every capability the bridge knows
/// about, optionally filtered by category or sensitivity tag.
pub async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Vec<CapabilityEntry>>, (StatusCode, Json<ApiError>)> {
    let entries = collect_entries(&state, None);
    Ok(Json(filter_entries(entries, &q)))
}

/// `GET /v1/capabilities/:method` — same projection but scoped to
/// the matching method. Returns 404 when no peer advertises it.
pub async fn get_one(
    State(state): State<AppState>,
    Path(method): Path<String>,
) -> Result<Json<Vec<CapabilityEntry>>, (StatusCode, Json<ApiError>)> {
    let entries = collect_entries(&state, Some(&method));
    if entries.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ApiError {
                error: format!("no peer advertises capability '{method}'"),
            }),
        ));
    }
    Ok(Json(entries))
}

/// Walk every cached manifest + every capability inside, optionally
/// filtering by exact method name.
fn collect_entries(state: &AppState, method_filter: Option<&str>) -> Vec<CapabilityEntry> {
    let mut out = Vec::new();
    for cached in state.manifest_cache.entries() {
        for cap in &cached.manifest.capabilities {
            if let Some(m) = method_filter
                && cap.method_name != m
            {
                continue;
            }
            out.push(CapabilityEntry {
                alias: cached.alias.clone(),
                node_id: cached.manifest.node_id.to_string(),
                node_type: cached.manifest.node_type.clone(),
                method_name: cap.method_name.clone(),
                major_version: cap.major_version,
                kind: kind_to_str(cap.kind).into(),
                idempotency: idempotency_to_str(cap.idempotency).into(),
                cost_class: cost_class_to_str(cap.cost_class).into(),
                sensitivity_tags: cap.sensitivity_tags.clone(),
                requires_groups: cap.requires_groups.clone(),
                policy_attachment_point: cap.policy_attachment_point.clone(),
                description: cap.description.clone(),
                categories: cap.categories.clone(),
                environment_requirements: cap.environment_requirements.clone(),
            });
        }
    }
    out
}

/// Client-side filter pass over collected entries.
fn filter_entries(entries: Vec<CapabilityEntry>, q: &ListQuery) -> Vec<CapabilityEntry> {
    entries
        .into_iter()
        .filter(|e| {
            if let Some(c) = q.category.as_deref()
                && !c.is_empty()
                && !e.categories.iter().any(|x| x == c)
            {
                return false;
            }
            if let Some(t) = q.tag.as_deref()
                && !t.is_empty()
                && !e.sensitivity_tags.iter().any(|x| x == t)
            {
                return false;
            }
            true
        })
        .collect()
}

fn kind_to_str(k: relix_core::capability::CapabilityKind) -> &'static str {
    use relix_core::capability::CapabilityKind::*;
    match k {
        Unary => "unary",
        StreamOut => "stream_out",
    }
}

fn idempotency_to_str(i: relix_core::capability::Idempotency) -> &'static str {
    use relix_core::capability::Idempotency::*;
    match i {
        Idempotent => "idempotent",
        AtMostOnce => "at_most_once",
        AtLeastOnceSafe => "at_least_once_safe",
    }
}

fn cost_class_to_str(c: relix_core::capability::CostClass) -> &'static str {
    use relix_core::capability::CostClass::*;
    match c {
        Cheap => "cheap",
        Expensive => "expensive",
        ExternalPaid => "external_paid",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::capability::{CapabilityKind, CostClass, Idempotency};

    fn cap(method: &str) -> relix_core::capability::CapabilityDescriptor {
        let mut d = relix_core::capability::CapabilityDescriptor::unary(method);
        d.categories = vec!["parse".into()];
        d.sensitivity_tags = vec!["parse:html".into()];
        d
    }

    fn cap_with_tag(method: &str, tag: &str) -> relix_core::capability::CapabilityDescriptor {
        let mut d = relix_core::capability::CapabilityDescriptor::unary(method);
        d.sensitivity_tags = vec![tag.to_string()];
        d
    }

    fn entry(method: &str) -> CapabilityEntry {
        CapabilityEntry {
            alias: Some("tool".into()),
            node_id: "deadbeef".into(),
            node_type: "tool".into(),
            method_name: method.into(),
            major_version: 1,
            kind: "unary".into(),
            idempotency: "idempotent".into(),
            cost_class: "cheap".into(),
            sensitivity_tags: vec!["parse:html".into()],
            requires_groups: vec![],
            policy_attachment_point: method.into(),
            description: None,
            categories: vec!["parse".into()],
            environment_requirements: vec![],
        }
    }

    #[test]
    fn filter_by_category_matches_only_matching_entries() {
        let entries = vec![entry("a"), entry("b")];
        let q = ListQuery {
            category: Some("parse".into()),
            tag: None,
        };
        assert_eq!(filter_entries(entries, &q).len(), 2);
        let entries = vec![entry("a"), entry("b")];
        let q = ListQuery {
            category: Some("fetch".into()),
            tag: None,
        };
        assert_eq!(filter_entries(entries, &q).len(), 0);
    }

    #[test]
    fn filter_by_tag_matches_only_matching_entries() {
        let entries = vec![entry("a"), entry("b")];
        let q = ListQuery {
            category: None,
            tag: Some("parse:html".into()),
        };
        assert_eq!(filter_entries(entries, &q).len(), 2);
        let entries = vec![entry("a")];
        let q = ListQuery {
            category: None,
            tag: Some("external:network".into()),
        };
        assert_eq!(filter_entries(entries, &q).len(), 0);
    }

    #[test]
    fn empty_filters_pass_everything_through() {
        let entries = vec![entry("a"), entry("b"), entry("c")];
        let q = ListQuery {
            category: Some(String::new()),
            tag: Some(String::new()),
        };
        assert_eq!(filter_entries(entries, &q).len(), 3);
    }

    #[test]
    fn kind_idempotency_cost_class_strings_match_serde_naming() {
        // These string values are part of the public JSON contract.
        // Catch regressions where a future enum variant is added but
        // the projector forgets to map it.
        assert_eq!(kind_to_str(CapabilityKind::Unary), "unary");
        assert_eq!(kind_to_str(CapabilityKind::StreamOut), "stream_out");
        assert_eq!(idempotency_to_str(Idempotency::Idempotent), "idempotent");
        assert_eq!(idempotency_to_str(Idempotency::AtMostOnce), "at_most_once");
        assert_eq!(
            idempotency_to_str(Idempotency::AtLeastOnceSafe),
            "at_least_once_safe"
        );
        assert_eq!(cost_class_to_str(CostClass::Cheap), "cheap");
        assert_eq!(cost_class_to_str(CostClass::Expensive), "expensive");
        assert_eq!(cost_class_to_str(CostClass::ExternalPaid), "external_paid");
    }

    #[test]
    fn cap_helpers_compile_with_optional_fields() {
        // Smoke test for the test helpers; ensures the descriptor
        // builder still produces what we expect with the new P1
        // fields present.
        let d = cap("tool.x");
        assert_eq!(d.method_name, "tool.x");
        assert!(d.description.is_none());
        assert_eq!(d.categories, vec!["parse".to_string()]);
        let d2 = cap_with_tag("tool.y", "external:network");
        assert_eq!(d2.sensitivity_tags, vec!["external:network".to_string()]);
    }
}
