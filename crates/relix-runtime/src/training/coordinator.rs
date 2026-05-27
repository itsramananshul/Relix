//! RELIX-7.15 — coordinator-side capability registration.
//!
//! Six capabilities, all unary, all JSON-encoded:
//!
//! - `training.list_interactions`
//! - `training.get_interaction`
//! - `training.export`
//! - `training.score_interaction`
//! - `training.stats`
//! - `training.delete_interaction`

use std::path::PathBuf;
use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};
use serde::Deserialize;

use super::exporter::{ExportEngine, ExportFilters, ExportFormat};
use super::scorer;
use super::store::{ListFilters, TrainingStore};
use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// Wire every training capability onto `bridge`.
pub fn register(bridge: &mut DispatchBridge, store: TrainingStore, export_dir: PathBuf) {
    register_list_interactions(bridge, store.clone());
    register_get_interaction(bridge, store.clone());
    register_score_interaction(bridge, store.clone());
    register_stats(bridge, store.clone());
    register_delete_interaction(bridge, store.clone());
    register_export(bridge, store, export_dir);
}

#[derive(Debug, Deserialize, Default)]
struct ListArgs {
    #[serde(default = "default_page")]
    page: u32,
    #[serde(default = "default_page_size")]
    page_size: u32,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    min_quality_score: Option<f32>,
    #[serde(default)]
    date_from: Option<i64>,
    #[serde(default)]
    date_to: Option<i64>,
    #[serde(default)]
    exported: Option<bool>,
}

fn default_page() -> u32 {
    1
}
fn default_page_size() -> u32 {
    50
}

fn register_list_interactions(bridge: &mut DispatchBridge, store: TrainingStore) {
    bridge.register(
        "training.list_interactions",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = store.clone();
            async move {
                let args = match decode::<ListArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(out) => return out,
                };
                let filters = ListFilters {
                    agent: args.agent,
                    session_id: args.session_id,
                    model: args.model,
                    min_quality_score: args.min_quality_score,
                    date_from: args.date_from,
                    date_to: args.date_to,
                    exported: args.exported,
                    require_scored: false,
                };
                match store.list_summaries(&filters, args.page, args.page_size) {
                    Ok(rows) => ok_json(&rows),
                    Err(e) => internal(&e),
                }
            }
        })),
    );
}

#[derive(Debug, Deserialize, Default)]
struct IdArgs {
    #[serde(default)]
    interaction_id: String,
}

fn register_get_interaction(bridge: &mut DispatchBridge, store: TrainingStore) {
    bridge.register(
        "training.get_interaction",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = store.clone();
            async move {
                let args = match decode::<IdArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(out) => return out,
                };
                if args.interaction_id.trim().is_empty() {
                    return invalid("interaction_id is required");
                }
                match store.get(&args.interaction_id) {
                    Ok(Some(rec)) => ok_json(&rec),
                    Ok(None) => not_found(&args.interaction_id),
                    Err(e) => internal(&e),
                }
            }
        })),
    );
}

fn register_score_interaction(bridge: &mut DispatchBridge, store: TrainingStore) {
    bridge.register(
        "training.score_interaction",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = store.clone();
            async move {
                let args = match decode::<IdArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(out) => return out,
                };
                if args.interaction_id.trim().is_empty() {
                    return invalid("interaction_id is required");
                }
                match scorer::score_one(&store, &args.interaction_id) {
                    Ok(Some(s)) => ok_json(&serde_json::json!({
                        "interaction_id": args.interaction_id,
                        "quality_score": s,
                    })),
                    Ok(None) => not_found(&args.interaction_id),
                    Err(e) => internal(&e),
                }
            }
        })),
    );
}

fn register_stats(bridge: &mut DispatchBridge, store: TrainingStore) {
    bridge.register(
        "training.stats",
        Arc::new(FnHandler(move |_ctx: InvocationCtx| {
            let store = store.clone();
            async move {
                match store.stats() {
                    Ok(s) => ok_json(&s),
                    Err(e) => internal(&e),
                }
            }
        })),
    );
}

fn register_delete_interaction(bridge: &mut DispatchBridge, store: TrainingStore) {
    bridge.register(
        "training.delete_interaction",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = store.clone();
            async move {
                let args = match decode::<IdArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(out) => return out,
                };
                if args.interaction_id.trim().is_empty() {
                    return invalid("interaction_id is required");
                }
                match store.delete(&args.interaction_id) {
                    Ok(true) => ok_json(&serde_json::json!({
                        "interaction_id": args.interaction_id,
                        "deleted": true,
                    })),
                    Ok(false) => not_found(&args.interaction_id),
                    Err(e) => internal(&e),
                }
            }
        })),
    );
}

#[derive(Debug, Deserialize, Default)]
struct ExportArgs {
    #[serde(default)]
    format: String,
    #[serde(default)]
    export_set: String,
    #[serde(default)]
    output_dir: Option<String>,
    #[serde(flatten)]
    filters: ExportFilters,
}

fn register_export(bridge: &mut DispatchBridge, store: TrainingStore, default_output_dir: PathBuf) {
    bridge.register(
        "training.export",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let store = store.clone();
            let default_output_dir = default_output_dir.clone();
            async move {
                let args = match decode::<ExportArgs>(&ctx.args) {
                    Ok(a) => a,
                    Err(out) => return out,
                };
                let Some(format) = ExportFormat::from_str_loose(&args.format) else {
                    return invalid(&format!(
                        "unsupported format {fmt:?}; expected one of openai / anthropic / generic / raw_json",
                        fmt = args.format
                    ));
                };
                if args.export_set.trim().is_empty() {
                    return invalid("export_set is required");
                }
                let out_dir = args
                    .output_dir
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or(default_output_dir);
                let engine = ExportEngine::new(store, out_dir);
                let now = super::recorder::now_ms();
                match engine.export(format, &args.filters, &args.export_set, now) {
                    Ok(res) => ok_json(&res),
                    Err(super::exporter::ExportError::InvalidArgs(m)) => invalid(&m),
                    Err(e) => internal(&e),
                }
            }
        })),
    );
}

// ── shared helpers ────────────────────────────────────────────

fn decode<T: serde::de::DeserializeOwned + Default>(args: &[u8]) -> Result<T, HandlerOutcome> {
    if args.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(args).map_err(|e| invalid(&format!("decode args: {e}")))
}

fn ok_json<T: serde::Serialize>(value: &T) -> HandlerOutcome {
    match serde_json::to_vec(value) {
        Ok(b) => HandlerOutcome::Ok(b),
        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("training: encode response: {e}"),
            retry_hint: 0,
            retry_after: None,
        }),
    }
}

fn invalid(msg: &str) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause: msg.to_string(),
        retry_hint: 0,
        retry_after: None,
    })
}

fn not_found(id: &str) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause: format!("training: no interaction with id {id:?}"),
        retry_hint: 0,
        retry_after: None,
    })
}

fn internal<E: std::fmt::Display>(e: &E) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::RESPONDER_INTERNAL,
        cause: format!("training: {e}"),
        retry_hint: 0,
        retry_after: None,
    })
}

/// Static descriptor list for the six capabilities. Mirrors
/// `metrics_capability_descriptors` in `controller_runtime`.
pub fn training_capability_descriptors() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "training.list_interactions",
            "Paginate recorded training interactions. Args: JSON \
             { page?, page_size?, agent?, session_id?, model?, \
               min_quality_score?, date_from?, date_to?, exported? }. \
             Returns lightweight summaries (no full prompts).",
        ),
        (
            "training.get_interaction",
            "Fetch a single interaction by id. Args: JSON \
             { interaction_id }. Returns the full record including \
             system_prompt / user_message / response / tool_calls.",
        ),
        (
            "training.export",
            "Materialise an export file. Args: JSON \
             { format, export_set, output_dir?, min_quality_score?, agent?, session_id?, \
               date_from?, date_to?, max_interactions?, include_tool_calls? }. \
             Returns { matched_count, exported_count, output_path?, total_tokens }.",
        ),
        (
            "training.score_interaction",
            "Re-score one interaction. Args: JSON { interaction_id }. \
             Returns { interaction_id, quality_score }.",
        ),
        (
            "training.stats",
            "Aggregate stats: total / exported / average_quality_score / \
             score_distribution (10 buckets + unscored) / by_agent / by_model. \
             No args.",
        ),
        (
            "training.delete_interaction",
            "Hard-delete an interaction. Args: JSON { interaction_id }. \
             Returns { interaction_id, deleted: true } on success.",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::super::store::TrainingStore;
    use super::super::types::InteractionRecord;
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use relix_core::policy::PolicyEngine;
    use tempfile::TempDir;

    fn fresh_bridge() -> (DispatchBridge, TempDir) {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::permissive();
        let bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        (bridge, dir)
    }

    fn sample(id: &str) -> InteractionRecord {
        InteractionRecord::new(
            super::super::types::InteractionId(id.into()),
            "sess".into(),
            "alice".into(),
            "gpt-4o-mini".into(),
            "openai".into(),
            "you are alice".into(),
            "hello".into(),
            "hi there".into(),
            vec![],
            Some(20),
            Some(30),
            100,
            true,
            None,
            super::super::recorder::now_ms(),
        )
    }

    #[tokio::test]
    async fn capabilities_register_without_panic() {
        let (mut bridge, dir) = fresh_bridge();
        let store = TrainingStore::in_memory().unwrap();
        store.insert(&sample("a")).unwrap();
        register(&mut bridge, store, dir.path().to_path_buf());
        // Capability registration alone should not panic.
        let _snapshot = bridge.capability_stats_snapshot();
    }

    #[test]
    fn descriptors_cover_six_methods() {
        let methods: Vec<&str> = training_capability_descriptors()
            .iter()
            .map(|(m, _)| *m)
            .collect();
        for expected in [
            "training.list_interactions",
            "training.get_interaction",
            "training.export",
            "training.score_interaction",
            "training.stats",
            "training.delete_interaction",
        ] {
            assert!(
                methods.contains(&expected),
                "missing descriptor: {expected}"
            );
        }
    }
}
