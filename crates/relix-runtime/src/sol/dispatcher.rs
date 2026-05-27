//! Host-side `remote_call` dispatcher trait — Relix-specific extension to the
//! ported OpenPrem SOL runtime.
//!
//! The VM holds an optional `Arc<dyn RemoteCallDispatcher + Send + Sync>` and
//! invokes it when executing `Inst::RemoteCall`. Implementations live outside
//! `crate::sol`; the canonical production impl is in
//! `crate::controller_runtime::flow_runner` and wraps the libp2p
//! `transport::rpc::Client` plus the caller's identity bundle.
//!
//! **SIMP-014:** the trait method is synchronous. The production impl bridges
//! to async libp2p via `tokio::task::block_in_place +
//! Handle::current().block_on(...)` so the calling VM thread blocks without
//! poisoning the multi-threaded tokio runtime. Documented in
//! `specs/alpha-simplifications.md`.

use std::fmt;

/// Outcome a dispatcher returns. The wire-level RELIX-1 response envelope's
/// `Ok(body)` becomes `Ok(body bytes)`; any error becomes `Err(RemoteCallError)`.
pub type RemoteCallResult = Result<Vec<u8>, RemoteCallError>;

/// Host-side dispatcher invoked by `Inst::RemoteCall`. Implementations must be
/// `Send + Sync` because the dispatcher lives on the VM, which may be moved
/// across tokio tasks.
pub trait RemoteCallDispatcher: Send + Sync {
    /// Invoke a capability on a remote peer.
    ///
    /// - `peer_alias`: name from the controller config's `[peers]` table
    ///   (e.g. `"memory"` resolves to a configured TCP multiaddr).
    /// - `method`: fully-qualified capability method (`"memory.search"`).
    /// - `arg`: opaque bytes — SIMP-016 ships the UTF-8 of the SOL-side
    ///   string argument. Typed CBOR args land at Gate 2.
    ///
    /// Returns the response body bytes on success, or a structured error
    /// the VM surfaces via `last_error()`.
    fn remote_call(&self, peer_alias: &str, method: &str, arg: &[u8]) -> RemoteCallResult;

    /// RELIX-2 step 4: streaming variant. Dispatches a
    /// `/relix/rpc/stream/1` substream call against `peer_alias`
    /// + `method` with `arg` as the request envelope payload.
    /// The streaming substream returns a sequence of Chunk
    /// frames; this method collects them all into a single
    /// `Vec<u8>` (concatenated bytes, in arrival order) AND
    /// invokes `on_chunk` for each chunk as it arrives.
    ///
    /// The return-value contract matches `remote_call` (final
    /// concatenated body or structured error) so the SOL VM's
    /// `Inst::RemoteCallStream` opcode produces a single
    /// heap-string ref — SOL flows stay synchronous from the
    /// author's perspective. The `on_chunk` callback exists
    /// for external observers (the web bridge's SSE response)
    /// that want to ship tokens to the HTTP client as they
    /// arrive, before the VM has finished collecting.
    ///
    /// Default impl falls back to [`Self::remote_call`] and
    /// reports the whole body as a single chunk. Streaming-
    /// capable dispatchers (e.g. the controller-runtime's
    /// `RealDispatcher` once step 5 wires libp2p streaming)
    /// override this to drive the substream protocol
    /// directly.
    fn remote_call_stream(
        &self,
        peer_alias: &str,
        method: &str,
        arg: &[u8],
        on_chunk: &dyn Fn(&[u8]),
    ) -> RemoteCallResult {
        let body = self.remote_call(peer_alias, method, arg)?;
        on_chunk(&body);
        Ok(body)
    }
}

/// Structured error returned by a dispatcher. Carries enough for the VM and
/// the per-flow event log to record what happened.
#[derive(Clone, Debug)]
pub struct RemoteCallError {
    /// Stable error-kind (mirrors `relix_core::types::error_kinds::*` where
    /// applicable; `0` = `local_dispatch_error` for dispatcher-side issues).
    pub kind: u32,
    /// Peer alias the call targeted (empty if resolution failed).
    pub peer: String,
    /// Method that was attempted.
    pub method: String,
    /// Human-readable cause.
    pub cause: String,
}

impl RemoteCallError {
    /// Convenience constructor for dispatcher-local failures (no peer reached).
    pub fn local(
        peer: impl Into<String>,
        method: impl Into<String>,
        cause: impl Into<String>,
    ) -> Self {
        Self {
            kind: 0,
            peer: peer.into(),
            method: method.into(),
            cause: cause.into(),
        }
    }

    /// Construct from a `relix_core::types::ErrorEnvelope` returned by the responder.
    pub fn from_envelope(
        peer: impl Into<String>,
        method: impl Into<String>,
        env: &relix_core::types::ErrorEnvelope,
    ) -> Self {
        Self {
            kind: env.kind,
            peer: peer.into(),
            method: method.into(),
            cause: env.cause.clone(),
        }
    }
}

impl fmt::Display for RemoteCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "remote_call({}, {}): kind={} cause={}",
            self.peer, self.method, self.kind, self.cause
        )
    }
}

impl std::error::Error for RemoteCallError {}
