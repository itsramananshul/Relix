//! Production [`WorkflowDispatcher`] backed by a
//! [`MeshClient`]. Each call resolves the peer alias from
//! the configured peers map, builds a request envelope with
//! the workflow's identity, hands it to the mesh, and decodes
//! the response into raw body bytes or a structured
//! [`DispatchError`].

use async_trait::async_trait;
use std::time::Duration;

use relix_core::bundle::Bundle;

use super::dispatcher::{DispatchError, DispatchResult, WorkflowDispatcher};
use crate::dispatch::{build_request, decode_response};
use crate::manifest::MeshClient;
use crate::transport::envelope::ResponseResult;

/// OnceCell-style holder for the workflow dispatcher. The
/// coordinator registers `workflow.*` capabilities at
/// startup BEFORE the mesh client + peer ids are fully wired
/// (peer discovery happens after the bridge boots), so the
/// dispatcher is plumbed in later via this cell. An empty
/// cell means workflow.run returns a clear "mesh not ready"
/// error rather than a panic.
pub type WorkflowDispatcherCell =
    std::sync::Arc<tokio::sync::OnceCell<std::sync::Arc<dyn WorkflowDispatcher>>>;

/// Dispatcher that talks to peers via [`MeshClient::call`].
pub struct MeshWorkflowDispatcher {
    mesh: MeshClient,
    identity: Bundle,
    deadline_secs: i64,
    network_timeout: Duration,
}

impl MeshWorkflowDispatcher {
    /// Construct a mesh-backed dispatcher.
    ///
    /// `deadline_secs` is stamped on every outbound envelope
    /// and `network_timeout` is the local wall-clock cap on
    /// each call (deadline + 5s by default so the responder
    /// has time to surface its own deadline error before our
    /// timeout fires).
    pub fn new(mesh: MeshClient, identity: Bundle, deadline_secs: i64) -> Self {
        let network_timeout = Duration::from_secs((deadline_secs + 5).max(10) as u64);
        Self {
            mesh,
            identity,
            deadline_secs,
            network_timeout,
        }
    }
}

#[async_trait]
impl WorkflowDispatcher for MeshWorkflowDispatcher {
    async fn dispatch(&self, peer_alias: &str, capability: &str, input: &[u8]) -> DispatchResult {
        let envelope = build_request(
            capability,
            input.to_vec(),
            self.identity.clone(),
            self.deadline_secs,
        );
        let resp_bytes =
            match tokio::time::timeout(self.network_timeout, self.mesh.call(peer_alias, envelope))
                .await
            {
                Ok(Ok(b)) => b,
                Ok(Err(e)) => {
                    return Err(DispatchError {
                        peer: peer_alias.to_string(),
                        method: capability.to_string(),
                        cause: format!("mesh transport: {e}"),
                    });
                }
                Err(_elapsed) => {
                    return Err(DispatchError {
                        peer: peer_alias.to_string(),
                        method: capability.to_string(),
                        cause: format!(
                            "outbound call exceeded {} second wall-clock timeout",
                            self.network_timeout.as_secs()
                        ),
                    });
                }
            };
        let resp = decode_response(&resp_bytes).map_err(|e| DispatchError {
            peer: peer_alias.to_string(),
            method: capability.to_string(),
            cause: format!("response decode: {e}"),
        })?;
        match resp.res {
            ResponseResult::Ok(body) => Ok(body.to_vec()),
            ResponseResult::Err(env) => Err(DispatchError {
                peer: peer_alias.to_string(),
                method: capability.to_string(),
                cause: format!("responder error ({}): {}", env.kind, env.cause),
            }),
            ResponseResult::StreamHandle(_) => Err(DispatchError {
                peer: peer_alias.to_string(),
                method: capability.to_string(),
                cause: "streaming responses are not supported by the workflow dispatcher"
                    .to_string(),
            }),
        }
    }
}
