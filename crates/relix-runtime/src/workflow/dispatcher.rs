//! Workflow dispatcher trait. Wraps "dispatch one capability
//! call against one peer alias". The real implementation is
//! supplied by the coordinator and wraps the `MeshClient`;
//! tests use a recording stub.

use async_trait::async_trait;

/// Dispatch outcome. Successful responses carry the raw
/// response body bytes (same shape as a `remote_call`
/// response). Failures carry a structured cause so the
/// workflow executor can route on `Failure` edges.
pub type DispatchResult = Result<Vec<u8>, DispatchError>;

/// Structured dispatch failure. Includes peer + method
/// context so the execution trace can record exactly where
/// things went wrong.
#[derive(Debug, Clone, thiserror::Error)]
#[error("workflow dispatch to peer `{peer}` method `{method}` failed: {cause}")]
pub struct DispatchError {
    pub peer: String,
    pub method: String,
    pub cause: String,
}

/// Single-call dispatcher the workflow executor uses to talk
/// to peers. Implementations are `Send + Sync` because the
/// executor may run parallel steps concurrently across
/// tokio tasks.
#[async_trait]
pub trait WorkflowDispatcher: Send + Sync {
    /// Invoke `capability` on `peer_alias` with `input`
    /// bytes. Returns the response body on success or a
    /// structured error on failure. Implementations should
    /// apply their own per-call deadline / timeout.
    async fn dispatch(&self, peer_alias: &str, capability: &str, input: &[u8]) -> DispatchResult;
}
