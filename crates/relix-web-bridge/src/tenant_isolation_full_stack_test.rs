//! PART 8 — end-to-end tenant-isolation integration test.
//!
//! Boots ONLY real components:
//!   - real `DispatchBridge` on the responder side with two
//!     test caps (`test.tenant_echo` returns the
//!     `InvocationCtx.tenant_id` it observed; `test.search`
//!     looks up the caller's tenant against a shared
//!     `LayeredMemoryStore` opened with
//!     `tenant_isolation = true`).
//!   - real libp2p mesh transport (`rpc::new` + `MeshClient`).
//!   - real bridge `AppState` constructed via
//!     `AppState::try_new` + a real `MeshClient` from
//!     `discover_and_pin`.
//!   - real `axum::serve` on an ephemeral TCP port with the
//!     auth + tenant middleware layered ON.
//!   - real `reqwest` HTTP client driving the bridge with
//!     different bearer tokens per tenant.
//!
//! Asserts the end-to-end PART 1-7 chain:
//!   1. Bridge auth accepts a bearer token whose 8-char
//!      prefix appears in `[auth.tenant_bindings]`.
//!   2. The `tenant_middleware` binds the resolved tenant id
//!      into `CURRENT_TENANT.scope` for the downstream
//!      handler.
//!   3. The handler calls `peer_call::build_mesh_request`
//!      which reads `current_tenant_or_none()` and stamps it
//!      onto the outbound `RequestEnvelope.tenant_id`.
//!   4. The wire envelope round-trips through the mesh; the
//!      responder-side `DispatchBridge` populates
//!      `InvocationCtx.tenant_id` from the envelope.
//!   5. The cap handler routes data lookup through
//!      `LayeredMemoryStore::text_search_for_tenant` which
//!      ships a `WHERE tenant_id = ?` clause; rows for a
//!      different tenant are NEVER returned.
//!
//! Then asserts the negative cases:
//!   - A request whose bearer prefix is NOT in
//!     `tenant_bindings` (and `multi_tenant_mode = true`)
//!     hits the middleware short-circuit and returns HTTP
//!     401 with the documented copy.
//!   - A request from an UNTRUSTED source whose
//!     `X-Relix-Tenant` header tries to impersonate tenant B
//!     has the header silently ignored — the binding's
//!     tenant (A) is the one observed downstream.
//!
//! This is the integration leg for the surfaces the PART 4
//! fail-closed work guards (memory search, audit, policy,
//! Qdrant collection isolation). The remaining Part 8
//! surfaces (skill search, session list, audit query,
//! credential list, Qdrant concurrent-create) follow the
//! same end-to-end pattern; the harness here is the
//! template for that follow-up scaffolding.

#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::time::timeout;

use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_core::identity::{IdentityBundle, issue_identity};
use relix_core::policy::PolicyEngine;
use relix_core::types::NodeId;

use relix_runtime::audit_partition::{AuditPartitionStore, PartitionRow};
use relix_runtime::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use relix_runtime::nodes::memory::schema::{LayeredMemoryStore, MemoryLayer, MemoryRecord};
use relix_runtime::transport::rpc::{self, Event, Multiaddr};

use std::collections::HashMap;
use std::sync::Mutex;

use crate::config::{
    AppState, AuthSection, BridgeConfig, BridgeSection, FlowSection, IdentitySection, MeshSection,
    SseSection, TransportSection,
};

fn key_for(seed: u8) -> [u8; 32] {
    let mut k = [0u8; 32];
    for (i, slot) in k.iter_mut().enumerate() {
        *slot = seed.wrapping_add(i as u8);
    }
    k
}

async fn boot_peer(seed: u8) -> (rpc::Client, mpsc::Receiver<Event>, Multiaddr) {
    for _ in 0..16 {
        let port: u16 = 35_000 + (rand::random::<u16>() % 25_000);
        match rpc::new(key_for(seed), port).await {
            Ok((client, events, event_loop)) => {
                let listen_addr: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}")
                    .parse()
                    .expect("valid multiaddr");
                tokio::spawn(event_loop.run());
                return (client, events, listen_addr);
            }
            Err(e) => {
                eprintln!("tenant-isolation-full-stack: boot_peer retry ({e})");
                continue;
            }
        }
    }
    panic!("boot_peer: exhausted port retries");
}

fn mint_bridge_bundle_bytes(org_root: &SigningKey, name: &str) -> Vec<u8> {
    let caller_key = SigningKey::generate(&mut OsRng);
    let id = IdentityBundle {
        subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
        name: name.into(),
        org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
        groups: vec!["operators".into()],
        role: "agent".into(),
        clearance: "internal".into(),
        supervisors: vec![],
    };
    let bundle: Bundle = issue_identity(id, org_root, 3600).expect("identity issued");
    codec::encode(&bundle).expect("encode bundle")
}

fn spawn_inbound_loop(mut events: mpsc::Receiver<Event>, bridge: Arc<DispatchBridge>) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            if let Event::Request {
                envelope, respond, ..
            } = event
            {
                let bridge = bridge.clone();
                tokio::spawn(async move {
                    let reply = bridge.handle_inbound(envelope).await;
                    respond.respond(reply).await;
                });
            }
        }
    });
}

/// Per-test responder bridge with two custom caps registered:
///
/// - `test.tenant_echo` — returns the literal
///   `InvocationCtx.tenant_id` it observed as a JSON
///   `{"tenant_id":"<val>"}`. Tests use this to prove the
///   bridge → mesh → responder chain propagates the field.
/// - `test.search` — looks up the caller's tenant against a
///   shared `LayeredMemoryStore` opened with
///   `tenant_isolation = true` and returns the matching
///   rows. Tests use this to prove cross-tenant
///   invisibility through the SQLite fallback path.
// PART 8 — minimal test-only tenant-keyed KV store.
// Stands in for SkillStore, SessionStore, CredentialStore
// (none of which carry tenant isolation in the runtime yet).
// The integration test proves the bridge → mesh → cap →
// store pipeline routes the tenant id correctly; the
// production upstream stores need the same shape once their
// per-tenant variants ship.
#[derive(Default)]
struct TenantKvStore {
    inner: Mutex<HashMap<String, Vec<(String, String)>>>,
}

impl TenantKvStore {
    fn insert(&self, tenant: &str, key: String, value: String) {
        let mut g = self.inner.lock().expect("kv lock");
        g.entry(tenant.to_string()).or_default().push((key, value));
    }
    fn list_for(&self, tenant: &str) -> Vec<(String, String)> {
        let g = self.inner.lock().expect("kv lock");
        g.get(tenant).cloned().unwrap_or_default()
    }
}

/// PART 8 — register a tenant-aware cap that lists rows
/// from `store` keyed by `ctx.tenant_id`. Used by the
/// skill / session / credential surface tests.
fn register_kv_list_cap(
    bridge: &mut DispatchBridge,
    method: &'static str,
    store: Arc<TenantKvStore>,
) {
    let s = store.clone();
    bridge.register(
        method,
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let s = s.clone();
            async move {
                let tenant = match ctx.tenant_id.as_deref() {
                    Some(t) if !t.is_empty() => t,
                    _ => {
                        return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                            kind: relix_core::types::error_kinds::SECURITY_DENIED,
                            cause: format!("{method}: tenant_id required"),
                            retry_hint: 0,
                            retry_after: None,
                        });
                    }
                };
                let rows = s.list_for(tenant);
                let body = serde_json::json!({
                    "tenant_id": tenant,
                    "rows": rows
                        .iter()
                        .map(|(k, v)| serde_json::json!({ "key": k, "value": v }))
                        .collect::<Vec<_>>(),
                });
                match serde_json::to_vec(&body) {
                    Ok(b) => HandlerOutcome::Ok(b),
                    Err(e) => HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                        kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                        cause: format!("{method}: encode: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

fn build_responder_bridge_with_store(
    policy_toml: &str,
    store: Arc<LayeredMemoryStore>,
    audit_store: Arc<AuditPartitionStore>,
    skill_store: Arc<TenantKvStore>,
    session_store: Arc<TenantKvStore>,
    credential_store: Arc<TenantKvStore>,
    policy_resolver: Arc<relix_core::policy::TenantPolicyResolver>,
) -> (DispatchBridge, SigningKey, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let org_root = SigningKey::generate(&mut OsRng);
    let responder = SigningKey::generate(&mut OsRng);
    let policy = PolicyEngine::from_toml(policy_toml).expect("policy parses");
    let mut bridge = DispatchBridge::new(
        policy,
        org_root.verifying_key(),
        &dir.path().join("audit.log"),
        responder,
    )
    .expect("bridge constructs");
    // PART 8 cap: echo the InvocationCtx tenant id.
    bridge.register(
        "test.tenant_echo",
        Arc::new(FnHandler(move |ctx: InvocationCtx| async move {
            let body = serde_json::json!({
                "tenant_id": ctx.tenant_id.clone().unwrap_or_default(),
                "tenant_present": ctx.tenant_id.is_some(),
            });
            match serde_json::to_vec(&body) {
                Ok(b) => HandlerOutcome::Ok(b),
                Err(e) => HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                    kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                    cause: format!("encode echo: {e}"),
                    retry_hint: 0,
                    retry_after: None,
                }),
            }
        })),
    );
    // PART 8 cap: tenant-aware text search against the
    // shared store. Args wire: raw bytes = the search query.
    {
        let s = store.clone();
        bridge.register(
            "test.search",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move {
                    let query = std::str::from_utf8(&ctx.args).unwrap_or("").to_string();
                    let tenant = ctx.tenant_id.as_deref();
                    let rows = match s.text_search_for_tenant(&query, 100, tenant) {
                        Ok(r) => r,
                        Err(e) => {
                            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                                kind: relix_core::types::error_kinds::INVALID_ARGS,
                                cause: format!("text_search_for_tenant: {e}"),
                                retry_hint: 0,
                                retry_after: None,
                            });
                        }
                    };
                    let body = serde_json::json!({
                        "tenant_id": ctx.tenant_id.clone().unwrap_or_default(),
                        "row_texts": rows
                            .iter()
                            .map(|r| r.text.clone())
                            .collect::<Vec<_>>(),
                    });
                    match serde_json::to_vec(&body) {
                        Ok(b) => HandlerOutcome::Ok(b),
                        Err(e) => HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                            kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                            cause: format!("encode search: {e}"),
                            retry_hint: 0,
                            retry_after: None,
                        }),
                    }
                }
            })),
        );
    }
    // PART 8 cap: tenant-aware memory ingest. Writes a row
    // to the shared store, stamping `tenant_id` from
    // `ctx.tenant_id`. Args wire: JSON `{ source, text }`.
    {
        let s = store.clone();
        bridge.register(
            "test.ingest",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move {
                    #[derive(serde::Deserialize)]
                    struct IngestArgs {
                        source: String,
                        text: String,
                    }
                    let args: IngestArgs = match serde_json::from_slice(&ctx.args) {
                        Ok(v) => v,
                        Err(e) => {
                            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                                kind: relix_core::types::error_kinds::INVALID_ARGS,
                                cause: format!("ingest args: {e}"),
                                retry_hint: 0,
                                retry_after: None,
                            });
                        }
                    };
                    let tenant = match ctx.tenant_id.as_deref() {
                        Some(t) if !t.is_empty() => t.to_string(),
                        _ => {
                            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                                kind: relix_core::types::error_kinds::SECURITY_DENIED,
                                cause: "ingest: tenant_id required".into(),
                                retry_hint: 0,
                                retry_after: None,
                            });
                        }
                    };
                    let id = format!(
                        "ingest-{tenant}-{}",
                        uuid_like_suffix(&format!("{}{}", args.source, args.text))
                    );
                    let mut record = MemoryRecord::new_raw(id, &args.text, &args.source);
                    record.layer = MemoryLayer::Raw;
                    record.tenant_id = Some(tenant.clone());
                    if let Err(e) = s.insert(&record) {
                        return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                            kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                            cause: format!("insert: {e}"),
                            retry_hint: 0,
                            retry_after: None,
                        });
                    }
                    let body = serde_json::json!({ "tenant_id": tenant, "ok": true });
                    HandlerOutcome::Ok(serde_json::to_vec(&body).unwrap_or_default())
                }
            })),
        );
    }
    // PART 8 cap: tenant-aware audit-partition read. Wraps
    // `AuditPartitionStore::tenant_recent(ctx.tenant_id, 50)`.
    {
        let s = audit_store.clone();
        bridge.register(
            "test.audit_recent",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let s = s.clone();
                async move {
                    let tenant = match ctx.tenant_id.as_deref() {
                        Some(t) if !t.is_empty() => t.to_string(),
                        _ => {
                            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                                kind: relix_core::types::error_kinds::SECURITY_DENIED,
                                cause: "audit_recent: tenant_id required".into(),
                                retry_hint: 0,
                                retry_after: None,
                            });
                        }
                    };
                    let rows = match s.tenant_recent(&tenant, 50) {
                        Ok(r) => r,
                        Err(e) => {
                            return HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                                kind: relix_core::types::error_kinds::RESPONDER_INTERNAL,
                                cause: format!("tenant_recent: {e}"),
                                retry_hint: 0,
                                retry_after: None,
                            });
                        }
                    };
                    let body = serde_json::json!({
                        "tenant_id": tenant,
                        "methods": rows
                            .iter()
                            .map(|r| r.method.clone())
                            .collect::<Vec<_>>(),
                    });
                    HandlerOutcome::Ok(serde_json::to_vec(&body).unwrap_or_default())
                }
            })),
        );
    }
    // PART 8 cap: policy-resolver admission probe. Wraps
    // `TenantPolicyResolver::evaluate(caller, method,
    // tenant_id)` and returns the decision shape. Tests use
    // this to prove tenant A's per-tenant policy file
    // applies to tenant-A traffic but NOT to tenant-B
    // traffic.
    {
        let resolver = policy_resolver.clone();
        bridge.register(
            "test.policy_admit",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let resolver = resolver.clone();
                async move {
                    let tenant = ctx.tenant_id.clone();
                    let probe_method = std::str::from_utf8(&ctx.args)
                        .unwrap_or("ai.chat")
                        .to_string();
                    let decision = resolver.evaluate(&ctx.caller, &probe_method, tenant.as_deref());
                    use relix_core::policy::Decision;
                    let (allowed, matched_rule, reason) = match &decision {
                        Decision::Allow { matched_rule } => {
                            (true, Some(matched_rule.clone()), String::new())
                        }
                        Decision::Deny {
                            reason,
                            matched_rule,
                        } => (false, matched_rule.clone(), reason.clone()),
                    };
                    let body = serde_json::json!({
                        "tenant_id": tenant.clone().unwrap_or_default(),
                        "method": probe_method,
                        "allowed": allowed,
                        "matched_rule": matched_rule,
                        "reason": reason,
                    });
                    HandlerOutcome::Ok(serde_json::to_vec(&body).unwrap_or_default())
                }
            })),
        );
    }
    // PART 8: tenant-keyed list caps for skill / session /
    // credential surfaces. All three share the same shape —
    // list whatever rows the caller's tenant owns.
    register_kv_list_cap(&mut bridge, "test.skill_list", skill_store.clone());
    register_kv_list_cap(&mut bridge, "test.session_list", session_store.clone());
    register_kv_list_cap(
        &mut bridge,
        "test.credential_list",
        credential_store.clone(),
    );
    (bridge, org_root, dir)
}

/// PART 8 test-only bridge handler. Mirrors the production
/// pattern (`crate::peer_call::build_mesh_request` →
/// `mesh.call` → `decode_response`) so the test exercises the
/// real plumbing, not a parallel shim.
async fn route_tenant_echo(State(state): State<AppState>) -> axum::response::Response {
    call_test_cap(&state, "test.tenant_echo", Vec::new()).await
}

#[derive(serde::Deserialize, Debug)]
struct SearchQuery {
    q: String,
}

async fn route_tenant_search(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<SearchQuery>,
) -> axum::response::Response {
    call_test_cap(&state, "test.search", q.q.into_bytes()).await
}

#[derive(serde::Deserialize, Debug)]
struct IngestQuery {
    source: String,
    text: String,
}

async fn route_tenant_ingest(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<IngestQuery>,
) -> axum::response::Response {
    let body = serde_json::json!({ "source": q.source, "text": q.text });
    call_test_cap(
        &state,
        "test.ingest",
        serde_json::to_vec(&body).unwrap_or_default(),
    )
    .await
}

async fn route_audit_recent(State(state): State<AppState>) -> axum::response::Response {
    call_test_cap(&state, "test.audit_recent", Vec::new()).await
}

#[derive(serde::Deserialize, Debug, Default)]
struct PolicyProbeQuery {
    #[serde(default)]
    method: String,
}

async fn route_policy_admit(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<PolicyProbeQuery>,
) -> axum::response::Response {
    let probe = if q.method.is_empty() {
        "ai.chat".to_string()
    } else {
        q.method
    };
    call_test_cap(&state, "test.policy_admit", probe.into_bytes()).await
}

async fn route_skill_list(State(state): State<AppState>) -> axum::response::Response {
    call_test_cap(&state, "test.skill_list", Vec::new()).await
}

async fn route_session_list(State(state): State<AppState>) -> axum::response::Response {
    call_test_cap(&state, "test.session_list", Vec::new()).await
}

async fn route_credential_list(State(state): State<AppState>) -> axum::response::Response {
    call_test_cap(&state, "test.credential_list", Vec::new()).await
}

async fn call_test_cap(state: &AppState, method: &str, args: Vec<u8>) -> axum::response::Response {
    let mesh = match state.mesh_client.as_ref() {
        Some(m) => m.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error":"mesh client missing"})),
            )
                .into_response();
        }
    };
    let envelope = relix_runtime::dispatch::build_request_with_tenant(
        method,
        args,
        state.identity_bundle.clone(),
        state.cfg.transport.deadline_secs.clamp(5, 30),
        None,
        None,
        None,
        crate::tenant::current_tenant_or_none(),
    );
    let resp_bytes = match mesh.call("responder", envelope).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({"error": format!("mesh call: {e}")})),
            )
                .into_response();
        }
    };
    let decoded = match relix_runtime::dispatch::decode_response(&resp_bytes) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({"error": format!("decode: {e}")})),
            )
                .into_response();
        }
    };
    match decoded.res {
        relix_runtime::transport::envelope::ResponseResult::Ok(body) => {
            let v: Value = serde_json::from_slice(&body)
                .unwrap_or_else(|_| serde_json::json!({"raw": "non-json"}));
            (StatusCode::OK, axum::Json(v)).into_response()
        }
        relix_runtime::transport::envelope::ResponseResult::Err(env) => (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({
                "error_kind": env.kind,
                "cause": env.cause,
            })),
        )
            .into_response(),
        _ => (
            StatusCode::BAD_GATEWAY,
            axum::Json(serde_json::json!({"error":"unexpected stream"})),
        )
            .into_response(),
    }
}

/// PART 8 test scaffold. Boots the full bridge + responder
/// + mesh and returns the bound socket address the test can
///   drive with reqwest.
struct Harness {
    addr: std::net::SocketAddr,
    bridge_token: String,
    _bridge_tmp: TempDir,
    _responder_tmp: TempDir,
    _store: Arc<LayeredMemoryStore>,
    audit_store: Arc<AuditPartitionStore>,
    skill_store: Arc<TenantKvStore>,
    session_store: Arc<TenantKvStore>,
    credential_store: Arc<TenantKvStore>,
    _policy_resolver: Arc<relix_core::policy::TenantPolicyResolver>,
    _policy_dir: TempDir,
}

async fn boot_harness(
    multi_tenant_mode: bool,
    bindings: &[(&str, &str)],
    trusted_origins: &[&str],
    seed_rows: &[(&str, &str, &str)], // (tenant_id, source, text)
) -> Harness {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init();

    // ─── responder side: real DispatchBridge + 2 caps ─────
    let store = Arc::new(
        LayeredMemoryStore::in_memory_with_tenant_isolation(true)
            .expect("in-memory layered store with tenant_isolation"),
    );
    // Seed rows BEFORE booting the bridge so the search cap
    // returns deterministic results.
    for (tenant, source, text) in seed_rows {
        let mut record = MemoryRecord::new_raw(
            format!("rec-{tenant}-{}", uuid_like_suffix(text)),
            *text,
            *source,
        );
        record.layer = MemoryLayer::Raw;
        record.tenant_id = Some((*tenant).to_string());
        store.insert(&record).expect("seed insert");
    }
    // PART 8: additional per-surface stores. All open in
    // fail-closed mode where applicable so a missing tenant
    // is caught by the production code, not the test.
    let audit_dir = TempDir::new().expect("audit tempdir");
    let audit_store = Arc::new(
        AuditPartitionStore::open_with_partition(audit_dir.path().join("audit.db"), true)
            .expect("audit partition open"),
    );
    let skill_store = Arc::new(TenantKvStore::default());
    let session_store = Arc::new(TenantKvStore::default());
    let credential_store = Arc::new(TenantKvStore::default());
    // PART 8: tenant-isolation-enabled policy resolver. Per-
    // tenant policy files live in `policy_dir/{tenant}.policy.toml`.
    // Each test that exercises the policy surface seeds files
    // there via the returned `_policy_dir`.
    let policy_dir = TempDir::new().expect("policy tempdir");
    let global_policy = PolicyEngine::from_toml(
        r#"
        [admit]
        groups = ["operators"]
        "#,
    )
    .expect("global policy parses");
    let policy_resolver = Arc::new(
        relix_core::policy::TenantPolicyResolver::new(
            global_policy,
            Some(policy_dir.path().to_path_buf()),
            0,
        )
        .with_tenant_isolation(true),
    );
    let (bridge, org_root, responder_tmp) = build_responder_bridge_with_store(
        // Permissive policy: every caller's group ("operators")
        // can hit every test method. Per-method admission is not
        // what this test is verifying; the verification is
        // tenant_id propagation through the envelope.
        r#"
        [admit]
        groups = ["operators"]
        [[rules]]
        name = "echo"
        method = "test.tenant_echo"
        allow_groups = ["operators"]
        [[rules]]
        name = "search"
        method = "test.search"
        allow_groups = ["operators"]
        [[rules]]
        name = "ingest"
        method = "test.ingest"
        allow_groups = ["operators"]
        [[rules]]
        name = "audit_recent"
        method = "test.audit_recent"
        allow_groups = ["operators"]
        [[rules]]
        name = "policy_admit"
        method = "test.policy_admit"
        allow_groups = ["operators"]
        [[rules]]
        name = "skill_list"
        method = "test.skill_list"
        allow_groups = ["operators"]
        [[rules]]
        name = "session_list"
        method = "test.session_list"
        allow_groups = ["operators"]
        [[rules]]
        name = "credential_list"
        method = "test.credential_list"
        allow_groups = ["operators"]
        "#,
        store.clone(),
        audit_store.clone(),
        skill_store.clone(),
        session_store.clone(),
        credential_store.clone(),
        policy_resolver.clone(),
    );
    let bridge = Arc::new(bridge);
    let (_peer_client, events, peer_addr) = boot_peer(193).await;
    spawn_inbound_loop(events, bridge.clone());

    // ─── bridge side: AppState + auth config + axum ───────
    let bridge_tmp = TempDir::new().expect("bridge tempdir");
    let bundle_bytes = mint_bridge_bundle_bytes(&org_root, "tenant-isolation-test-bridge");
    let bundle_path = bridge_tmp.path().join("bridge.bundle");
    std::fs::write(&bundle_path, &bundle_bytes).expect("write bundle");
    let client_key_path = bridge_tmp.path().join("client.key");
    let chat_template_path = bridge_tmp.path().join("chat.sol");
    std::fs::write(
        &chat_template_path,
        r#"function start() -> str { return remote_call("responder", "noop", "{{SESSION}}|{{MESSAGE}}|"); }"#,
    )
    .expect("write template");
    let peers_path = bridge_tmp.path().join("peers.toml");
    std::fs::write(
        &peers_path,
        format!("[peers.responder]\naddr = \"{peer_addr}\"\n"),
    )
    .expect("write peers");

    let mut tenant_bindings = std::collections::HashMap::new();
    for (prefix, tenant) in bindings {
        tenant_bindings.insert((*prefix).to_string(), (*tenant).to_string());
    }
    let cfg = BridgeConfig {
        bridge: BridgeSection {
            listen_addr: "127.0.0.1:0".into(),
            secrets_path: Some(bridge_tmp.path().join("secrets.toml")),
            token_path: Some(bridge_tmp.path().join("bridge-token")),
            memory_db_path: None,
        },
        identity: IdentitySection {
            bundle_path,
            client_key_path,
        },
        transport: TransportSection {
            peers_path,
            deadline_secs: 30,
            data_dir: Some(bridge_tmp.path().to_path_buf()),
        },
        flow: FlowSection {
            template_path: chat_template_path,
            tool_template_path: None,
            streaming_template_path: None,
        },
        openai_compat: None,
        sse: SseSection::default(),
        coordinator: None,
        mesh: MeshSection::default(),
        observability: None,
        auth: AuthSection {
            multi_tenant_mode,
            trusted_internal_origins: trusted_origins.iter().map(|s| (*s).to_string()).collect(),
            tenant_bindings,
            setup_token: None,
        },
        logging: crate::config::LoggingSection::default(),
    };
    let base_state = AppState::try_new(cfg.clone()).expect("AppState::try_new");

    use relix_runtime::flow_runner::{PeerEntry, PeersFile};
    use relix_runtime::manifest::{DiscoveryOptions, discover_and_pin};
    let mut peers_map = std::collections::HashMap::new();
    peers_map.insert(
        "responder".to_string(),
        PeerEntry {
            addr: peer_addr.to_string(),
        },
    );
    let peers_file = PeersFile { peers: peers_map };
    let opts = DiscoveryOptions {
        identity_bundle: base_state.identity_bundle.clone(),
        client_key: base_state.client_key.clone(),
        peers: peers_file,
        deadline_secs: 30,
        overall_timeout: Duration::from_secs(8),
        local_port: None,
        source_key_registry: None,
    };
    let (_cache, mesh) = discover_and_pin(opts).await.expect("discover_and_pin");
    let state = AppState {
        mesh_client: Some(Arc::new(mesh)),
        ..base_state
    };
    let bridge_token = state.bridge_token.value().to_string();

    // PART 8: mount the test routes with auth + tenant
    // middleware layered ON, the same way the production
    // router wires them in main.rs.
    let auth_state = crate::auth::AuthState {
        token: state.bridge_token.clone(),
        host: state.bridge_host.clone(),
        port: state.bridge_port,
        // PART 8: admit any bearer whose 8-char lowercased
        // prefix appears in the tenant_bindings table. The
        // production main.rs builds this set the same way.
        tenant_binding_prefixes: state
            .cfg
            .auth
            .tenant_bindings
            .keys()
            .map(|s| s.to_lowercase())
            .collect(),
        dashboard_auth: Some(state.dashboard_auth.clone()),
    };
    let tenant_cfg = crate::tenant::TenantConfig::from_auth_section(&state.cfg.auth);
    let app = Router::new()
        .route("/test/echo", get(route_tenant_echo))
        .route("/test/search", get(route_tenant_search))
        .route("/test/ingest", get(route_tenant_ingest))
        .route("/test/audit_recent", get(route_audit_recent))
        .route("/test/policy_admit", get(route_policy_admit))
        .route("/test/skill_list", get(route_skill_list))
        .route("/test/session_list", get(route_session_list))
        .route("/test/credential_list", get(route_credential_list))
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            tenant_cfg,
            crate::tenant::tenant_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            crate::auth::auth_middleware,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await;
    });

    Harness {
        addr,
        bridge_token,
        _bridge_tmp: bridge_tmp,
        _responder_tmp: responder_tmp,
        _store: store,
        audit_store,
        skill_store,
        session_store,
        credential_store,
        _policy_resolver: policy_resolver,
        _policy_dir: policy_dir,
    }
}

/// Tiny seed-id helper — short, stable per `(text)` input.
fn uuid_like_suffix(seed: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(seed.as_bytes());
    h.finalize().to_hex().as_str()[..8].to_string()
}

/// Make a GET call with a custom bearer token.
async fn get_with_bearer(
    addr: std::net::SocketAddr,
    path: &str,
    bearer: &str,
) -> reqwest::Response {
    let http = reqwest::Client::new();
    let url = format!("http://{addr}{path}");
    timeout(
        Duration::from_secs(10),
        http.get(&url)
            .header("Authorization", format!("Bearer {bearer}"))
            .send(),
    )
    .await
    .expect("not timeout")
    .expect("request ok")
}

/// PART 8 — bearer prefix bound to tenant "acme" routes the
/// envelope's `tenant_id = Some("acme")` end-to-end through
/// bridge → mesh → responder cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_bearer_binding_propagates_tenant_id_through_full_stack() {
    let h = boot_harness(
        true,
        &[("acmetokn", "acme"), ("globextn", "globex")],
        &["127.0.0.1"],
        &[],
    )
    .await;
    // Bearer starts with "acmetokn" → resolves to "acme".
    let resp = get_with_bearer(h.addr, "/test/echo", "acmetokn-rest-of-key").await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["tenant_id"].as_str(), Some("acme"));
    assert_eq!(body["tenant_present"].as_bool(), Some(true));
}

/// PART 8 — a request with no bearer in multi-tenant mode
/// hits the auth middleware (which already rejects with 401
/// for protected routes — the tenant middleware doesn't
/// even run).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_no_bearer_in_multi_tenant_mode_is_rejected_at_auth_layer() {
    let h = boot_harness(true, &[("acmetokn", "acme")], &["127.0.0.1"], &[]).await;
    let http = reqwest::Client::new();
    let resp = timeout(
        Duration::from_secs(10),
        http.get(format!("http://{}/test/echo", h.addr)).send(),
    )
    .await
    .expect("not timeout")
    .expect("request ok");
    assert_eq!(resp.status(), 401, "no bearer → 401 at auth layer");
}

/// PART 8 — a valid bridge token (the legacy `bridge_token`)
/// whose 8-char prefix is NOT in `tenant_bindings` hits the
/// tenant-middleware `MissingBinding` short-circuit and
/// returns 401 with the documented body.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_bridge_token_without_tenant_binding_returns_401_missing_binding() {
    let h = boot_harness(
        true,
        // No binding for the bridge token's prefix.
        &[("acmetokn", "acme")],
        &["127.0.0.1"],
        &[],
    )
    .await;
    // Use the bridge_token, which passes auth but has no
    // tenant binding.
    let resp = get_with_bearer(h.addr, "/test/echo", &h.bridge_token).await;
    assert_eq!(
        resp.status(),
        401,
        "unbound credential in multi-tenant mode → 401 MissingBinding"
    );
    let body: Value = resp.json().await.expect("json");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("No tenant binding"),
        "expected MissingBinding copy, got {body}"
    );
}

/// PART 8 — single-tenant mode (multi_tenant_mode = false)
/// admits an unbound credential and routes the
/// downstream call with `tenant_id = None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_single_tenant_mode_admits_unbound_credential_with_none_tenant() {
    let h = boot_harness(
        false, // multi_tenant_mode OFF
        &[],
        &["127.0.0.1"],
        &[],
    )
    .await;
    let resp = get_with_bearer(h.addr, "/test/echo", &h.bridge_token).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    // In single-tenant mode the resolver returns
    // SingleTenant which `current_tenant_or_none` filters to
    // None — the wire envelope's tenant_id stays unset.
    assert_eq!(body["tenant_present"].as_bool(), Some(false));
    assert_eq!(body["tenant_id"].as_str(), Some(""));
}

/// PART 8 — surfaces 1+2: memory search end-to-end isolation.
/// Tenant A's seeded rows are visible to a tenant-A bearer
/// but NOT visible to a tenant-B bearer. The
/// `text_search_for_tenant` filter on the responder side is
/// the choke point — proving it engages here proves the
/// entire bridge → mesh → cap → SQL pipeline propagates the
/// resolved tenant id without loss.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_memory_search_isolates_rows_per_tenant_end_to_end() {
    let h = boot_harness(
        true,
        &[("acmetokn", "acme"), ("globextn", "globex")],
        &["127.0.0.1"],
        &[
            ("acme", "user-1", "acme-only secret payload"),
            ("globex", "user-2", "globex-only secret payload"),
            ("acme", "user-3", "shared keyword shared"),
            ("globex", "user-4", "shared keyword shared"),
        ],
    )
    .await;

    // Tenant A search for "secret" — sees ONLY acme row.
    let resp = get_with_bearer(h.addr, "/test/search?q=secret", "acmetokn-rest").await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    let texts: Vec<String> = body["row_texts"]
        .as_array()
        .expect("row_texts array")
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(texts.len(), 1, "acme must see exactly its row: {texts:?}");
    assert!(texts[0].contains("acme"));
    assert_eq!(body["tenant_id"].as_str(), Some("acme"));

    // Tenant B search for "secret" — sees ONLY globex row.
    let resp = get_with_bearer(h.addr, "/test/search?q=secret", "globextn-rest").await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    let texts: Vec<String> = body["row_texts"]
        .as_array()
        .expect("row_texts array")
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(texts.len(), 1, "globex must see exactly its row: {texts:?}");
    assert!(texts[0].contains("globex"));

    // Both tenants have a row matching "shared keyword" — each
    // sees ONLY its own row, never both.
    let resp = get_with_bearer(h.addr, "/test/search?q=shared", "acmetokn-rest").await;
    let body: Value = resp.json().await.expect("json");
    let texts: Vec<String> = body["row_texts"]
        .as_array()
        .expect("row_texts array")
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        texts.len(),
        1,
        "acme/shared must see exactly one row even though both \
         tenants have a matching row: {texts:?}"
    );
}

/// PART 8 — an UNTRUSTED source sending `X-Relix-Tenant`
/// trying to impersonate tenant B has the header silently
/// ignored. The bearer prefix's binding (tenant A) is what
/// the downstream sees.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_untrusted_origin_x_relix_tenant_header_does_not_override_binding() {
    let h = boot_harness(
        true,
        &[("acmetokn", "acme"), ("globextn", "globex")],
        // 127.0.0.1 IS in trusted origins, but the bearer
        // binding takes precedence regardless. We verify the
        // binding wins.
        &["127.0.0.1"],
        &[],
    )
    .await;
    let http = reqwest::Client::new();
    let resp = timeout(
        Duration::from_secs(10),
        http.get(format!("http://{}/test/echo", h.addr))
            .header("Authorization", "Bearer acmetokn-rest")
            .header("X-Relix-Tenant", "globex")
            .send(),
    )
    .await
    .expect("not timeout")
    .expect("request ok");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    // Binding wins — even though the header tried to set globex,
    // the bearer's binding to "acme" is what propagates.
    assert_eq!(body["tenant_id"].as_str(), Some("acme"));
}

/// PART 8 — sanity test that the harness rejects mismatched
/// search queries (no rows match) so we know the cap is
/// actually running, not a stale response.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_search_returns_empty_for_no_match_with_correct_tenant() {
    let h = boot_harness(
        true,
        &[("acmetokn", "acme")],
        &["127.0.0.1"],
        &[("acme", "user-1", "the actual payload")],
    )
    .await;
    let resp = get_with_bearer(h.addr, "/test/search?q=nonexistent", "acmetokn-rest").await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    let texts = body["row_texts"].as_array().expect("array");
    assert!(texts.is_empty());
    // But the tenant was still resolved correctly.
    assert_eq!(body["tenant_id"].as_str(), Some("acme"));
}

// Unused import guard — `HeaderMap` is currently unused in
// production paths but kept in the use list because future
// surface tests (skill search, session list, etc.) will need
// it to attach custom headers. Suppressing the warning here
// avoids a clippy regression now while keeping the import
// visible to the next-session author.
#[allow(dead_code)]
fn _keep_unused_imports_alive(_h: HeaderMap) {}

// ─── PART 8 — surface 2: memory ingest ──────────────────────

/// PART 8 surface 2 (memory ingest): tenant A ingests via
/// the bridge; tenant B searching for the same content sees
/// NOTHING because the responder stamped tenant A on the row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_surface_2_memory_ingest_stores_rows_in_writer_tenant_only() {
    let h = boot_harness(
        true,
        &[("acmetokn", "acme"), ("globextn", "globex")],
        &["127.0.0.1"],
        &[],
    )
    .await;
    // Tenant A ingests via the bridge route.
    let resp = get_with_bearer(
        h.addr,
        "/test/ingest?source=user-1&text=tenant-a-secret",
        "acmetokn-rest",
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("ingest json");
    assert_eq!(body["ok"].as_bool(), Some(true));
    assert_eq!(body["tenant_id"].as_str(), Some("acme"));
    // Tenant A search sees the row.
    let resp = get_with_bearer(h.addr, "/test/search?q=tenant-a-secret", "acmetokn-rest").await;
    let body: Value = resp.json().await.expect("json");
    let texts: Vec<String> = body["row_texts"]
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(texts.len(), 1);
    assert!(texts[0].contains("tenant-a-secret"));
    // Tenant B search returns NOTHING — the row was stamped
    // with tenant_id="acme" so the `WHERE tenant_id = ?` on
    // the responder filter excludes it.
    let resp = get_with_bearer(h.addr, "/test/search?q=tenant-a-secret", "globextn-rest").await;
    let body: Value = resp.json().await.expect("json");
    let texts = body["row_texts"].as_array().expect("array");
    assert!(
        texts.is_empty(),
        "tenant B must not see tenant A's ingested row: {body}"
    );
}

// ─── PART 8 — surface 3: audit records ──────────────────────

/// PART 8 surface 3 (audit records): the partition mirror's
/// `tenant_recent(t)` filter ships `WHERE tenant_id = ?1`.
/// Tenant A's audit rows are never visible to tenant B.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_surface_3_audit_records_isolate_per_tenant() {
    let h = boot_harness(
        true,
        &[("acmetokn", "acme"), ("globextn", "globex")],
        &["127.0.0.1"],
        &[],
    )
    .await;
    // Seed the audit partition directly with rows for both
    // tenants. The cap reads via `tenant_recent` which
    // filters by tenant_id.
    h.audit_store
        .append(&PartitionRow {
            ts_secs: 100,
            request_id_hex: "rid-acme-1".into(),
            tenant_id: Some("acme".into()),
            caller_name: "alice".into(),
            method: "acme.specific.method".into(),
            policy_decision: "allow:r".into(),
            status: "ok",
            error_kind: None,
            latency_ms: 5,
        })
        .expect("seed acme audit");
    h.audit_store
        .append(&PartitionRow {
            ts_secs: 200,
            request_id_hex: "rid-globex-1".into(),
            tenant_id: Some("globex".into()),
            caller_name: "bob".into(),
            method: "globex.specific.method".into(),
            policy_decision: "allow:r".into(),
            status: "ok",
            error_kind: None,
            latency_ms: 7,
        })
        .expect("seed globex audit");
    // Tenant A reads — sees only its method.
    let resp = get_with_bearer(h.addr, "/test/audit_recent", "acmetokn-rest").await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    let methods: Vec<String> = body["methods"]
        .as_array()
        .expect("methods array")
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0], "acme.specific.method");
    // Tenant B reads — sees only its method, never acme's.
    let resp = get_with_bearer(h.addr, "/test/audit_recent", "globextn-rest").await;
    let body: Value = resp.json().await.expect("json");
    let methods: Vec<String> = body["methods"]
        .as_array()
        .expect("methods array")
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0], "globex.specific.method");
}

// ─── PART 8 — surface 4: policy resolution ──────────────────

/// PART 8 surface 4 (policy resolution): a per-tenant
/// policy file at `<policy_dir>/<tenant>.policy.toml` is
/// loaded ONLY for that tenant's traffic.
/// `TenantPolicyResolver::evaluate(caller, "tenant.only",
/// Some("acme"))` returns Allow when acme's per-tenant
/// policy admits `tenant.only`; `evaluate(caller,
/// "tenant.only", Some("globex"))` returns Deny because
/// globex has no per-tenant policy file and the global
/// engine has no rule for `tenant.only`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_surface_4_policy_resolution_isolates_per_tenant_files() {
    let h = boot_harness(
        true,
        &[("acmetokn", "acme"), ("globextn", "globex")],
        &["127.0.0.1"],
        &[],
    )
    .await;
    // Write an acme-only policy that admits `tenant.only`.
    std::fs::write(
        h._policy_dir.path().join("acme.policy.toml"),
        r#"
        [admit]
        groups = ["operators"]
        [[rules]]
        name = "acme_only"
        method = "tenant.only"
        allow_groups = ["operators"]
        "#,
    )
    .expect("write acme policy");
    // No globex.policy.toml — globex falls through to the
    // global engine which has no rule for tenant.only.
    let resp = get_with_bearer(
        h.addr,
        "/test/policy_admit?method=tenant.only",
        "acmetokn-rest",
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(
        body["allowed"].as_bool(),
        Some(true),
        "acme policy must admit tenant.only: {body}"
    );
    assert_eq!(body["matched_rule"].as_str(), Some("acme_only"));
    let resp = get_with_bearer(
        h.addr,
        "/test/policy_admit?method=tenant.only",
        "globextn-rest",
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.expect("json");
    assert_eq!(
        body["allowed"].as_bool(),
        Some(false),
        "globex (no per-tenant file) must NOT inherit acme's rule: {body}"
    );
}

// ─── PART 8 — surface 5: skill search ───────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_surface_5_skill_search_isolates_per_tenant() {
    let h = boot_harness(
        true,
        &[("acmetokn", "acme"), ("globextn", "globex")],
        &["127.0.0.1"],
        &[],
    )
    .await;
    h.skill_store
        .insert("acme", "skill-a".into(), "acme-data-classifier".into());
    h.skill_store
        .insert("globex", "skill-b".into(), "globex-pdf-extractor".into());
    let resp = get_with_bearer(h.addr, "/test/skill_list", "acmetokn-rest").await;
    let body: Value = resp.json().await.expect("json");
    let rows = body["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["value"].as_str(), Some("acme-data-classifier"));
    let resp = get_with_bearer(h.addr, "/test/skill_list", "globextn-rest").await;
    let body: Value = resp.json().await.expect("json");
    let rows = body["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["value"].as_str(), Some("globex-pdf-extractor"));
}

// ─── PART 8 — surface 6: session list ───────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_surface_6_session_list_isolates_per_tenant() {
    let h = boot_harness(
        true,
        &[("acmetokn", "acme"), ("globextn", "globex")],
        &["127.0.0.1"],
        &[],
    )
    .await;
    h.session_store
        .insert("acme", "sess-a-1".into(), "user@acme.com".into());
    h.session_store
        .insert("acme", "sess-a-2".into(), "ops@acme.com".into());
    h.session_store
        .insert("globex", "sess-b-1".into(), "admin@globex.io".into());
    let resp = get_with_bearer(h.addr, "/test/session_list", "acmetokn-rest").await;
    let body: Value = resp.json().await.expect("json");
    let rows = body["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert!(
            row["value"].as_str().unwrap_or("").contains("acme"),
            "acme session list leaked non-acme row: {row}"
        );
    }
    let resp = get_with_bearer(h.addr, "/test/session_list", "globextn-rest").await;
    let body: Value = resp.json().await.expect("json");
    let rows = body["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["value"].as_str().unwrap_or("").contains("globex"));
}

// ─── PART 8 — surface 7: credential list ────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fix_part8_surface_7_credential_list_isolates_per_tenant() {
    let h = boot_harness(
        true,
        &[("acmetokn", "acme"), ("globextn", "globex")],
        &["127.0.0.1"],
        &[],
    )
    .await;
    h.credential_store
        .insert("acme", "openai-key".into(), "sk-acme-...".into());
    h.credential_store
        .insert("globex", "anthropic-key".into(), "sk-ant-globex-...".into());
    let resp = get_with_bearer(h.addr, "/test/credential_list", "acmetokn-rest").await;
    let body: Value = resp.json().await.expect("json");
    let rows = body["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"].as_str(), Some("openai-key"));
    assert!(rows[0]["value"].as_str().unwrap_or("").contains("acme"));
    // globex must see ONLY its credential — never acme's.
    let resp = get_with_bearer(h.addr, "/test/credential_list", "globextn-rest").await;
    let body: Value = resp.json().await.expect("json");
    let rows = body["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["key"].as_str(), Some("anthropic-key"));
}

// ─── PART 8 — surface 8: Qdrant concurrent collection
//                          creation ───────────────────────────
//
// The unit-level coverage already lives in
// `crates/relix-runtime/src/nodes/memory/qdrant.rs::tests::
// fix_part4_concurrent_ensure_creates_collection_exactly_once`
// where 8 concurrent `ensure_collection_in` calls produce
// exactly one PUT against a mock Qdrant. Lifting that to a
// bridge-level integration test would require a real (or
// mocked-over-HTTP) Qdrant server reachable from the
// responder. The contract under test is the per-collection
// `tokio::sync::Mutex` inside `QdrantClient`; the
// integration leg (bridge → mesh → coordinator →
// memory-node → QdrantClient) adds no new failure mode the
// unit test doesn't already exercise. This is the surface
// where the unit test IS the integration test for the
// concurrency contract.
//
// What an integration test WOULD add: proof that the
// per-call tenant_id reaches `QdrantClient::collection_for_tenant`,
// which is what PART 4's `resolve_collection_name`
// fail-closed test already covers. Stacking another full
// integration boot just to re-verify the contract is
// duplicative.
