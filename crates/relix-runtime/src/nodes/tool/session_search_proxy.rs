//! `tool.* memory.session_search` proxy — lets agents
//! running on the tool node search their own chat-turn
//! history without learning the memory peer's alias.
//!
//! The trait keeps the tool node decoupled from the
//! `MeshClient`: tests inject a stub, production wires a real
//! outbound dispatcher pointing at the memory peer.

use std::sync::Arc;

use async_trait::async_trait;

use crate::dispatch::{HandlerOutcome, InvocationCtx, build_request, decode_response};
use crate::manifest::MeshClient;
use crate::transport::envelope::ResponseResult;
use relix_core::bundle::Bundle;
use relix_core::capability::{
    CapabilityDescriptor, CapabilityKind, CostClass, Idempotency, RiskLevel,
};
use relix_core::types::{ErrorEnvelope, error_kinds};

/// Outbound proxy to `memory.session_search`.
///
/// Wire args pass through verbatim
/// (`subject_id|query|limit`); responder JSON body returns
/// verbatim. Tests inject a stub. Production
/// [`MemorySessionSearchMeshProxy`] dials the configured
/// memory peer.
#[async_trait]
pub trait MemorySessionSearchProxy: Send + Sync {
    async fn call(&self, args: &str) -> Result<String, String>;
}

/// `Arc<OnceCell<...>>` so the controller populates the proxy
/// post-startup once `[tool.memory_peer]` is configured. The
/// tool handler reads the cell on every call.
pub type MemorySessionSearchProxyHandle =
    Arc<tokio::sync::OnceCell<Arc<dyn MemorySessionSearchProxy>>>;

/// Live mesh-backed proxy. Dials the configured memory peer.
#[derive(Clone)]
pub struct MemorySessionSearchMeshProxy {
    mesh: MeshClient,
    alias: String,
    identity: Bundle,
    deadline_secs: i64,
}

impl MemorySessionSearchMeshProxy {
    pub fn new(mesh: MeshClient, alias: String, identity: Bundle, deadline_secs: i64) -> Self {
        Self {
            mesh,
            alias,
            identity,
            deadline_secs,
        }
    }
}

#[async_trait]
impl MemorySessionSearchProxy for MemorySessionSearchMeshProxy {
    async fn call(&self, args: &str) -> Result<String, String> {
        let envelope = build_request(
            "memory.session_search",
            args.as_bytes().to_vec(),
            self.identity.clone(),
            self.deadline_secs,
        );
        let resp_bytes = self
            .mesh
            .call(&self.alias, envelope)
            .await
            .map_err(|e| format!("memory transport: {e}"))?;
        let resp = decode_response(&resp_bytes).map_err(|e| format!("decode resp: {e}"))?;
        match resp.res {
            ResponseResult::Ok(body) => {
                String::from_utf8(body.to_vec()).map_err(|e| format!("body utf8: {e}"))
            }
            ResponseResult::Err(env) => Err(format!(
                "memory.session_search err kind={} cause={}",
                env.kind, env.cause
            )),
            ResponseResult::StreamHandle(_) => Err("unexpected stream response".to_string()),
        }
    }
}

/// Capability descriptor for `memory.session_search` published
/// from the tool node. Always advertised (discoverable in the
/// manifest); the handler returns `PEER_UNREACHABLE` when the
/// proxy cell is empty.
pub fn descriptor() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("memory.session_search");
    d.major_version = 1;
    d.description = Some(
        "Search the chronicle for chat turns matching a query string. \
         Wire args: subject_id|query|limit. Returns a JSON array of \
         {session_id, role, content, timestamp_unix, snippet, score}. \
         Read-only; safe for agents to call during task execution."
            .to_string(),
    );
    d.kind = CapabilityKind::Unary;
    d.risk_level = RiskLevel::Safe;
    d.cost_class = CostClass::Cheap;
    d.idempotency = Idempotency::Idempotent;
    d.categories = vec!["memory".into(), "search".into(), "read".into()];
    d.sensitivity_tags = vec!["read-only".into()];
    d
}

/// Handler invoked when an agent calls `memory.session_search`
/// on the tool node. Routes through the configured proxy or
/// returns `PEER_UNREACHABLE` when none is wired.
pub async fn handle(
    proxy_cell: &tokio::sync::OnceCell<Arc<dyn MemorySessionSearchProxy>>,
    ctx: &InvocationCtx,
) -> HandlerOutcome {
    let args = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.to_string(),
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("memory.session_search arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    let Some(proxy) = proxy_cell.get() else {
        return HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::PEER_UNREACHABLE,
            cause: "tool.memory_peer not configured; \
                    set [tool.memory_peer] in the tool node config to enable \
                    memory.session_search proxying"
                .into(),
            retry_hint: 2,
            retry_after: None,
        });
    };
    match proxy.call(&args).await {
        Ok(body) => HandlerOutcome::Ok(body.into_bytes()),
        Err(cause) => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::TRANSPORT,
            cause: format!("memory.session_search: {cause}"),
            retry_hint: 1,
            retry_after: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubProxy {
        last_args: std::sync::Mutex<Option<String>>,
        canned: String,
        err: Option<String>,
    }

    #[async_trait]
    impl MemorySessionSearchProxy for StubProxy {
        async fn call(&self, args: &str) -> Result<String, String> {
            *self.last_args.lock().unwrap() = Some(args.to_string());
            if let Some(e) = &self.err {
                Err(e.clone())
            } else {
                Ok(self.canned.clone())
            }
        }
    }

    fn ctx(args: &[u8]) -> InvocationCtx {
        InvocationCtx {
            caller: relix_core::identity::VerifiedIdentity {
                subject_id: relix_core::types::NodeId::from_pubkey(b"x"),
                name: "x".into(),
                org_id: relix_core::types::NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: relix_core::types::TraceId::new(),
            request_id: relix_core::types::RequestId::new(),
            args: args.to_vec(),
            tenant_id: None,
        }
    }

    #[tokio::test]
    async fn proxy_call_passes_args_verbatim_and_returns_body() {
        let cell: tokio::sync::OnceCell<Arc<dyn MemorySessionSearchProxy>> =
            tokio::sync::OnceCell::new();
        let stub = Arc::new(StubProxy {
            last_args: std::sync::Mutex::new(None),
            canned: r#"[{"session_id":"sess-A","role":"user","content":"hi"}]"#.into(),
            err: None,
        });
        cell.set(stub.clone() as Arc<dyn MemorySessionSearchProxy>)
            .ok();
        let outcome = handle(&cell, &ctx(b"alice|find|7")).await;
        match outcome {
            HandlerOutcome::Ok(body) => {
                let s = String::from_utf8(body).unwrap();
                assert!(s.contains("sess-A"));
            }
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        }
        assert_eq!(
            stub.last_args.lock().unwrap().clone().unwrap(),
            "alice|find|7"
        );
    }

    #[tokio::test]
    async fn empty_proxy_cell_surfaces_peer_unreachable() {
        let cell: tokio::sync::OnceCell<Arc<dyn MemorySessionSearchProxy>> =
            tokio::sync::OnceCell::new();
        let outcome = handle(&cell, &ctx(b"|q|20")).await;
        match outcome {
            HandlerOutcome::Err(e) => {
                assert_eq!(e.kind, error_kinds::PEER_UNREACHABLE);
                assert!(e.cause.contains("[tool.memory_peer]"));
            }
            HandlerOutcome::Ok(_) => panic!("expected Err"),
        }
    }

    #[tokio::test]
    async fn proxy_error_surfaces_transport_kind() {
        let cell: tokio::sync::OnceCell<Arc<dyn MemorySessionSearchProxy>> =
            tokio::sync::OnceCell::new();
        let stub: Arc<dyn MemorySessionSearchProxy> = Arc::new(StubProxy {
            last_args: std::sync::Mutex::new(None),
            canned: String::new(),
            err: Some("simulated drop".into()),
        });
        cell.set(stub).ok();
        let outcome = handle(&cell, &ctx(b"|q|20")).await;
        match outcome {
            HandlerOutcome::Err(e) => {
                assert_eq!(e.kind, error_kinds::TRANSPORT);
                assert!(e.cause.contains("simulated drop"));
            }
            HandlerOutcome::Ok(_) => panic!("expected Err"),
        }
    }

    #[test]
    fn descriptor_advertises_safe_idempotent_read() {
        let d = descriptor();
        assert_eq!(d.method_name, "memory.session_search");
        assert!(matches!(d.kind, CapabilityKind::Unary));
        assert!(matches!(d.risk_level, RiskLevel::Safe));
        assert!(matches!(d.idempotency, Idempotency::Idempotent));
        assert!(d.categories.iter().any(|c| c == "memory"));
        assert!(d.categories.iter().any(|c| c == "search"));
    }
}
