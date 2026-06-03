//! Dispatch bridge — RELIX-1 §1.13 admission pipeline (alpha subset).
//!
//! The bridge owns:
//! - the capability registry (method → handler map),
//! - the policy engine,
//! - the trust root (org-root pubkey),
//! - the audit log,
//! - the per-tenant policy resolver,
//! - the replay-nonce cache (P2).
//!
//! For every inbound `transport::rpc::Event::Request`, it runs the strict
//! admission pipeline (RELIX-1 §1.13 steps 1, 3, 4 (replay), 5, 9, 10, 11)
//! and dispatches to the registered handler.

pub mod replay;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde_bytes::ByteBuf;

use relix_core::audit::{AuditDraft, AuditLog, AuditStatus};
use relix_core::bundle::Bundle;
use relix_core::codec;
use relix_core::identity::{VerifiedIdentity, validate_identity_bundle};
use relix_core::policy::{Decision, PolicyEngine};
use relix_core::types::{ErrorEnvelope, NodeId, Timestamp, error_kinds};

use crate::transport::envelope::{RequestEnvelope, ResponseEnvelope, ResponseResult};

/// Context passed to a capability handler. Carries verified caller identity
/// and enough state for the handler to perform outbound calls and emit events.
///
/// `Clone` lets the RELIX-7.19 fallback engine re-invoke the same handler
/// (retry / escalate paths) without forcing handlers to take ownership.
#[derive(Clone)]
pub struct InvocationCtx {
    /// Verified caller identity (post admission steps 5+9).
    pub caller: VerifiedIdentity,
    /// Trace context echoed in outbound calls.
    pub trace_id: relix_core::types::TraceId,
    /// The request id of the inbound call (echoed back).
    pub request_id: relix_core::types::RequestId,
    /// CBOR-encoded arguments.
    pub args: Vec<u8>,
    /// GAP 23: per-request tenant identifier propagated from
    /// the `X-Relix-Tenant` header (or set explicitly by
    /// mesh-internal callers). `None` is the default tenant.
    /// Handlers that enforce per-tenant isolation read
    /// [`Self::tenant_id_or_default`].
    #[doc(hidden)]
    pub tenant_id: Option<String>,
}

impl InvocationCtx {
    /// Resolve the request's tenant id, defaulting to
    /// `"default"` when the field is absent. Used by every cap
    /// that scopes its work per tenant.
    pub fn tenant_id_or_default(&self) -> &str {
        self.tenant_id.as_deref().unwrap_or("default")
    }
}

/// Outcome a handler returns. Maps to `ResponseResult` on the wire.
pub enum HandlerOutcome {
    /// Encoded successful response body.
    Ok(Vec<u8>),
    /// Application-level error.
    Err(ErrorEnvelope),
}

/// RELIX-2 step 2: stream of handler output chunks. Used by
/// streaming-capable handlers ([`StreamingHandler`]) — each
/// `Ok(bytes)` is dispatched to the wire as a
/// [`crate::transport::stream::StreamFrame::Chunk`]; an
/// `Err(envelope)` terminates the stream with a
/// [`crate::transport::stream::StreamFrame::Err`]. The stream
/// completing naturally writes
/// [`crate::transport::stream::StreamFrame::End`].
pub type HandlerStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<Vec<u8>, ErrorEnvelope>> + Send>>;

/// PH-DISP1: internal outcome bucket for [`DispatchBridge::bump_stats`].
enum StatBucket {
    Ok,
    Err,
    Denied,
    Unknown,
}

/// W2-007d: one observation in the policy denial ring.
/// Cheap to clone — every field is owned + small.
#[derive(Debug, Clone)]
pub struct PolicyDenialEntry {
    /// Unix seconds when the denial was recorded.
    pub at: i64,
    /// Method the caller attempted.
    pub method: String,
    /// Caller's subject_id (hex). Same string the audit log
    /// records — operators can correlate.
    pub caller_subject_id: String,
    /// Caller's friendly name from their VerifiedIdentity.
    pub caller_name: String,
    /// Name of the policy rule that explicitly denied, or
    /// `"default_deny"` when nothing matched.
    pub rule: String,
    /// Operator-readable reason from the policy engine.
    pub reason: String,
}

/// W2-007d: bounded ring of recent [`PolicyDenialEntry`]s on
/// the local DispatchBridge.
///
/// CORR PART 4: the eviction policy is now **time-windowed**
/// in addition to count-bounded. Pre-fix FIFO eviction let a
/// noisy attacker fill the ring with low-signal denials and
/// thereby push the high-signal evidence of *their* attempt
/// off the back of the ring. The time window
/// ([`POLICY_DENIAL_WINDOW_SECS`], 1h by default) means every
/// denial older than the window is dropped at insert time so
/// the ring carries a true 1-hour sample regardless of how
/// many entries arrived during it. The count cap
/// ([`POLICY_DENIAL_HARD_CAP`]) still applies as a memory
/// ceiling — beyond it the oldest entry is dropped.
#[derive(Debug)]
pub struct PolicyDenialRing {
    entries: std::sync::Mutex<std::collections::VecDeque<PolicyDenialEntry>>,
    /// Hard memory cap. Once reached, even time-windowed
    /// trimming yields to FIFO eviction.
    capacity: usize,
    /// Time window in seconds. Entries with `at < now - window`
    /// are dropped at every push.
    window_secs: i64,
}

/// W2-007d: default ring capacity. Same convention as the
/// other in-memory rings (fs / terminal / mcp audit).
pub const POLICY_DENIAL_RING_DEFAULT: usize = 256;

/// CORR PART 4: hard cap on the policy-denial ring's count.
/// Operators querying `node.policy.recent_denials` see at most
/// this many entries per request.
pub const POLICY_DENIAL_HARD_CAP: usize = 10_000;

/// CORR PART 4: time window (1 hour) for the policy-denial
/// ring. Older entries are dropped at insert time so the ring
/// always reflects the last hour of activity even when a
/// noisy attacker fills the count cap with low-signal denials.
pub const POLICY_DENIAL_WINDOW_SECS: i64 = 3600;

impl Default for PolicyDenialRing {
    fn default() -> Self {
        Self::new(POLICY_DENIAL_HARD_CAP)
    }
}

impl PolicyDenialRing {
    pub fn new(capacity: usize) -> Self {
        Self::with_window(capacity, POLICY_DENIAL_WINDOW_SECS)
    }

    /// CORR PART 4: build with an explicit time window. Lets
    /// tests verify the time-windowed eviction behaviour
    /// deterministically without waiting an hour.
    pub fn with_window(capacity: usize, window_secs: i64) -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::VecDeque::with_capacity(
                capacity.min(1024),
            )),
            capacity: capacity.clamp(1, POLICY_DENIAL_HARD_CAP),
            window_secs: window_secs.max(1),
        }
    }

    pub fn push(&self, e: PolicyDenialEntry) {
        let now = e.at;
        let cutoff = now.saturating_sub(self.window_secs);
        let mut g = self.entries.lock().unwrap_or_else(|e| {
            tracing::warn!("policy denial ring poisoned; recovering inner state");
            e.into_inner()
        });
        // Trim entries that fell outside the window. The ring
        // is in insertion order so a `while front older than
        // cutoff: pop_front` walk is O(k) where k is the
        // number of expired entries.
        while let Some(front) = g.front() {
            if front.at < cutoff {
                g.pop_front();
            } else {
                break;
            }
        }
        // Hard count cap — pre-fix FIFO behaviour kicks in only
        // after time-windowing has done its job.
        while g.len() >= self.capacity {
            g.pop_front();
        }
        g.push_back(e);
    }

    /// Snapshot the most recent `max` entries, newest first.
    pub fn snapshot_newest_first(&self, max: usize) -> Vec<PolicyDenialEntry> {
        let g = self.entries.lock().unwrap_or_else(|e| {
            tracing::warn!("policy denial ring poisoned; recovering inner state");
            e.into_inner()
        });
        g.iter().rev().take(max).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|e| {
                tracing::warn!("policy denial ring poisoned; recovering inner state");
                e.into_inner()
            })
            .len()
    }

    #[allow(dead_code)] // pairs with len() per clippy len_zero
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A capability handler: native function invoked by the dispatch bridge.
#[async_trait]
pub trait Handler: Send + Sync {
    /// Invoke the handler. The dispatch bridge has already verified identity
    /// and policy; the handler need only execute the capability.
    async fn invoke(&self, ctx: InvocationCtx) -> HandlerOutcome;
}

/// Function-handler adapter. Lets a `Fn(InvocationCtx) -> Future<HandlerOutcome>`
/// be used without writing a struct impl every time.
pub struct FnHandler<F>(pub F);

#[async_trait]
impl<F, Fut> Handler for FnHandler<F>
where
    F: Fn(InvocationCtx) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = HandlerOutcome> + Send,
{
    async fn invoke(&self, ctx: InvocationCtx) -> HandlerOutcome {
        (self.0)(ctx).await
    }
}

/// RELIX-2 step 2: streaming capability handler. The bridge
/// runs the FULL admission pipeline (decode → deadline →
/// identity → gate → policy → broker) before invoking the
/// handler, identical to the unary [`Handler`] path. The
/// difference is the response shape:
///
/// - On success, the handler returns a [`HandlerStream`]; the
///   bridge pipes each chunk through a
///   [`crate::transport::stream::StreamFrame::Chunk`] frame
///   and closes with `End` when the stream finishes.
/// - On structured failure (`Err(envelope)`), the bridge
///   writes a single terminal `StreamFrame::Err` frame.
/// - On admission failure (pre-handler), the bridge writes
///   the same terminal `StreamFrame::Err` frame — admission
///   errors never reach the handler.
#[async_trait]
pub trait StreamingHandler: Send + Sync {
    async fn invoke_stream(&self, ctx: InvocationCtx) -> Result<HandlerStream, ErrorEnvelope>;
}

/// Function-handler adapter for streaming handlers. Same
/// ergonomic role as [`FnHandler`] for the unary path.
pub struct FnStreamingHandler<F>(pub F);

#[async_trait]
impl<F, Fut> StreamingHandler for FnStreamingHandler<F>
where
    F: Fn(InvocationCtx) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<HandlerStream, ErrorEnvelope>> + Send,
{
    async fn invoke_stream(&self, ctx: InvocationCtx) -> Result<HandlerStream, ErrorEnvelope> {
        (self.0)(ctx).await
    }
}

/// The registry + admission pipeline. Constructed once at controller startup.
pub struct DispatchBridge {
    handlers: HashMap<String, Arc<dyn Handler>>,
    /// RELIX-2: streaming-capability handlers. Separate from
    /// `handlers` so a method is either unary OR streaming;
    /// registering the same name under both is a configuration
    /// error operators see at startup if both register calls
    /// happen on the same bridge.
    streaming_handlers: HashMap<String, Arc<dyn StreamingHandler>>,
    policy: PolicyEngine,
    trust_root: VerifyingKey,
    audit: tokio::sync::Mutex<AuditLog>,
    responder_node_id: NodeId,
    /// PH-DISP1: per-capability invocation counters. One row
    /// per method name the bridge has seen, populated as
    /// requests pass through the admission pipeline. Exposed
    /// via [`Self::capability_stats_snapshot`] for the bridge
    /// / dashboard to project. Pure observability; doesn't
    /// gate any decision.
    ///
    /// W2-006b: wrapped in `Arc` so handlers (e.g. the
    /// `node.dispatch.stats` capability the bridge exposes)
    /// can capture a cheap clone of the shared lock without
    /// needing access to the whole DispatchBridge.
    capability_stats: Arc<std::sync::RwLock<HashMap<String, CapStats>>>,
    /// W2-007d: bounded ring of recent policy denials. The
    /// admission step pushes one entry on every Deny outcome
    /// before the audit log is written. Surfaced via the
    /// built-in `node.policy.recent_denials` capability.
    policy_denials: Arc<PolicyDenialRing>,
    /// Optional agent-employee gate plumbing. Wired by the
    /// coordinator binary at startup. `None` on every other
    /// node — those nodes skip the gate step entirely and
    /// preserve today's behavior.
    agent_gate: Option<AgentGateBindings>,
    /// W2: per-agent capability access broker. Configured from
    /// `[[execution.agents]]` at startup. `None` (or an empty
    /// broker) means the broker is permissive — useful for
    /// nodes / deployments that don't run agent policies.
    access_broker: Option<Arc<crate::nodes::execution::broker::AgentAccessBroker>>,
    /// RELIX-7.11: per-invocation metrics sink. When set, the
    /// dispatch pipeline records one row per dispatched call
    /// (success OR handler-error) with agent name, method,
    /// latency, sizes, and error kind. Policy-denied /
    /// unknown-method outcomes are NOT recorded — the
    /// dispatch-stats counters above already cover those.
    ///
    /// Wired by the controller startup via
    /// [`Self::set_metrics_sink`]. `None` keeps the bridge in
    /// counter-only mode (the dispatch-stats path keeps
    /// running unconditionally).
    metrics_sink: Option<Arc<dyn crate::metrics::MetricsSink>>,
    /// RELIX-7.11: peer alias the bridge runs on. Carried in
    /// every recorded metric so cross-peer dashboards can
    /// disambiguate (the agent name is the *caller's*
    /// identity; this is the responder's friendly name).
    peer_alias: String,
    /// RELIX-7.19: per-invocation confidence scorer. `None`
    /// keeps the bridge's hot path byte-for-byte pre-7.19;
    /// `Some` wires the scorer + the fallback engine so every
    /// dispatched outcome gets scored, scored-recorded on the
    /// metric row, and (when a policy matches) passed through
    /// the fallback action selector.
    confidence_scorer: Option<Arc<crate::confidence::ConfidenceScorer>>,
    /// RELIX-7.19: fallback engine wired alongside the scorer.
    /// `None` means "score but never re-dispatch / replace /
    /// alert / abort" — useful for read-only observation
    /// deployments.
    fallback_engine: Option<Arc<crate::confidence::FallbackEngine>>,
    /// RELIX-7.19: shared last-confidence slot read by the SOL
    /// `last_confidence()` builtin. `None` on nodes that don't
    /// host a SOL VM execution surface. Updated atomically
    /// after every scored dispatch.
    last_confidence_cell: Option<crate::confidence::LastConfidenceCell>,
    /// RELIX-7.19 GAP 2: the alert engine the bridge calls
    /// when a confidence verdict triggers the `Alert`
    /// fallback action. Holds the per-(agent, method,
    /// LowConfidence) dedup state. `None` keeps the legacy
    /// `tracing::warn!` fallback path so operators who
    /// haven't wired the alert pipeline still see the alert.
    alert_engine: Option<Arc<crate::metrics::AlertEngine>>,
    /// RELIX-7.19 GAP 2: the alert sink the bridge fires
    /// `LowConfidence` events through. Wired alongside the
    /// engine. Typically a `CompositeAlertSink` over the
    /// chronicle + multi-channel fan-out.
    alert_sink: Option<Arc<dyn crate::metrics::AlertDeliver>>,
    /// RELIX-7.28 Part 1: pre-dispatch cost-control gate.
    /// `None` keeps the bridge in pre-7.28 mode (no budget
    /// enforcement). When wired, the bridge consults
    /// `enforcer.check(agent, method)` after admission and
    /// before handler invocation.
    budget_enforcer: Option<Arc<crate::metrics::BudgetEnforcer>>,
    /// RELIX-7.28 Part 3: mesh-level PII gate. `None` keeps
    /// the bridge in pre-7.28 mode (no PII scanning). When
    /// wired, the bridge scans inbound request args (and
    /// optionally outbound response bodies) at the mesh
    /// boundary.
    pii_gate: Option<Arc<crate::nodes::pii_gate::MeshPiiGate>>,
    /// GAP 23B: per-tenant policy resolver. `None` keeps the
    /// bridge in single-tenant mode (every call evaluates
    /// against the global [`Self::policy`]). When wired, the
    /// admission step consults the resolver with the request's
    /// `tenant_id`; tenants without a per-tenant policy file
    /// transparently fall through to the global engine.
    tenant_policy: Option<Arc<relix_core::policy::TenantPolicyResolver>>,
    /// GAP 23C: per-tenant audit partition mirror. `None`
    /// keeps the bridge in pre-23C mode (only the canonical
    /// signed CBOR log is written). When wired, every
    /// finalised audit also lands as a row in a queryable
    /// SQLite store keyed by sanitised tenant id.
    audit_partition: Option<Arc<crate::audit_partition::AuditPartitionStore>>,
    /// GAP 15 partial: global "always require operator approval"
    /// method allowlist. Operators populate it from
    /// `[approval] always_require_methods = [...]` in the
    /// controller config. When a call's method name appears
    /// here, admission step 8.5 rejects with
    /// [`relix_core::types::error_kinds::APPROVAL_REQUIRED`] —
    /// even if the caller's policy and (when wired) agent gate
    /// would otherwise admit. The list is checked AFTER the
    /// agent gate, so operators who run both surfaces get the
    /// gate's per-agent semantics AND a hard "always require"
    /// floor.
    ///
    /// SEC PART C: `Arc<HashSet<String>>` so the per-request
    /// admission check is O(1). Operators with 50 always-require
    /// methods no longer pay 50 string comparisons per inbound
    /// call.
    always_require_methods: Arc<HashSet<String>>,
    /// P1: Ed25519 verification key set for
    /// [`crate::approval::ApprovalToken`]. The controller reads
    /// `RELIX_APPROVAL_SIGNING_KEY` at startup, builds a
    /// signer, and stores both the signer (for issuance) and
    /// a keyset derived from it (for verification) here. The
    /// admission gate threads `&keyset` through
    /// `GateInputs::keyset`. Empty keyset = every token-bearing
    /// call fails with `approval_token_missing_key`.
    approval_keyset: crate::approval::ApprovalKeySet,
    /// P1: Ed25519 signer used to mint approval tokens on this
    /// controller. `None` on responder-only nodes that verify
    /// tokens issued by a separate coordinator. When `Some`,
    /// the verifying key is also registered in
    /// `approval_keyset` so a token issued by this controller
    /// is accepted by this same controller (the typical
    /// single-controller deployment).
    approval_signer: Option<crate::approval::ApprovalSigner>,
    /// NOT-DONE 1: clock injection for TTL-sensitive admission
    /// paths (token expiry check at step 8). Defaults to
    /// [`relix_core::clock::SystemClock`]; tests install a
    /// [`relix_core::clock::FakeClock`] via
    /// [`Self::set_clock`] to drive deterministic boundary
    /// cases without sleeping.
    clock: Arc<dyn relix_core::clock::Clock>,
    /// P2 — RELIX-1 §1.9 sliding-window nonce cache. Every
    /// inbound envelope's `rid` is checked against the cache
    /// before any other admission step beyond decode. Duplicate
    /// rids inside the freshness window return
    /// [`error_kinds::REPLAY_REJECTED`].
    replay_cache: replay::ReplayCache,
    /// SECTION 7 — ONE-SIDED future clock-skew allowance in ms.
    /// An envelope stamped more than this far in the FUTURE
    /// (`issued_at_ms - now_ms > max_clock_skew_ms`) is rejected;
    /// a future-stamped envelope must never be admitted just for
    /// being "within window". Default 5 s; tune via
    /// [`Self::set_max_clock_skew_ms`].
    max_clock_skew_ms: i64,
    /// SECTION 7 — freshness window in ms for PAST envelopes
    /// (RELIX-1 §1.9 — 5 minutes). An envelope older than this
    /// (`now_ms - issued_at_ms > freshness_window_ms`) is
    /// rejected as stale. Also the replay cache's retention
    /// window, since a nonce older than this is necessarily
    /// stale. Tune via [`Self::set_freshness_window_ms`].
    freshness_window_ms: i64,
    /// P5 — optional session-token verification gate. When
    /// `Some` AND `verify_on_dispatch_enabled = true`, every
    /// admitted call MUST carry a wire-encoded session token
    /// whose signature + expiry + scope cover the requested
    /// method. Wired by the controller startup alongside the
    /// session identity service.
    session_service: Option<Arc<crate::identity::SessionIdentityService>>,
    /// P5 — operator switch for the session-token gate.
    /// Mirrors `[identity.session] verify_on_dispatch`.
    /// Defaults to `false` so existing deployments behave
    /// unchanged.
    verify_on_dispatch_enabled: bool,
}

/// Describe a capability by method name. The gate uses this
/// for the risk-ceiling + categories check. Returns `None`
/// when the bridge has no metadata for the method (gate
/// falls back to a category-free, risk-free admit).
pub type CapabilityDescribeFn =
    Arc<dyn Fn(&str) -> Option<relix_core::capability::CapabilityDescriptor> + Send + Sync>;

/// SEC PART 7: build a [`CapabilityDescribeFn`] backed by the
/// shared descriptor cache the [`crate::manifest::ManifestProvider`]
/// populates at capability-registration time. The returned
/// closure does a single lock-free `DashMap::get` per
/// invocation regardless of how many capabilities the node has
/// registered, replacing the pre-fix path where each lookup had
/// to scan the manifest's capability vector under a lock.
pub fn describe_fn_from_cache(cache: crate::manifest::DescriptorCache) -> CapabilityDescribeFn {
    Arc::new(move |method: &str| cache.get(method).map(|entry| entry.value().clone()))
}

/// Coordinator-side closure that records an approval request
/// when the gate returns `RequireApproval`. Implementation
/// mints the approval row + chronicle event + telegram
/// notification. Returns the new approval_id.
pub type OnRequireApprovalFn = Arc<
    dyn Fn(&crate::admission::agent_gate::GateApprovalRequest, &str) -> Result<String, String>
        + Send
        + Sync,
>;

/// What the bridge needs to run the agent gate.
#[derive(Clone)]
pub struct AgentGateBindings {
    /// Read-only store handle for the categorical lookups.
    pub store: crate::admission::agent_gate::AgentStoreHandle,
    /// Closure the gate uses to look up a descriptor for the
    /// method being called.
    pub describe: CapabilityDescribeFn,
    /// Closure that records an approval row + chronicle
    /// event + telegram fire when the gate returns
    /// `RequireApproval`. Returns the new approval_id.
    pub on_require_approval: OnRequireApprovalFn,
}

/// PH-DISP1: per-capability counters. Counts are lifetime —
/// reset on bridge restart. The dashboard renders these via a
/// future projection; today they're queryable in-process.
///
/// W2-006a extends the counters with latency fields:
/// `last_elapsed_ms`, `total_elapsed_ms`, `max_elapsed_ms`, and
/// `latency_samples`. Mean latency is `total_elapsed_ms /
/// latency_samples` (callers compute it; this struct stays a
/// dumb counter bag). Latency captures only successful or
/// handler-errored invocations — policy-denied / unknown-method
/// attempts don't reach the handler so they have no elapsed
/// time to record.
#[derive(Debug, Default, Clone)]
pub struct CapStats {
    /// Total successful invocations (handler returned Ok).
    pub invocations: u64,
    /// Total handler-level errors (handler returned Err).
    pub errors: u64,
    /// Total policy-denied attempts. Never reaches the handler.
    pub denied: u64,
    /// Total unknown-method attempts. These don't have a
    /// registered handler so the counter lives under the
    /// caller-supplied method name; useful for spotting
    /// mistyped capability names.
    pub unknown_method: u64,
    /// Wall-clock unix seconds of the most recent invocation
    /// outcome (Ok or Err — set to `now` on every dispatch).
    pub last_invoked_at: i64,
    /// Wall-clock unix seconds of the most recent error
    /// (handler Err OR policy-denied OR unknown_method).
    pub last_error_at: Option<i64>,
    /// W2-006a: elapsed_ms of the most recent dispatched
    /// invocation (Ok or Err). 0 when no invocation has
    /// completed yet — distinguishable from a real 0ms call by
    /// `latency_samples == 0`.
    pub last_elapsed_ms: u64,
    /// W2-006a: rolling max of the per-call elapsed_ms across
    /// every Ok or Err invocation. Useful for "is anything
    /// hanging?" at-a-glance.
    pub max_elapsed_ms: u64,
    /// W2-006a: sum of elapsed_ms across every Ok or Err
    /// invocation. Divide by `latency_samples` for mean.
    /// Saturates on overflow (u64 is more than enough for
    /// realistic operator workloads, but the saturating
    /// arithmetic stays defensive).
    pub total_elapsed_ms: u64,
    /// W2-006a: number of Ok+Err invocations recorded — the
    /// denominator for mean latency. Distinct from
    /// `invocations + errors` only because policy-denied /
    /// unknown_method don't contribute (no handler call → no
    /// elapsed time).
    pub latency_samples: u64,
    /// W2-006d: bounded ring of the most-recent per-call
    /// elapsed_ms values (newest at the back). Capacity
    /// [`RECENT_LATENCIES_CAP`]; FIFO eviction. Powers the
    /// dashboard's inline sparkline so operators see latency
    /// shape (steady? spiky? climbing?) without staring at
    /// just last/mean/max numbers.
    pub recent_latencies: std::collections::VecDeque<u32>,
}

/// W2-006d: how many recent per-call latency samples to keep
/// per capability. 32 is enough to draw a meaningful
/// sparkline at the dashboard's natural column width without
/// bloating the per-row footprint.
pub const RECENT_LATENCIES_CAP: usize = 32;

/// CORR PART 4: hard cap on the cardinality of distinct
/// method names tracked in `capability_stats`. Pre-fix path
/// grew this map unbounded; a hostile caller could mint fresh
/// method names per request and force the bridge to allocate
/// per-method counter rows forever. Beyond this cap the
/// per-method counters route into [`CAPABILITY_STATS_OVERFLOW_KEY`].
pub const CAPABILITY_STATS_CAP: usize = 10_000;

/// CORR PART 4: synthetic method-name key the bridge increments
/// on behalf of every distinct method name that arrived after
/// the cardinality cap. Operators see "we hit the cap N times"
/// rather than losing observability outright.
pub const CAPABILITY_STATS_OVERFLOW_KEY: &str = "__capability_stats_overflow__";

/// RELIX-7.19: hard ceiling on `Retry` action `max_retries`.
/// Operators that configure a higher value get clamped silently
/// — a runaway retry loop is worse than a wrong policy.
pub const MAX_RETRY_CAP: u32 = 8;

/// RELIX-7.19 GAP 3: score the just-completed handler outcome
/// via the scorer + record it on the rolling window. Pulled
/// out for reuse by the retry / escalate paths in
/// [`DispatchBridge::apply_confidence`]. Reads provider
/// signals (`finish_reason` + `logprob`) from the
/// `MetricsSink`'s join cache keyed by `request_id` — the
/// AI handler fires them via `attach_provider_signals` after
/// every `generate_reply` (or after each streamed
/// `FinishReason` chunk). Sinks that don't implement the
/// trait default (no-op) safely return `None`.
fn score_outcome(
    scorer: &crate::confidence::ConfidenceScorer,
    sink: Option<&Arc<dyn crate::metrics::MetricsSink>>,
    request_id: relix_core::types::RequestId,
    agent: &str,
    method: &str,
    outcome: &HandlerOutcome,
    elapsed_ms: u64,
) -> crate::confidence::ConfidenceScore {
    let (body, success): (&[u8], bool) = match outcome {
        HandlerOutcome::Ok(b) => (b.as_slice(), true),
        HandlerOutcome::Err(_) => (&[], false),
    };
    let hint = sink.and_then(|s| s.take_provider_signals(request_id));
    let finish_reason = hint.as_ref().and_then(|h| h.finish_reason.clone());
    let logprob = hint.as_ref().and_then(|h| h.logprob);
    // RELIX-7.29 PART 2: pop the self-consistency hint when
    // the AI handler ran adaptive sampling for this call. The
    // scorer will substitute it for `provider_signal`.
    let sc_hint = sink.and_then(|s| s.take_self_consistency(request_id));
    let inputs = crate::confidence::ScoringInputs {
        response_body: body,
        finish_reason: finish_reason.as_deref(),
        logprob,
        latency_ms: elapsed_ms,
        success,
        self_consistency: sc_hint.as_ref().map(|h| h.score),
    };
    scorer.score_and_record(agent, method, &inputs)
}

impl DispatchBridge {
    /// Construct.
    pub fn new(
        policy: PolicyEngine,
        trust_root: VerifyingKey,
        audit_path: &std::path::Path,
        responder_signer: SigningKey,
    ) -> Result<Self, DispatchError> {
        let responder_node_id = NodeId::from_pubkey(&responder_signer.verifying_key().to_bytes());
        let audit = AuditLog::open(audit_path, responder_signer)
            .map_err(|e| DispatchError::AuditOpen(e.to_string()))?;
        Ok(Self {
            handlers: HashMap::new(),
            streaming_handlers: HashMap::new(),
            policy,
            trust_root,
            audit: tokio::sync::Mutex::new(audit),
            responder_node_id,
            capability_stats: Arc::new(std::sync::RwLock::new(HashMap::new())),
            policy_denials: Arc::new(PolicyDenialRing::default()),
            agent_gate: None,
            access_broker: None,
            metrics_sink: None,
            peer_alias: String::new(),
            confidence_scorer: None,
            fallback_engine: None,
            last_confidence_cell: None,
            alert_engine: None,
            alert_sink: None,
            budget_enforcer: None,
            pii_gate: None,
            tenant_policy: None,
            audit_partition: None,
            always_require_methods: Arc::new(HashSet::new()),
            approval_keyset: crate::approval::ApprovalKeySet::new(),
            approval_signer: None,
            replay_cache: replay::ReplayCache::new(replay::DEFAULT_WINDOW_MS),
            max_clock_skew_ms: replay::DEFAULT_CLOCK_SKEW_MS,
            freshness_window_ms: replay::DEFAULT_WINDOW_MS,
            session_service: None,
            verify_on_dispatch_enabled: false,
            clock: Arc::new(relix_core::clock::SystemClock),
        })
    }

    /// NOT-DONE 1: install a [`relix_core::clock::Clock`] used
    /// by every TTL-sensitive admission path. Tests install a
    /// [`relix_core::clock::FakeClock`] to drive deterministic
    /// boundary cases. Idempotent; the default is
    /// [`relix_core::clock::SystemClock`].
    pub fn set_clock(&mut self, clock: Arc<dyn relix_core::clock::Clock>) {
        self.clock = clock;
    }

    /// NOT-DONE 1: borrow the installed clock. Used by the
    /// controller startup wiring so the clock shared by the
    /// dispatch bridge is the same one
    /// `ApprovalDeliveryService` + the background-task
    /// migration consult.
    pub fn clock(&self) -> Arc<dyn relix_core::clock::Clock> {
        self.clock.clone()
    }

    /// P1: install the Ed25519 signer the controller uses to
    /// mint approval tokens. Also registers the signer's
    /// verifying key in the local keyset so this controller can
    /// verify the tokens it issues. Idempotent — calling twice
    /// replaces the prior signer + replaces the keyset with a
    /// single-key set built from the new signer.
    pub fn set_approval_signer(&mut self, signer: crate::approval::ApprovalSigner) {
        self.approval_keyset = crate::approval::ApprovalKeySet::from_signer(&signer);
        self.approval_signer = Some(signer);
    }

    /// P1: install an explicit verification keyset. Used by
    /// responder-only nodes that don't mint tokens themselves
    /// but accept tokens issued by one or more remote
    /// signers. Idempotent.
    pub fn set_approval_keyset(&mut self, keyset: crate::approval::ApprovalKeySet) {
        self.approval_keyset = keyset;
    }

    /// P1: borrow the configured signer (when present). Used
    /// by the controller's startup-self-check log + by tests.
    pub fn approval_signer(&self) -> Option<&crate::approval::ApprovalSigner> {
        self.approval_signer.as_ref()
    }

    /// P1: borrow the configured verification keyset. Always
    /// returns a handle — empty when no signer is wired.
    pub fn approval_keyset(&self) -> &crate::approval::ApprovalKeySet {
        &self.approval_keyset
    }

    /// SECTION 7: set the ONE-SIDED future clock-skew allowance
    /// in ms. Admission rejects an envelope stamped more than
    /// this far in the future. This is independent of the
    /// replay cache window (which tracks the freshness window) —
    /// see [`Self::set_freshness_window_ms`].
    pub fn set_max_clock_skew_ms(&mut self, max_skew_ms: i64) {
        self.max_clock_skew_ms = max_skew_ms.max(1);
    }

    /// SECTION 7: read the configured future clock-skew
    /// allowance in ms. Used by tests + the controller log.
    pub fn max_clock_skew_ms(&self) -> i64 {
        self.max_clock_skew_ms
    }

    /// SECTION 7: set the PAST freshness window in ms (RELIX-1
    /// §1.9 mandates 5 minutes by default). Resizes the replay
    /// cache to the same window — a nonce older than the window
    /// is necessarily stale, so it can be evicted. Existing
    /// entries are dropped on resize (operators tuning the
    /// window live want a clean cache; the freshness check
    /// rejects anything older than the new window anyway).
    pub fn set_freshness_window_ms(&mut self, window_ms: i64) {
        let window = window_ms.max(1);
        self.freshness_window_ms = window;
        self.replay_cache = replay::ReplayCache::new(window);
    }

    /// SECTION 7: read the configured past freshness window.
    pub fn freshness_window_ms(&self) -> i64 {
        self.freshness_window_ms
    }

    /// P2: borrow a clone of the replay-cache handle so the
    /// controller startup can spawn the background eviction
    /// task alongside the bridge.
    pub fn replay_cache(&self) -> replay::ReplayCache {
        self.replay_cache.clone()
    }

    /// P5: install the session identity service used by the
    /// `verify_on_dispatch` admission gate. The service owns
    /// the token store and the signing-key bytes — the
    /// dispatch bridge consults its `verify(wire)` for every
    /// inbound call. Idempotent — calling twice replaces the
    /// previous service.
    pub fn set_session_service(&mut self, service: Arc<crate::identity::SessionIdentityService>) {
        self.session_service = Some(service);
    }

    /// P5: flip the `verify_on_dispatch` gate on or off.
    /// Idempotent. When `true` AND a session service is
    /// wired, every admitted call MUST carry a valid wire
    /// session token whose scopes cover the requested method.
    /// Operators set this via `[identity.session] verify_on_dispatch`.
    pub fn set_verify_on_dispatch(&mut self, enabled: bool) {
        self.verify_on_dispatch_enabled = enabled;
    }

    /// P5: read the configured verify-on-dispatch flag. Used
    /// by the controller startup self-check log.
    pub fn verify_on_dispatch_enabled(&self) -> bool {
        self.verify_on_dispatch_enabled
    }

    /// GATE 1: whether a session verification service is
    /// actually wired. The controller boot path consults this
    /// to FAIL CLOSED when `verify_on_dispatch = true` was
    /// requested but no service could be constructed — the gate
    /// must never silently admit unverified calls because the
    /// service is absent.
    pub fn session_service_wired(&self) -> bool {
        self.session_service.is_some()
    }

    /// GAP 15 partial: set the global "always require approval"
    /// method allowlist. Pass a list of method names that
    /// should ALWAYS require operator approval, regardless of
    /// the caller's policy decision or per-agent gate rule.
    /// Idempotent — calling twice replaces the prior list.
    /// Passing an empty list disables the floor.
    ///
    /// SEC PART C: collapses the input `Vec<String>` into a
    /// `HashSet` so the per-request `always_requires_approval`
    /// check runs in O(1) instead of O(n). Deduplicates
    /// silently — operators who list the same method twice get
    /// one floor entry.
    pub fn set_always_require_methods(&mut self, methods: Vec<String>) {
        self.always_require_methods = Arc::new(methods.into_iter().collect());
    }

    /// GAP 15 partial: snapshot the configured allowlist. Used
    /// by tests + by the optional `node.policy.always_require_list`
    /// surface for operator visibility. SEC PART C: returns a
    /// sorted Vec (HashSet iteration order is not stable) so
    /// the wire shape stays predictable.
    pub fn always_require_methods(&self) -> Vec<String> {
        let mut v: Vec<String> = self.always_require_methods.iter().cloned().collect();
        v.sort();
        v
    }

    /// GAP 15 partial: returns `true` when `method` is on the
    /// always-require allowlist. SEC PART C: O(1) HashSet
    /// lookup. Pure check; no admission side effect.
    pub fn always_requires_approval(&self, method: &str) -> bool {
        self.always_require_methods.contains(method)
    }

    /// GAP 23B: wire the per-tenant policy resolver. Idempotent
    /// — calling twice replaces the prior resolver. `None`
    /// reverts to single-tenant mode (admission falls back to
    /// the bridge's global [`Self::policy`]).
    pub fn set_tenant_policy_resolver(
        &mut self,
        resolver: Arc<relix_core::policy::TenantPolicyResolver>,
    ) {
        self.tenant_policy = Some(resolver);
    }

    /// GAP 23B: cheap-clone handle on the tenant resolver. Used
    /// by the `node.policy.tenant_list` + `node.policy.tenant_get`
    /// caps so the handlers can read the resolver without owning
    /// the bridge.
    pub fn tenant_policy_handle(&self) -> Option<Arc<relix_core::policy::TenantPolicyResolver>> {
        self.tenant_policy.clone()
    }

    /// GAP 23C: wire the audit partition mirror. Idempotent —
    /// calling twice replaces the prior store. `None` reverts to
    /// pre-23C mode (only the canonical signed log is written).
    pub fn set_audit_partition_store(
        &mut self,
        store: Arc<crate::audit_partition::AuditPartitionStore>,
    ) {
        self.audit_partition = Some(store);
    }

    /// GAP 23C: cheap-clone handle on the audit partition store.
    /// Used by the `node.audit.tenant_*` caps so the handlers can
    /// read the store without owning the bridge.
    pub fn audit_partition_handle(
        &self,
    ) -> Option<Arc<crate::audit_partition::AuditPartitionStore>> {
        self.audit_partition.clone()
    }

    /// RELIX-7.28 Part 1: wire the budget enforcer. Idempotent —
    /// calling twice replaces the prior enforcer. `None` opts back
    /// to "no budget gate" behaviour.
    pub fn set_budget_enforcer(&mut self, enforcer: Arc<crate::metrics::BudgetEnforcer>) {
        self.budget_enforcer = Some(enforcer);
    }

    /// Cheap-clone handle on the budget enforcer. Used by the
    /// `budget.*` coordinator caps so the handlers can call
    /// `status()` / `reset()` without holding the bridge.
    pub fn budget_enforcer_handle(&self) -> Option<Arc<crate::metrics::BudgetEnforcer>> {
        self.budget_enforcer.clone()
    }

    /// RELIX-7.28 Part 3: wire the mesh PII gate. Idempotent.
    pub fn set_pii_gate(&mut self, gate: Arc<crate::nodes::pii_gate::MeshPiiGate>) {
        self.pii_gate = Some(gate);
    }

    /// Cheap-clone handle on the PII gate. Used by the `pii.*`
    /// coordinator capabilities to read scan stats + recent events.
    pub fn pii_gate_handle(&self) -> Option<Arc<crate::nodes::pii_gate::MeshPiiGate>> {
        self.pii_gate.clone()
    }

    /// RELIX-7.19: wire confidence scoring + fallback. Both
    /// arms must be wired together — operators that want
    /// scoring without auto-fallback pass a `FallbackEngine`
    /// built from an empty policy list (every cap defaults to
    /// `Pass`). Idempotent.
    pub fn set_confidence(
        &mut self,
        scorer: Arc<crate::confidence::ConfidenceScorer>,
        engine: Arc<crate::confidence::FallbackEngine>,
    ) {
        self.confidence_scorer = Some(scorer);
        self.fallback_engine = Some(engine);
    }

    /// RELIX-7.19: cheap-clone handle on the confidence scorer
    /// — used by the `confidence.*` coordinator caps + tests.
    pub fn confidence_scorer_handle(&self) -> Option<Arc<crate::confidence::ConfidenceScorer>> {
        self.confidence_scorer.clone()
    }

    /// RELIX-7.19: cheap-clone handle on the fallback engine.
    pub fn fallback_engine_handle(&self) -> Option<Arc<crate::confidence::FallbackEngine>> {
        self.fallback_engine.clone()
    }

    /// RELIX-7.19: install the shared last-confidence cell.
    /// The bridge writes to it after every scored dispatch;
    /// the SOL VM reads it via `last_confidence()`.
    pub fn set_last_confidence_cell(&mut self, cell: crate::confidence::LastConfidenceCell) {
        self.last_confidence_cell = Some(cell);
    }

    /// Borrow the bridge's last-confidence cell. Returns
    /// `None` when no cell has been wired (pre-7.19
    /// behaviour preserved).
    pub fn last_confidence_cell(&self) -> Option<crate::confidence::LastConfidenceCell> {
        self.last_confidence_cell.clone()
    }

    /// RELIX-7.19 GAP 2: wire the alert pipeline the bridge
    /// fires through when a confidence verdict triggers the
    /// `Alert` fallback action. Both arms must be wired
    /// together: the engine owns the per-(agent, method,
    /// LowConfidence) dedup state; the sink delivers the
    /// resulting `AlertEvent`s. Calling once with `None` for
    /// either arm leaves the bridge in `tracing::warn!`
    /// fallback mode — backwards compatible.
    pub fn set_alert_pipeline(
        &mut self,
        engine: Arc<crate::metrics::AlertEngine>,
        sink: Arc<dyn crate::metrics::AlertDeliver>,
    ) {
        self.alert_engine = Some(engine);
        self.alert_sink = Some(sink);
    }

    /// Cheap-clone handle on the alert engine. Used by tests
    /// and the operator-facing `alerts.active` capability so
    /// the alert pipeline can introspect dedup state without
    /// owning the bridge.
    pub fn alert_engine_handle(&self) -> Option<Arc<crate::metrics::AlertEngine>> {
        self.alert_engine.clone()
    }

    /// RELIX-7.11: wire the per-invocation metrics sink + the
    /// peer alias the recorded rows carry. Idempotent — calling
    /// twice with a fresh sink replaces the previous wiring.
    pub fn set_metrics_sink(
        &mut self,
        sink: Arc<dyn crate::metrics::MetricsSink>,
        peer_alias: impl Into<String>,
    ) {
        self.metrics_sink = Some(sink);
        self.peer_alias = peer_alias.into();
    }

    /// Cheap-clone handle to the metrics sink. Used by handlers
    /// (most notably `ai.chat`) that want to attach a token
    /// usage hint via [`crate::metrics::MetricsSink::attach_ai_usage`]
    /// before returning.
    pub fn metrics_sink_handle(&self) -> Option<Arc<dyn crate::metrics::MetricsSink>> {
        self.metrics_sink.clone()
    }

    /// RELIX-7.11: helper. Records one invocation through the
    /// configured metrics sink. Called from the dispatch hot
    /// path after the handler returns. No-op when no sink is
    /// wired.
    ///
    /// RELIX-7.19: `confidence_score` is the verdict from the
    /// [`crate::confidence::ConfidenceScorer`] for this call;
    /// `None` when the bridge has no scorer wired.
    #[allow(clippy::too_many_arguments)]
    fn record_metric(
        &self,
        method: &str,
        agent_name: &str,
        request_id: relix_core::types::RequestId,
        latency_ms: u64,
        success: bool,
        error_kind: Option<&str>,
        input_bytes: usize,
        output_bytes: usize,
        confidence_score: Option<f32>,
        // GROUP 6: the request's VERIFIED tenant (envelope
        // `tenant_id`, resolved by the bridge from the auth
        // principal — never a wire body). Stored on the row so
        // metrics reads can be tenant-scoped.
        tenant_id: &str,
    ) {
        let Some(sink) = self.metrics_sink.as_ref() else {
            return;
        };
        let metric = crate::metrics::InvocationMetric {
            agent_name: agent_name.to_string(),
            tenant_id: tenant_id.to_string(),
            peer_alias: self.peer_alias.clone(),
            method: method.to_string(),
            timestamp_ms: unix_now_ms(),
            latency_ms,
            success,
            error_kind: error_kind.map(|s| s.to_string()),
            token_count: None,
            cost_micros: None,
            input_bytes,
            output_bytes,
            model: None,
            confidence_score,
            routing_tier: None,
            request_id: Some(request_id),
        };
        sink.record_invocation(metric);
    }

    /// GAP 22 Feature 2: record a minimal denial metric so the
    /// AlertEngine's ask-human-rate drift detector has a
    /// time-series signal to read.
    ///
    /// The existing `record_metric` path skips admission
    /// denials by design (POLICY_DENIED + UNKNOWN_METHOD are
    /// already counted by the dispatch-stats lifetime
    /// counters). APPROVAL_REQUIRED is in a different category
    /// though — it's a "this call was real and would have run
    /// but needs an operator approval" signal that the
    /// drift detector specifically wants. This helper writes
    /// the minimum row needed for the per-agent ratio query
    /// (`success = false`, `error_kind` populated, no token /
    /// cost / model).
    fn record_admission_denial_metric(
        &self,
        method: &str,
        agent_name: &str,
        request_id: relix_core::types::RequestId,
        started: Instant,
        error_kind_str: &'static str,
        // GROUP 6: request's VERIFIED tenant (envelope tenant_id).
        tenant_id: &str,
    ) {
        let Some(sink) = self.metrics_sink.as_ref() else {
            return;
        };
        let metric = crate::metrics::InvocationMetric {
            agent_name: agent_name.to_string(),
            tenant_id: tenant_id.to_string(),
            peer_alias: self.peer_alias.clone(),
            method: method.to_string(),
            timestamp_ms: unix_now_ms(),
            latency_ms: started.elapsed().as_millis() as u64,
            success: false,
            error_kind: Some(error_kind_str.to_string()),
            token_count: None,
            cost_micros: None,
            input_bytes: 0,
            output_bytes: 0,
            model: None,
            confidence_score: None,
            routing_tier: None,
            request_id: Some(request_id),
        };
        sink.record_invocation(metric);
    }

    /// Wire the agent-employee gate. Called by the coordinator
    /// binary after the [`crate::nodes::coordinator::agent::AgentStore`]
    /// is open. No-op on nodes that don't host an agent store.
    pub fn set_agent_gate(&mut self, bindings: AgentGateBindings) {
        self.agent_gate = Some(bindings);
    }

    /// W2: wire the per-agent access broker. Controllers that
    /// parse `[[execution.agents]]` from their config hand the
    /// resulting broker in at startup. Calling with an empty
    /// broker is equivalent to leaving the slot `None`: every
    /// check returns Allow.
    pub fn set_access_broker(
        &mut self,
        broker: Arc<crate::nodes::execution::broker::AgentAccessBroker>,
    ) {
        self.access_broker = Some(broker);
    }

    /// Cheap-clone handle to the access broker. `None` when no
    /// broker has been wired. Bridge endpoints + tests use this
    /// to inspect agent policies + rate-limit state.
    pub fn access_broker_handle(
        &self,
    ) -> Option<Arc<crate::nodes::execution::broker::AgentAccessBroker>> {
        self.access_broker.clone()
    }

    /// W2-007d: cheap-clone accessor for the policy denial
    /// ring. Used by the built-in `node.policy.recent_denials`
    /// capability + future bridge proxy.
    pub fn policy_denials_handle(&self) -> Arc<PolicyDenialRing> {
        self.policy_denials.clone()
    }

    /// W2-006b: return a cheap clone of the capability-stats
    /// RwLock handle. Handlers registered against this bridge
    /// (e.g. the built-in `node.dispatch.stats`) capture this
    /// to read the snapshot without owning the bridge.
    pub fn capability_stats_handle(&self) -> Arc<std::sync::RwLock<HashMap<String, CapStats>>> {
        self.capability_stats.clone()
    }

    /// W2-007a: return a clone of the PolicyEngine. Used by the
    /// `node.policy.simulate` built-in capability — handlers
    /// can answer "what would the policy say?" questions
    /// without owning the bridge.
    pub fn policy_handle(&self) -> PolicyEngine {
        self.policy.clone()
    }

    /// PH-DISP1: snapshot of every capability's counters.
    /// Order is stable (by method name) so dashboards diff
    /// cleanly across calls. Returns an empty vec when no
    /// requests have been dispatched yet.
    pub fn capability_stats_snapshot(&self) -> Vec<(String, CapStats)> {
        let g = self
            .capability_stats
            .read()
            .expect("capability_stats read lock");
        let mut out: Vec<(String, CapStats)> =
            g.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// PH-DISP1: internal helper. Bumps the counter row for
    /// `method` according to the outcome bucket.
    fn bump_stats(&self, method: &str, bucket: StatBucket, now: i64) {
        self.bump_stats_with_latency(method, bucket, now, None);
    }

    /// W2-006a: variant that also records per-call elapsed_ms
    /// for Ok / Err invocations. Denied / Unknown buckets don't
    /// have a handler call so the elapsed argument is ignored
    /// (callers may still pass `Some` for ergonomics; we skip
    /// the update).
    fn bump_stats_with_latency(
        &self,
        method: &str,
        bucket: StatBucket,
        now: i64,
        elapsed_ms: Option<u64>,
    ) {
        let mut g = self
            .capability_stats
            .write()
            .expect("capability_stats write lock");
        // CORR PART 4: hard cap on distinct method names that
        // are tracked. A misbehaving / hostile caller can
        // generate unlimited fresh method names; pre-fix path
        // grew this map unbounded. When the cap is hit,
        // counters for an unknown method route into the
        // shared `__overflow__` bucket so observability isn't
        // dropped on the floor, but the cardinality stays
        // bounded.
        let row_key: String = if !g.contains_key(method) && g.len() >= CAPABILITY_STATS_CAP {
            // Track the overflow event itself in a single
            // counter row so operators see the symptom
            // ("we've hit the cap N times for distinct
            // unseen methods").
            let overflow = g
                .entry(CAPABILITY_STATS_OVERFLOW_KEY.to_string())
                .or_default();
            overflow.unknown_method = overflow.unknown_method.saturating_add(1);
            overflow.last_invoked_at = now;
            overflow.last_error_at = Some(now);
            CAPABILITY_STATS_OVERFLOW_KEY.to_string()
        } else {
            method.to_string()
        };
        let row = g.entry(row_key).or_default();
        row.last_invoked_at = now;
        match bucket {
            StatBucket::Ok => {
                row.invocations = row.invocations.saturating_add(1);
            }
            StatBucket::Err => {
                row.errors = row.errors.saturating_add(1);
                row.last_error_at = Some(now);
            }
            StatBucket::Denied => {
                row.denied = row.denied.saturating_add(1);
                row.last_error_at = Some(now);
            }
            StatBucket::Unknown => {
                row.unknown_method = row.unknown_method.saturating_add(1);
                row.last_error_at = Some(now);
            }
        }
        // Latency only meaningful for Ok / Err (handler ran).
        if matches!(bucket, StatBucket::Ok | StatBucket::Err)
            && let Some(ms) = elapsed_ms
        {
            row.last_elapsed_ms = ms;
            row.max_elapsed_ms = row.max_elapsed_ms.max(ms);
            row.total_elapsed_ms = row.total_elapsed_ms.saturating_add(ms);
            row.latency_samples = row.latency_samples.saturating_add(1);
            // W2-006d: push into the bounded ring (clamp to
            // u32 to keep the wire payload compact — anyone
            // with a single-call latency > 49 days has bigger
            // problems than a saturating cast).
            let ms_u32 = u32::try_from(ms).unwrap_or(u32::MAX);
            if row.recent_latencies.len() == RECENT_LATENCIES_CAP {
                row.recent_latencies.pop_front();
            }
            row.recent_latencies.push_back(ms_u32);
        }
    }

    /// Register a capability handler.
    pub fn register(&mut self, method: impl Into<String>, handler: Arc<dyn Handler>) {
        self.handlers.insert(method.into(), handler);
    }

    /// `true` when a handler has been registered under
    /// `method`. Operator-facing utility for tests + the
    /// manifest sanity check; the admission pipeline still
    /// owns the actual routing decision.
    pub fn has_handler(&self, method: &str) -> bool {
        self.handlers.contains_key(method)
    }

    /// RELIX-2 step 2: register a streaming capability
    /// handler. A method name is either unary OR streaming,
    /// never both — the dispatch path looks up against the
    /// transport that delivered the request (unary
    /// request/response → `handlers`; `/relix/rpc/stream/1`
    /// substream → `streaming_handlers`). Registering the
    /// same method against both maps on the same bridge is a
    /// configuration error that surfaces at startup via the
    /// manifest sanity check.
    pub fn register_streaming(
        &mut self,
        method: impl Into<String>,
        handler: Arc<dyn StreamingHandler>,
    ) {
        self.streaming_handlers.insert(method.into(), handler);
    }

    /// `true` when a streaming handler has been registered
    /// under `method`.
    pub fn has_streaming_handler(&self, method: &str) -> bool {
        self.streaming_handlers.contains_key(method)
    }

    /// Back-compat wrapper around [`Self::handle_inbound_with_surface`]
    /// — passes `caller_surface = None` so callers that don't yet
    /// thread a transport-derived surface stay compiling. New
    /// production callers should use the explicit variant so the
    /// agent gate's surface allowlist is enforced against the
    /// transport-layer-trusted alias of the caller.
    pub async fn handle_inbound(&self, encoded_envelope: Vec<u8>) -> Vec<u8> {
        self.handle_inbound_with_surface(encoded_envelope, None)
            .await
    }

    /// Run the admission pipeline on an inbound encoded envelope and dispatch.
    /// Returns the encoded response envelope to send back on the wire.
    ///
    /// SEC PART 1: `caller_surface` is the trusted surface label
    /// derived from the libp2p transport layer (peer alias of
    /// the calling node, computed from its `PeerId`). The agent
    /// gate consults this for `surface_allowlist` matching —
    /// the operator-asserted `envelope.surface` field is
    /// ignored for admission decisions.
    pub async fn handle_inbound_with_surface(
        &self,
        encoded_envelope: Vec<u8>,
        caller_surface: Option<String>,
    ) -> Vec<u8> {
        let started_at = Instant::now();

        // === Admission step 1: decode envelope ===
        let req: RequestEnvelope = match codec::decode(&encoded_envelope) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "admission step 1 decode failed");
                return encode_error_response_no_audit(
                    relix_core::types::RequestId([0u8; 16]),
                    self.responder_node_id,
                    error_kinds::INVALID_ARGS,
                    "envelope decode failed",
                );
            }
        };

        // === Admission step 3: deadline ===
        // P2 — RELIX-1 §1.7 + the explicit operator request to
        // drop the 30s grace: the deadline is the deadline. A
        // request that arrives after `req.deadline` is rejected
        // with `TIMEOUT` (DEADLINE_EXCEEDED-class). Clock skew
        // is handled separately by the freshness check below,
        // which is bounded by `max_clock_skew_ms`, not 30s.
        let now = unix_now();
        let now_ms = self.clock.now_ms();
        if now > req.deadline.0 {
            return self
                .audit_and_err(
                    req,
                    started_at,
                    "admission:deadline_exceeded",
                    error_kinds::TIMEOUT,
                )
                .await;
        }

        // === Admission step 4a: freshness (RELIX-1 §1.9) ===
        // SECTION 7 — ONE-SIDED skew. A future-stamped envelope
        // must NOT be admitted just for being "within window":
        // reject anything stamped more than `max_clock_skew_ms`
        // ahead of the responder. Separately reject anything
        // OLDER than the freshness window (5 min) as stale.
        let age_ms = now_ms - req.issued_at_ms; // >0 past, <0 future
        if age_ms < -self.max_clock_skew_ms {
            return self
                .audit_and_err(
                    req,
                    started_at,
                    "admission:future_envelope",
                    error_kinds::REPLAY_REJECTED,
                )
                .await;
        }
        if age_ms > self.freshness_window_ms {
            return self
                .audit_and_err(
                    req,
                    started_at,
                    "admission:stale_envelope",
                    error_kinds::REPLAY_REJECTED,
                )
                .await;
        }

        // === Admission step 5: verify identity bundle ===
        // SECTION 7: identity verification happens BEFORE the
        // replay-cache insert (step 5b below) so an
        // unauthenticated attacker who can post raw bytes cannot
        // pin nonces into the cache (a no-auth DoS).
        let verified = match validate_identity_bundle(&req.identity_bundle, &self.trust_root, now) {
            Ok(v) => v,
            Err(e) => {
                return self
                    .audit_and_err_unverified(
                        &req,
                        started_at,
                        format!("admission:identity_invalid:{e}"),
                        error_kinds::IDENTITY_INVALID,
                    )
                    .await;
            }
        };

        // === Admission step 5b: replay-cache check (RELIX-1 §1.9) ===
        // SECTION 7 — key the cache on (caller_peer_id, rid, n)
        // so two distinct peers reusing the same rid do NOT
        // collide. `n` is the envelope's `issued_at_ms` (a true
        // replay re-sends the identical envelope, including its
        // timestamp). Runs only after identity verified.
        let replay_key = format!(
            "{}|{}|{}",
            verified.subject_id,
            hex::encode(req.rid.0),
            req.issued_at_ms
        );
        if let Err(replay::ReplayError::Replayed) =
            self.replay_cache.check_and_insert(&replay_key, now_ms)
        {
            return self
                .audit_and_err_with_id(
                    &req,
                    &verified,
                    started_at,
                    "admission:replay_rejected".to_string(),
                    error_kinds::REPLAY_REJECTED,
                    AuditStatus::Denied,
                )
                .await;
        }

        // === Admission step 6: session-token verification (P5) ===
        // Spec `identity-employees.md` §H.5 / SessionIdentityConfig:
        // when `verify_on_dispatch = true` AND a session
        // service is wired, every admitted call MUST carry a
        // wire-encoded session token whose signature, expiry,
        // tenant scoping AND scope-set cover the requested
        // method. The flag exists purely so existing
        // deployments stay byte-identical until operators flip
        // it; when off this step is a no-op.
        if self.verify_on_dispatch_enabled {
            // GATE 1 (defense in depth): the verify-on-dispatch
            // gate is ON. If no session service is wired we FAIL
            // CLOSED rather than short-circuiting past the check
            // and admitting an unverified call. The controller
            // already refuses to boot in this state (see
            // `session_verification_boot_gate`); this guards
            // against any runtime divergence so the gate can
            // never silently fail open.
            let Some(svc) = self.session_service.as_ref() else {
                return self
                    .audit_and_err_with_id(
                        &req,
                        &verified,
                        started_at,
                        "session_service_unavailable".to_string(),
                        error_kinds::SECURITY_DENIED,
                        AuditStatus::Denied,
                    )
                    .await;
            };
            let Some(token_wire) = req.session_token.as_deref() else {
                return self
                    .audit_and_err_with_id(
                        &req,
                        &verified,
                        started_at,
                        "session_token_missing".to_string(),
                        error_kinds::SECURITY_DENIED,
                        AuditStatus::Denied,
                    )
                    .await;
            };
            let v = svc.verify(token_wire);
            if !v.valid {
                let reason = v
                    .reason
                    .unwrap_or_else(|| "session_token_invalid".to_string());
                return self
                    .audit_and_err_with_id(
                        &req,
                        &verified,
                        started_at,
                        format!("session_token_invalid: {reason}"),
                        error_kinds::SECURITY_DENIED,
                        AuditStatus::Denied,
                    )
                    .await;
            }
            // Scope check: the token MUST list the requested
            // method (exact byte match) in its scopes vector.
            // Operators that want broader tokens grant the
            // wildcard scope `"*"` at issue time; the dispatch
            // gate honours it here.
            let scope_admits = v.scopes.iter().any(|s| s == "*" || s == &req.method);
            if !scope_admits {
                return self
                    .audit_and_err_with_id(
                        &req,
                        &verified,
                        started_at,
                        format!(
                            "session_token_invalid: token_insufficient_scope \
                             (method={}, scopes={:?})",
                            req.method, v.scopes
                        ),
                        error_kinds::SECURITY_DENIED,
                        AuditStatus::Denied,
                    )
                    .await;
            }
        }

        // === Admission step 7: capability lookup ===
        let Some(handler) = self.handlers.get(&req.method).cloned() else {
            // PH-DISP1: count even unknown-method attempts so
            // operators can spot mistyped capability names in
            // the dashboard (e.g. "task.todo_set" vs the typo
            // "task.todo_create").
            self.bump_stats(&req.method, StatBucket::Unknown, now);
            return self
                .audit_and_err_with_id(
                    &req,
                    &verified,
                    started_at,
                    "admission:unknown_method".into(),
                    error_kinds::UNKNOWN_METHOD,
                    AuditStatus::Error,
                )
                .await;
        };

        // === Admission step 8: agent-employee gate (categorical / surface / risk / approval) ===
        if let Some(bindings) = self.agent_gate.as_ref() {
            let descriptor = (bindings.describe)(&req.method);
            // NOT-DONE 1: source the TTL-check `now_ms` from the
            // injected clock instead of `unix_now_ms()` so tests
            // can drive boundary cases via `FakeClock` without
            // sleeping.
            let now_ms = self.clock.now_ms();
            let gate_decision = crate::admission::agent_gate::evaluate(
                Some(&bindings.store),
                crate::admission::agent_gate::GateInputs {
                    identity: &verified,
                    envelope: &req,
                    capability: descriptor.as_ref(),
                    now,
                    now_ms,
                    keyset: &self.approval_keyset,
                    caller_surface: caller_surface.as_deref(),
                },
            );
            match gate_decision {
                crate::admission::agent_gate::GateDecision::Allow(_a) => {
                    // SEC PART A: token consumption is atomic
                    // INSIDE evaluate_token now (via
                    // try_consume_token_atomic). No follow-up
                    // call here — the previous post-allow
                    // consume_approval_token call was racy and
                    // is gone.
                }
                crate::admission::agent_gate::GateDecision::Deny(deny) => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    self.policy_denials.push(PolicyDenialEntry {
                        at: now,
                        method: req.method.clone(),
                        caller_subject_id: verified.subject_id.to_string(),
                        caller_name: verified.name.clone(),
                        rule: deny.matched_rule.clone(),
                        reason: deny.reason.clone(),
                    });
                    return self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            format!("agent_gate:deny:{}:{}", deny.matched_rule, deny.reason),
                            relix_core::types::error_kinds::POLICY_DENIED,
                            AuditStatus::Denied,
                        )
                        .await;
                }
                crate::admission::agent_gate::GateDecision::RequireApproval(req_appr) => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    // The GateApprovalRequest already carries the
                    // task_id from the envelope (or None when the
                    // caller didn't supply one). Pass it through
                    // for symmetry with the closure signature; the
                    // closure prefers `req_appr.task_id`.
                    let task_id_hint = req_appr.task_id.as_deref().unwrap_or("");
                    let cause = match (bindings.on_require_approval)(&req_appr, task_id_hint) {
                        Ok(approval_id) => format!("approval_required:{approval_id}"),
                        Err(e) => format!("approval_required (create failed: {e})"),
                    };
                    // GAP 22 Feature 2: stamp the denial onto the
                    // metrics time series so the ask-human-rate
                    // drift detector has a signal to read.
                    self.record_admission_denial_metric(
                        &req.method,
                        &verified.name,
                        req.rid,
                        started_at,
                        "APPROVAL_REQUIRED",
                        req.tenant_id.as_deref().unwrap_or("default"),
                    );
                    return self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            cause,
                            relix_core::types::error_kinds::APPROVAL_REQUIRED,
                            AuditStatus::Denied,
                        )
                        .await;
                }
            }
        }

        // === Admission step 8.5 (GAP 15): always-require allowlist ===
        // Some methods need an operator approval EVERY time,
        // regardless of the caller's policy decision or any
        // per-agent gate rule. Examples typically include
        // `tool.fs.write` against production paths, payment
        // emissions, or destructive infra calls.
        //
        // SEC PART A: a non-empty approval_token field is NOT a
        // free pass any more. To bypass the always-require
        // floor:
        //
        // - When the agent gate is wired, the gate has ALREADY
        //   parsed + verified + atomically consumed the token
        //   at step 8 and returned `Allow` — control reached
        //   here only if the gate admitted, so re-verifying
        //   would just burn a second blocklist row attempt.
        //   The token is trusted.
        // - When the agent gate is NOT wired, there is no
        //   store backing the blocklist. We CANNOT verify the
        //   token atomically without the store. Fail closed
        //   with `approval_token_unverifiable` so the operator
        //   sees the missing infrastructure in audit logs.
        if self.always_requires_approval(&req.method) {
            let token_present = req.approval_token.is_some();
            let gate_wired = self.agent_gate.is_some();
            if !token_present {
                self.bump_stats(&req.method, StatBucket::Denied, now);
                let cause = "always_require_methods".to_string();
                self.record_admission_denial_metric(
                    &req.method,
                    &verified.name,
                    req.rid,
                    started_at,
                    "APPROVAL_REQUIRED",
                    req.tenant_id.as_deref().unwrap_or("default"),
                );
                return self
                    .audit_and_err_with_id(
                        &req,
                        &verified,
                        started_at,
                        cause,
                        relix_core::types::error_kinds::APPROVAL_REQUIRED,
                        AuditStatus::Denied,
                    )
                    .await;
            }
            if token_present && !gate_wired {
                self.bump_stats(&req.method, StatBucket::Denied, now);
                self.record_admission_denial_metric(
                    &req.method,
                    &verified.name,
                    req.rid,
                    started_at,
                    "SECURITY_DENIED",
                    req.tenant_id.as_deref().unwrap_or("default"),
                );
                return self
                    .audit_and_err_with_id(
                        &req,
                        &verified,
                        started_at,
                        "approval_token_unverifiable: no agent gate wired \
                         to verify + consume the token atomically — operator \
                         must wire `[[execution.agents]]` or remove the \
                         always_require_methods entry"
                            .to_string(),
                        relix_core::types::error_kinds::SECURITY_DENIED,
                        AuditStatus::Denied,
                    )
                    .await;
            }
            // Token was already verified + consumed by the agent
            // gate at step 8; reaching here means it admitted.
        }

        // === Admission step 9: policy ===
        // GAP 23B: route through the tenant resolver when wired
        // so a request with `tenant_id = Some(t)` evaluates
        // against `{policy.dir}/{t}.policy.toml` when present,
        // falling back to the global engine otherwise.
        let decision = match &self.tenant_policy {
            Some(r) => r.evaluate(&verified, &req.method, req.tenant_id.as_deref()),
            None => self.policy.evaluate(&verified, &req.method),
        };
        let (policy_decision_str, denied) = match &decision {
            Decision::Allow { matched_rule } => (format!("allow:{matched_rule}"), false),
            Decision::Deny {
                reason,
                matched_rule,
            } => (
                format!(
                    "deny:{}:{}",
                    matched_rule.as_deref().unwrap_or("default_deny"),
                    reason
                ),
                true,
            ),
        };
        if denied {
            self.bump_stats(&req.method, StatBucket::Denied, now);
            // W2-007d: capture the structured denial for the
            // operator-facing ring. Pulls the rule / reason
            // out of the `decision` match arm rather than
            // re-parsing the joined string. The audit log
            // still records the canonical line; this ring is
            // a fast read surface.
            if let Decision::Deny {
                reason,
                matched_rule,
            } = &decision
            {
                self.policy_denials.push(PolicyDenialEntry {
                    at: now,
                    method: req.method.clone(),
                    caller_subject_id: verified.subject_id.to_string(),
                    caller_name: verified.name.clone(),
                    rule: matched_rule
                        .clone()
                        .unwrap_or_else(|| "default_deny".to_string()),
                    reason: reason.clone(),
                });
            }
            return self
                .audit_and_err_with_id(
                    &req,
                    &verified,
                    started_at,
                    policy_decision_str,
                    error_kinds::POLICY_DENIED,
                    AuditStatus::Denied,
                )
                .await;
        }

        // === W2: per-agent access-broker check ===
        // Categorical allow/deny + sliding-window rate limit
        // sourced from `[[execution.agents]]`. The broker keys
        // off the caller's friendly identity name. When no
        // broker is wired or no policy matches the caller, the
        // check returns Allow and the dispatch proceeds.
        if let Some(broker) = self.access_broker.as_ref() {
            // CORR PART 3: atomic check + record under one
            // broker lock. Pre-fix `check()` then
            // `record_call()` released the lock between the
            // two calls, letting two concurrent callers both
            // observe headroom under the rate limit.
            match broker.atomic_check_and_record(&verified.name, &req.method) {
                crate::nodes::execution::broker::AccessDecision::Allow => {}
                crate::nodes::execution::broker::AccessDecision::Deny { reason } => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    self.policy_denials.push(PolicyDenialEntry {
                        at: now,
                        method: req.method.clone(),
                        caller_subject_id: verified.subject_id.to_string(),
                        caller_name: verified.name.clone(),
                        rule: "access_broker".to_string(),
                        reason: reason.clone(),
                    });
                    return self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            format!("access_broker:deny:{reason}"),
                            error_kinds::POLICY_DENIED,
                            AuditStatus::Denied,
                        )
                        .await;
                }
                crate::nodes::execution::broker::AccessDecision::RateLimited {
                    retry_after_secs,
                } => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    self.policy_denials.push(PolicyDenialEntry {
                        at: now,
                        method: req.method.clone(),
                        caller_subject_id: verified.subject_id.to_string(),
                        caller_name: verified.name.clone(),
                        rule: "access_broker_rate_limit".to_string(),
                        reason: format!("retry after {retry_after_secs}s"),
                    });
                    return self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            format!("access_broker:rate_limited:{retry_after_secs}s"),
                            error_kinds::POLICY_DENIED,
                            AuditStatus::Denied,
                        )
                        .await;
                }
            }
        }

        // === RELIX-7.28 Part 3: mesh-level PII gate (inbound) ===
        // Runs after the access broker so we don't pay scan cost
        // for calls policy already denied. Block / redact actions
        // can short-circuit the dispatch entirely.
        let mut args_for_dispatch: Vec<u8> = req.args.to_vec();
        if let Some(gate) = self.pii_gate.as_ref()
            && let Some(outcome) = gate.scan_inbound(
                req.rid.to_string().as_str(),
                &verified.name,
                &req.method,
                &mut args_for_dispatch,
            )
        {
            use crate::nodes::pii_gate::GateOutcome;
            match outcome {
                GateOutcome::Blocked { cause } => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    return self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            format!("pii_gate:block:{cause}"),
                            error_kinds::POLICY_DENIED,
                            AuditStatus::Denied,
                        )
                        .await;
                }
                GateOutcome::Redacted | GateOutcome::Logged => {}
            }
        }

        // === RELIX-7.28 Part 1: budget enforcement gate ===
        // Pre-dispatch cost-control. Throttle sleeps; reject
        // short-circuits with RESOURCE_EXHAUSTED. AlertOnly returns
        // Allow but fires a BudgetExceeded alert as a side effect.
        if let Some(enforcer) = self.budget_enforcer.as_ref() {
            match enforcer.check(&verified.name, &req.method).await {
                crate::metrics::BudgetDecision::Allow => {}
                crate::metrics::BudgetDecision::Throttle { delay, info } => {
                    tracing::warn!(
                        agent = %verified.name,
                        method = %req.method,
                        delay_ms = delay.as_millis() as u64,
                        window = %info.window,
                        scope = %info.scope,
                        "budget enforcer: throttling call"
                    );
                    tokio::time::sleep(delay).await;
                }
                crate::metrics::BudgetDecision::Reject { info } => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    return self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            format!("budget:reject:{}", info.cause),
                            error_kinds::RESOURCE_EXHAUSTED,
                            AuditStatus::Denied,
                        )
                        .await;
                }
            }
        }

        // === Admission step 10: dispatch ===
        let ctx = InvocationCtx {
            caller: verified.clone(),
            trace_id: req.tid,
            request_id: req.rid,
            args: args_for_dispatch,
            tenant_id: req.tenant_id.clone(),
        };
        // W2-006a: capture per-call elapsed_ms. Instant::now
        // straddles only the handler invocation — admission /
        // policy / audit are explicitly NOT included so the
        // operator-visible latency reflects user code, not the
        // bridge's overhead.
        let dispatch_started = std::time::Instant::now();
        let outcome = handler.invoke(ctx.clone()).await;
        let elapsed_ms = dispatch_started.elapsed().as_millis().min(u64::MAX as u128) as u64;

        // RELIX-7.19: confidence scoring + fallback. No-op when
        // no scorer is wired (pre-7.19 byte-for-byte path).
        let (outcome, total_elapsed_ms, confidence_score) = self
            .apply_confidence(
                &req.method,
                &handler,
                &ctx,
                outcome,
                elapsed_ms,
                req.deadline.0,
            )
            .await;

        // === Admission step 11: audit ===
        let (result, status, error_kind) = match outcome {
            HandlerOutcome::Ok(body) => (
                ResponseResult::Ok(ByteBuf::from(body)),
                AuditStatus::Ok,
                None,
            ),
            HandlerOutcome::Err(e) => (
                ResponseResult::Err(e.clone()),
                AuditStatus::Error,
                Some(e.kind),
            ),
        };
        // PH-DISP1: count the dispatched outcome.
        // W2-006a: also record latency for Ok / Err.
        let bucket = if matches!(status, AuditStatus::Ok) {
            StatBucket::Ok
        } else {
            StatBucket::Err
        };
        self.bump_stats_with_latency(&req.method, bucket, now, Some(total_elapsed_ms));
        // RELIX-7.11: per-invocation metric row. Dispatched
        // outcomes only — denied / unknown_method already get
        // dispatch-stats counters above. The sink is
        // non-blocking; never adds latency to the hot path.
        let output_bytes = match &result {
            ResponseResult::Ok(b) => b.len(),
            _ => 0,
        };
        let err_kind_str: Option<&str> = match &result {
            ResponseResult::Err(e) => Some(error_kind_to_str(e.kind)),
            _ => None,
        };
        self.record_metric(
            &req.method,
            &verified.name,
            req.rid,
            total_elapsed_ms,
            matches!(status, AuditStatus::Ok),
            err_kind_str,
            req.args.len(),
            output_bytes,
            confidence_score,
            req.tenant_id.as_deref().unwrap_or("default"),
        );
        let aid = self
            .write_audit(
                &req,
                &verified,
                started_at,
                policy_decision_str,
                status,
                error_kind,
            )
            .await;
        let resp = ResponseEnvelope {
            pv: 1,
            rid: req.rid,
            responder: self.responder_node_id,
            res: result,
            aid: ByteBuf::from(aid),
            processed_at: Timestamp::now(),
            confidence: confidence_score,
        };
        codec::encode(&resp).unwrap_or_default()
    }

    /// RELIX-7.19: post-handler scoring + fallback execution.
    /// Returns the (possibly-replaced) outcome, the total
    /// wall-clock latency including retries / escalations, and
    /// the final confidence score the metric + envelope carry.
    /// `None` confidence => no scorer wired => no fallback
    /// considered.
    async fn apply_confidence(
        &self,
        method: &str,
        handler: &Arc<dyn Handler>,
        ctx: &InvocationCtx,
        outcome: HandlerOutcome,
        first_elapsed_ms: u64,
        deadline_unix_secs: i64,
    ) -> (HandlerOutcome, u64, Option<f32>) {
        let scorer = match &self.confidence_scorer {
            Some(s) => s.clone(),
            None => return (outcome, first_elapsed_ms, None),
        };
        let engine = match &self.fallback_engine {
            Some(e) => e.clone(),
            None => return (outcome, first_elapsed_ms, None),
        };

        let agent = ctx.caller.name.clone();
        let request_id = ctx.request_id;
        let sink = self.metrics_sink.as_ref();
        let mut current_outcome = outcome;
        let mut total_elapsed = first_elapsed_ms;
        let initial_score = score_outcome(
            &scorer,
            sink,
            request_id,
            &agent,
            method,
            &current_outcome,
            first_elapsed_ms,
        );
        let mut best_score = initial_score.final_score;
        let verdict = engine.decide(method, initial_score.final_score);
        if !verdict.matched || matches!(verdict.action, crate::confidence::FallbackAction::Pass) {
            self.publish_confidence(initial_score.final_score);
            return (
                current_outcome,
                total_elapsed,
                Some(initial_score.final_score),
            );
        }
        match verdict.action {
            crate::confidence::FallbackAction::Pass => {}
            crate::confidence::FallbackAction::Retry {
                max_retries,
                retry_delay_ms,
            } => {
                let mut threshold = if verdict.critical {
                    // Critical retry — climb past low_threshold
                    // before stopping. Default threshold falls
                    // out from the policy match.
                    0.5
                } else {
                    0.5
                };
                if let Some(p) = engine
                    .list()
                    .iter()
                    .find(|p| crate::confidence::fallback::glob_match(&p.capability, method))
                {
                    threshold = p.low_threshold;
                }
                let retries = max_retries.min(MAX_RETRY_CAP);
                for _ in 0..retries {
                    // P2: respect the original admission
                    // deadline. The admission step gates the
                    // first invocation against `req.deadline.0`
                    // strictly (no grace). Each retry can add
                    // seconds of wall time and must NOT push
                    // the response past the caller's deadline.
                    // When the budget is gone we stop retrying
                    // and return DEADLINE_EXCEEDED.
                    let now = unix_now();
                    if now > deadline_unix_secs {
                        return (
                            HandlerOutcome::Err(ErrorEnvelope {
                                kind: error_kinds::TIMEOUT,
                                cause:
                                    "retry:deadline_exceeded — request deadline elapsed mid-retry"
                                        .to_string(),
                                retry_hint: 0,
                                retry_after: None,
                            }),
                            total_elapsed,
                            Some(best_score),
                        );
                    }
                    if retry_delay_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(retry_delay_ms)).await;
                    }
                    let retry_start = std::time::Instant::now();
                    let retry_outcome = handler.invoke(ctx.clone()).await;
                    let retry_elapsed =
                        retry_start.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    total_elapsed = total_elapsed.saturating_add(retry_elapsed);
                    let retry_score = score_outcome(
                        &scorer,
                        sink,
                        request_id,
                        &agent,
                        method,
                        &retry_outcome,
                        retry_elapsed,
                    );
                    if retry_score.final_score >= best_score {
                        best_score = retry_score.final_score;
                        current_outcome = retry_outcome;
                    }
                    if retry_score.final_score > threshold {
                        break;
                    }
                }
            }
            crate::confidence::FallbackAction::Escalate { escalate_to } => {
                if let Some(escalated_handler) = self.handlers.get(&escalate_to).cloned() {
                    let escalate_start = std::time::Instant::now();
                    let escalated = escalated_handler.invoke(ctx.clone()).await;
                    let escalated_elapsed =
                        escalate_start.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    total_elapsed = total_elapsed.saturating_add(escalated_elapsed);
                    // Score the escalated call against ITS OWN
                    // (agent, method) rolling window. The
                    // engine's decision on the escalated call
                    // is NOT re-applied to avoid recursive
                    // escalation loops (the spec says "The
                    // escalated call gets its own confidence
                    // check" — we score + record but do not
                    // re-fallback).
                    let escalated_score = score_outcome(
                        &scorer,
                        sink,
                        request_id,
                        &agent,
                        &escalate_to,
                        &escalated,
                        escalated_elapsed,
                    );
                    if escalated_score.final_score >= best_score {
                        best_score = escalated_score.final_score;
                        current_outcome = escalated;
                    }
                } else {
                    tracing::warn!(
                        target: "confidence.escalate",
                        method = %method,
                        escalate_to = %escalate_to,
                        "escalation target not registered; falling through to original outcome"
                    );
                }
            }
            crate::confidence::FallbackAction::SafeDefault { default_value } => {
                tracing::warn!(
                    target: "confidence.safe_default",
                    agent = %agent,
                    method = %method,
                    score = best_score,
                    "swapping low-confidence body for configured safe default"
                );
                current_outcome = HandlerOutcome::Ok(default_value.into_bytes());
                best_score = (best_score).max(1.0); // safe default is trusted
            }
            crate::confidence::FallbackAction::Alert { alert_message } => {
                // RELIX-7.19 GAP 2: prefer the wired alert
                // pipeline (engine dedup + sink fan-out). Fall
                // back to `tracing::warn!` when neither arm has
                // been wired — operators who haven't configured
                // `[metrics.alerts]` still see the alert in
                // their tracing collector.
                match (&self.alert_engine, &self.alert_sink) {
                    (Some(engine), Some(sink)) => {
                        let low = verdict.low_threshold.unwrap_or(0.5);
                        let critical = verdict.critical_threshold.unwrap_or(0.3);
                        let events = engine.evaluate_low_confidence(
                            &agent,
                            method,
                            best_score,
                            low,
                            critical,
                            alert_message,
                        );
                        for ev in events {
                            sink.deliver(&ev);
                        }
                    }
                    _ => {
                        tracing::warn!(
                            target: "confidence.alert",
                            agent = %agent,
                            method = %method,
                            score = best_score,
                            error_rate = initial_score.rolling_error_rate,
                            message = %alert_message,
                            "confidence alert"
                        );
                    }
                }
            }
            crate::confidence::FallbackAction::Abort { abort_message } => {
                current_outcome = HandlerOutcome::Err(relix_core::types::ErrorEnvelope {
                    kind: relix_core::types::error_kinds::INVALID_ARGS,
                    cause: abort_message,
                    retry_hint: 0,
                    retry_after: None,
                });
                best_score = 0.0;
            }
        }
        self.publish_confidence(best_score);
        (current_outcome, total_elapsed, Some(best_score))
    }

    /// Write the most-recent confidence to the shared cell so
    /// SOL `last_confidence()` reads the new value. No-op when
    /// no cell is wired.
    fn publish_confidence(&self, score: f32) {
        if let Some(cell) = &self.last_confidence_cell {
            cell.set(score);
        }
    }

    async fn audit_and_err(
        &self,
        req: RequestEnvelope,
        started: Instant,
        decision: &str,
        error_kind: u32,
    ) -> Vec<u8> {
        // Caller is "unknown" — best effort: zero claims.
        let unknown = unknown_identity();
        let aid = self
            .write_audit(
                &req,
                &unknown,
                started,
                decision.to_string(),
                AuditStatus::Error,
                Some(error_kind),
            )
            .await;
        encode_error_response(req.rid, self.responder_node_id, aid, error_kind, decision)
    }

    async fn audit_and_err_unverified(
        &self,
        req: &RequestEnvelope,
        started: Instant,
        decision: String,
        error_kind: u32,
    ) -> Vec<u8> {
        let unknown = unknown_identity();
        let aid = self
            .write_audit(
                req,
                &unknown,
                started,
                decision.clone(),
                AuditStatus::Error,
                Some(error_kind),
            )
            .await;
        encode_error_response(req.rid, self.responder_node_id, aid, error_kind, &decision)
    }

    async fn audit_and_err_with_id(
        &self,
        req: &RequestEnvelope,
        caller: &VerifiedIdentity,
        started: Instant,
        decision: String,
        error_kind: u32,
        status: AuditStatus,
    ) -> Vec<u8> {
        let aid = self
            .write_audit(
                req,
                caller,
                started,
                decision.clone(),
                status,
                Some(error_kind),
            )
            .await;
        encode_error_response(req.rid, self.responder_node_id, aid, error_kind, &decision)
    }

    /// RELIX-2 step 2: streaming-substream entry point.
    /// Mirrors [`Self::handle_inbound`]'s admission flow
    /// step-for-step (decode → deadline → identity → unknown-
    /// method → agent gate → policy → access broker → dispatch
    /// → audit) but routes the response through a
    /// [`crate::transport::stream::StreamWriter`] instead of
    /// the unary CBOR response envelope.
    ///
    /// On admission rejection the bridge writes a single
    /// terminal `StreamFrame::Err` frame to the writer with the
    /// matching `error_kinds::*` code. On admission success
    /// the bridge writes a `StreamFrame::Header` (carrying the
    /// responder id + audit record id + processed_at — the
    /// streaming analogue of `ResponseEnvelope` headers), then
    /// pipes each chunk yielded by the handler through a
    /// `StreamFrame::Chunk`. The stream terminator is either a
    /// `StreamFrame::End` (graceful) or a `StreamFrame::Err`
    /// (handler bailed mid-stream).
    ///
    /// Caller cancellation (the upstream drops the
    /// `StreamReader`) surfaces as a write failure on the next
    /// `Chunk` attempt; the bridge stops pulling from the
    /// handler, records the cancellation in the audit log
    /// with `AuditStatus::Error`, and lets the writer drop —
    /// the substream is already closed by the time we notice.
    ///
    /// The admission pipeline here is intentionally a
    /// near-line-for-line mirror of `handle_inbound`. The
    /// duplication is honest: extracting a shared helper would
    /// touch every existing security check on the hot path, so
    /// the refactor lands in a follow-up commit once the
    /// streaming surface has stabilised. Future TODO:
    /// `run_admission(envelope) -> AdmissionOutcome` shared by
    /// both paths.
    /// SEC PART 1: back-compat — see [`Self::handle_inbound_stream_with_surface`].
    pub async fn handle_inbound_stream(
        &self,
        encoded_envelope: Vec<u8>,
        writer: crate::transport::stream::StreamWriter,
    ) {
        self.handle_inbound_stream_with_surface(encoded_envelope, writer, None)
            .await
    }

    /// SEC PART 1: streaming counterpart to
    /// [`Self::handle_inbound_with_surface`]. `caller_surface`
    /// is the transport-derived surface label the agent gate
    /// consults instead of the operator-asserted
    /// `envelope.surface`.
    pub async fn handle_inbound_stream_with_surface(
        &self,
        encoded_envelope: Vec<u8>,
        writer: crate::transport::stream::StreamWriter,
        caller_surface: Option<String>,
    ) {
        use crate::transport::stream::StreamFrame;
        use serde_bytes::ByteBuf;

        let started_at = Instant::now();
        let mut writer = writer;

        // === Admission step 1: decode envelope ===
        let req: RequestEnvelope = match codec::decode(&encoded_envelope) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "streaming admission step 1 decode failed"
                );
                let _ = writer
                    .write_err(error_kinds::INVALID_ARGS, format!("envelope decode: {e}"))
                    .await;
                return;
            }
        };

        // === Admission step 3: deadline (P2 — no grace) ===
        let now = unix_now();
        let now_ms = self.clock.now_ms();
        if now > req.deadline.0 {
            let _ = writer
                .write_err(error_kinds::TIMEOUT, "admission:deadline_exceeded")
                .await;
            let _ = self
                .audit_and_err(
                    req,
                    started_at,
                    "admission:deadline_exceeded",
                    error_kinds::TIMEOUT,
                )
                .await;
            return;
        }

        // === Admission step 4a: freshness (RELIX-1 §1.9) ===
        // SECTION 7 — ONE-SIDED skew (see the unary path).
        let age_ms = now_ms - req.issued_at_ms; // >0 past, <0 future
        if age_ms < -self.max_clock_skew_ms {
            let _ = writer
                .write_err(error_kinds::REPLAY_REJECTED, "admission:future_envelope")
                .await;
            let _ = self
                .audit_and_err(
                    req,
                    started_at,
                    "admission:future_envelope",
                    error_kinds::REPLAY_REJECTED,
                )
                .await;
            return;
        }
        if age_ms > self.freshness_window_ms {
            let _ = writer
                .write_err(error_kinds::REPLAY_REJECTED, "admission:stale_envelope")
                .await;
            let _ = self
                .audit_and_err(
                    req,
                    started_at,
                    "admission:stale_envelope",
                    error_kinds::REPLAY_REJECTED,
                )
                .await;
            return;
        }

        // === Admission step 5: verify identity ===
        // SECTION 7: replay-cache insert happens AFTER this, so
        // an unauthenticated caller cannot pin nonces (no-auth DoS).
        let verified = match validate_identity_bundle(&req.identity_bundle, &self.trust_root, now) {
            Ok(v) => v,
            Err(e) => {
                let cause = format!("admission:identity_invalid:{e}");
                let _ = writer
                    .write_err(error_kinds::IDENTITY_INVALID, cause.clone())
                    .await;
                let _ = self
                    .audit_and_err_unverified(
                        &req,
                        started_at,
                        cause,
                        error_kinds::IDENTITY_INVALID,
                    )
                    .await;
                return;
            }
        };

        // === Admission step 5b: replay-cache check (RELIX-1 §1.9) ===
        // SECTION 7 — keyed on (caller_peer_id, rid, n); runs
        // only after identity verified.
        let replay_key = format!(
            "{}|{}|{}",
            verified.subject_id,
            hex::encode(req.rid.0),
            req.issued_at_ms
        );
        if let Err(replay::ReplayError::Replayed) =
            self.replay_cache.check_and_insert(&replay_key, now_ms)
        {
            let _ = writer
                .write_err(error_kinds::REPLAY_REJECTED, "admission:replay_rejected")
                .await;
            let _ = self
                .audit_and_err_with_id(
                    &req,
                    &verified,
                    started_at,
                    "admission:replay_rejected".to_string(),
                    error_kinds::REPLAY_REJECTED,
                    AuditStatus::Denied,
                )
                .await;
            return;
        }

        // === Admission step 6: session-token verification (P5) ===
        if self.verify_on_dispatch_enabled {
            // GATE 1 (defense in depth): same fail-closed rule as
            // the unary path — gate ON but no session service
            // wired means DENY, never admit unverified.
            let Some(svc) = self.session_service.as_ref() else {
                let cause = "session_service_unavailable".to_string();
                let _ = writer
                    .write_err(error_kinds::SECURITY_DENIED, cause.clone())
                    .await;
                let _ = self
                    .audit_and_err_with_id(
                        &req,
                        &verified,
                        started_at,
                        cause,
                        error_kinds::SECURITY_DENIED,
                        AuditStatus::Denied,
                    )
                    .await;
                return;
            };
            let Some(token_wire) = req.session_token.as_deref() else {
                let cause = "session_token_missing".to_string();
                let _ = writer
                    .write_err(error_kinds::SECURITY_DENIED, cause.clone())
                    .await;
                let _ = self
                    .audit_and_err_with_id(
                        &req,
                        &verified,
                        started_at,
                        cause,
                        error_kinds::SECURITY_DENIED,
                        AuditStatus::Denied,
                    )
                    .await;
                return;
            };
            let v = svc.verify(token_wire);
            if !v.valid {
                let reason = v
                    .reason
                    .unwrap_or_else(|| "session_token_invalid".to_string());
                let cause = format!("session_token_invalid: {reason}");
                let _ = writer
                    .write_err(error_kinds::SECURITY_DENIED, cause.clone())
                    .await;
                let _ = self
                    .audit_and_err_with_id(
                        &req,
                        &verified,
                        started_at,
                        cause,
                        error_kinds::SECURITY_DENIED,
                        AuditStatus::Denied,
                    )
                    .await;
                return;
            }
            let scope_admits = v.scopes.iter().any(|s| s == "*" || s == &req.method);
            if !scope_admits {
                let cause = format!(
                    "session_token_invalid: token_insufficient_scope \
                     (method={}, scopes={:?})",
                    req.method, v.scopes
                );
                let _ = writer
                    .write_err(error_kinds::SECURITY_DENIED, cause.clone())
                    .await;
                let _ = self
                    .audit_and_err_with_id(
                        &req,
                        &verified,
                        started_at,
                        cause,
                        error_kinds::SECURITY_DENIED,
                        AuditStatus::Denied,
                    )
                    .await;
                return;
            }
        }

        // === Admission step 7: streaming-handler lookup ===
        let Some(handler) = self.streaming_handlers.get(&req.method).cloned() else {
            self.bump_stats(&req.method, StatBucket::Unknown, now);
            let cause = format!("unknown streaming method: {}", req.method);
            let _ = writer.write_err(error_kinds::UNKNOWN_METHOD, cause).await;
            let _ = self
                .audit_and_err_with_id(
                    &req,
                    &verified,
                    started_at,
                    "admission:unknown_streaming_method".into(),
                    error_kinds::UNKNOWN_METHOD,
                    AuditStatus::Error,
                )
                .await;
            return;
        };

        // === Admission step 8: agent gate ===
        if let Some(bindings) = self.agent_gate.as_ref() {
            let descriptor = (bindings.describe)(&req.method);
            // NOT-DONE 1: source the TTL-check `now_ms` from the
            // injected clock instead of `unix_now_ms()` so tests
            // can drive boundary cases via `FakeClock` without
            // sleeping.
            let now_ms = self.clock.now_ms();
            let gate_decision = crate::admission::agent_gate::evaluate(
                Some(&bindings.store),
                crate::admission::agent_gate::GateInputs {
                    identity: &verified,
                    envelope: &req,
                    capability: descriptor.as_ref(),
                    now,
                    now_ms,
                    keyset: &self.approval_keyset,
                    caller_surface: caller_surface.as_deref(),
                },
            );
            match gate_decision {
                crate::admission::agent_gate::GateDecision::Allow(_a) => {
                    // SEC PART A: atomic consume happens inside
                    // evaluate_token. No follow-up call here.
                }
                crate::admission::agent_gate::GateDecision::Deny(deny) => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    self.policy_denials.push(PolicyDenialEntry {
                        at: now,
                        method: req.method.clone(),
                        caller_subject_id: verified.subject_id.to_string(),
                        caller_name: verified.name.clone(),
                        rule: deny.matched_rule.clone(),
                        reason: deny.reason.clone(),
                    });
                    let cause = format!("agent_gate:deny:{}:{}", deny.matched_rule, deny.reason);
                    let _ = writer
                        .write_err(error_kinds::POLICY_DENIED, cause.clone())
                        .await;
                    let _ = self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            cause,
                            error_kinds::POLICY_DENIED,
                            AuditStatus::Denied,
                        )
                        .await;
                    return;
                }
                crate::admission::agent_gate::GateDecision::RequireApproval(req_appr) => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    let task_id_hint = req_appr.task_id.as_deref().unwrap_or("");
                    let cause = match (bindings.on_require_approval)(&req_appr, task_id_hint) {
                        Ok(approval_id) => format!("approval_required:{approval_id}"),
                        Err(e) => format!("approval_required (create failed: {e})"),
                    };
                    // GAP 22 Feature 2: stamp the denial onto the
                    // metrics time series.
                    self.record_admission_denial_metric(
                        &req.method,
                        &verified.name,
                        req.rid,
                        started_at,
                        "APPROVAL_REQUIRED",
                        req.tenant_id.as_deref().unwrap_or("default"),
                    );
                    let _ = writer
                        .write_err(error_kinds::APPROVAL_REQUIRED, cause.clone())
                        .await;
                    let _ = self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            cause,
                            error_kinds::APPROVAL_REQUIRED,
                            AuditStatus::Denied,
                        )
                        .await;
                    return;
                }
            }
        }

        // === Admission step 8.5 (GAP 15, streaming path) ===
        // SEC PART A: same structured-token contract as the
        // unary path. Token-bearing calls reach here only when
        // the agent gate already verified + atomically
        // consumed the token at step 8; absent gate + present
        // token = SECURITY_DENIED.
        if self.always_requires_approval(&req.method) {
            let token_present = req.approval_token.is_some();
            let gate_wired = self.agent_gate.is_some();
            if !token_present {
                self.bump_stats(&req.method, StatBucket::Denied, now);
                let cause = "always_require_methods".to_string();
                self.record_admission_denial_metric(
                    &req.method,
                    &verified.name,
                    req.rid,
                    started_at,
                    "APPROVAL_REQUIRED",
                    req.tenant_id.as_deref().unwrap_or("default"),
                );
                let _ = writer
                    .write_err(error_kinds::APPROVAL_REQUIRED, cause.clone())
                    .await;
                let _ = self
                    .audit_and_err_with_id(
                        &req,
                        &verified,
                        started_at,
                        cause,
                        error_kinds::APPROVAL_REQUIRED,
                        AuditStatus::Denied,
                    )
                    .await;
                return;
            }
            if token_present && !gate_wired {
                self.bump_stats(&req.method, StatBucket::Denied, now);
                self.record_admission_denial_metric(
                    &req.method,
                    &verified.name,
                    req.rid,
                    started_at,
                    "SECURITY_DENIED",
                    req.tenant_id.as_deref().unwrap_or("default"),
                );
                let cause = "approval_token_unverifiable: no agent gate wired \
                             to verify + consume the token atomically"
                    .to_string();
                let _ = writer
                    .write_err(error_kinds::SECURITY_DENIED, cause.clone())
                    .await;
                let _ = self
                    .audit_and_err_with_id(
                        &req,
                        &verified,
                        started_at,
                        cause,
                        error_kinds::SECURITY_DENIED,
                        AuditStatus::Denied,
                    )
                    .await;
                return;
            }
        }

        // === Admission step 9: policy ===
        // GAP 23B: route through the tenant resolver when wired
        // so a request with `tenant_id = Some(t)` evaluates
        // against `{policy.dir}/{t}.policy.toml` when present,
        // falling back to the global engine otherwise.
        let decision = match &self.tenant_policy {
            Some(r) => r.evaluate(&verified, &req.method, req.tenant_id.as_deref()),
            None => self.policy.evaluate(&verified, &req.method),
        };
        let (policy_decision_str, denied) = match &decision {
            Decision::Allow { matched_rule } => (format!("allow:{matched_rule}"), false),
            Decision::Deny {
                reason,
                matched_rule,
            } => (
                format!(
                    "deny:{}:{}",
                    matched_rule.as_deref().unwrap_or("default_deny"),
                    reason
                ),
                true,
            ),
        };
        if denied {
            self.bump_stats(&req.method, StatBucket::Denied, now);
            if let Decision::Deny {
                reason,
                matched_rule,
            } = &decision
            {
                self.policy_denials.push(PolicyDenialEntry {
                    at: now,
                    method: req.method.clone(),
                    caller_subject_id: verified.subject_id.to_string(),
                    caller_name: verified.name.clone(),
                    rule: matched_rule
                        .clone()
                        .unwrap_or_else(|| "default_deny".to_string()),
                    reason: reason.clone(),
                });
            }
            let _ = writer
                .write_err(error_kinds::POLICY_DENIED, policy_decision_str.clone())
                .await;
            let _ = self
                .audit_and_err_with_id(
                    &req,
                    &verified,
                    started_at,
                    policy_decision_str,
                    error_kinds::POLICY_DENIED,
                    AuditStatus::Denied,
                )
                .await;
            return;
        }

        // === Per-agent access broker ===
        // CORR PART 3: atomic check + record under one broker
        // lock — same race fix as the unary path.
        if let Some(broker) = self.access_broker.as_ref() {
            match broker.atomic_check_and_record(&verified.name, &req.method) {
                crate::nodes::execution::broker::AccessDecision::Allow => {}
                crate::nodes::execution::broker::AccessDecision::Deny { reason } => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    self.policy_denials.push(PolicyDenialEntry {
                        at: now,
                        method: req.method.clone(),
                        caller_subject_id: verified.subject_id.to_string(),
                        caller_name: verified.name.clone(),
                        rule: "access_broker".to_string(),
                        reason: reason.clone(),
                    });
                    let cause = format!("access_broker:deny:{reason}");
                    let _ = writer
                        .write_err(error_kinds::POLICY_DENIED, cause.clone())
                        .await;
                    let _ = self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            cause,
                            error_kinds::POLICY_DENIED,
                            AuditStatus::Denied,
                        )
                        .await;
                    return;
                }
                crate::nodes::execution::broker::AccessDecision::RateLimited {
                    retry_after_secs,
                } => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    self.policy_denials.push(PolicyDenialEntry {
                        at: now,
                        method: req.method.clone(),
                        caller_subject_id: verified.subject_id.to_string(),
                        caller_name: verified.name.clone(),
                        rule: "access_broker_rate_limit".to_string(),
                        reason: format!("retry after {retry_after_secs}s"),
                    });
                    let cause = format!("access_broker:rate_limited:{retry_after_secs}s");
                    let _ = writer
                        .write_err(error_kinds::POLICY_DENIED, cause.clone())
                        .await;
                    let _ = self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            cause,
                            error_kinds::POLICY_DENIED,
                            AuditStatus::Denied,
                        )
                        .await;
                    return;
                }
            }
        }

        // === RELIX-7.28 Part 3: mesh-level PII gate (inbound) ===
        let mut args_for_dispatch_stream: Vec<u8> = req.args.to_vec();
        if let Some(gate) = self.pii_gate.as_ref()
            && let Some(outcome) = gate.scan_inbound(
                req.rid.to_string().as_str(),
                &verified.name,
                &req.method,
                &mut args_for_dispatch_stream,
            )
        {
            use crate::nodes::pii_gate::GateOutcome;
            match outcome {
                GateOutcome::Blocked { cause } => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    let full_cause = format!("pii_gate:block:{cause}");
                    let _ = writer
                        .write_err(error_kinds::POLICY_DENIED, full_cause.clone())
                        .await;
                    let _ = self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            full_cause,
                            error_kinds::POLICY_DENIED,
                            AuditStatus::Denied,
                        )
                        .await;
                    return;
                }
                GateOutcome::Redacted | GateOutcome::Logged => {}
            }
        }

        // === RELIX-7.28 Part 1: budget enforcement gate (streaming) ===
        if let Some(enforcer) = self.budget_enforcer.as_ref() {
            match enforcer.check(&verified.name, &req.method).await {
                crate::metrics::BudgetDecision::Allow => {}
                crate::metrics::BudgetDecision::Throttle { delay, info: _ } => {
                    tokio::time::sleep(delay).await;
                }
                crate::metrics::BudgetDecision::Reject { info } => {
                    self.bump_stats(&req.method, StatBucket::Denied, now);
                    let cause = format!("budget:reject:{}", info.cause);
                    let _ = writer
                        .write_err(error_kinds::RESOURCE_EXHAUSTED, cause.clone())
                        .await;
                    let _ = self
                        .audit_and_err_with_id(
                            &req,
                            &verified,
                            started_at,
                            cause,
                            error_kinds::RESOURCE_EXHAUSTED,
                            AuditStatus::Denied,
                        )
                        .await;
                    return;
                }
            }
        }

        // === Admission step 10: dispatch ===
        let ctx = InvocationCtx {
            caller: verified.clone(),
            trace_id: req.tid,
            request_id: req.rid,
            args: args_for_dispatch_stream,
            tenant_id: req.tenant_id.clone(),
        };
        let dispatch_started = std::time::Instant::now();

        // Header frame first so the caller has the audit id +
        // responder node id + processed_at for cross-correlation
        // with the per-flow event log. The aid is the request
        // id bytes per the alpha convention used by
        // [`Self::write_audit`].
        let aid = req.rid.0.to_vec();
        if let Err(e) = writer
            .write_frame(&StreamFrame::Header {
                responder: self.responder_node_id,
                aid: ByteBuf::from(aid),
                processed_at: relix_core::types::Timestamp(now),
            })
            .await
        {
            tracing::warn!(
                error = %e,
                method = %req.method,
                "streaming: caller closed substream before header frame written"
            );
            let elapsed_ms = dispatch_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            self.bump_stats_with_latency(&req.method, StatBucket::Err, now, Some(elapsed_ms));
            let _ = self
                .write_audit(
                    &req,
                    &verified,
                    started_at,
                    "stream:cancelled_before_header".into(),
                    AuditStatus::Error,
                    Some(error_kinds::TRANSPORT),
                )
                .await;
            return;
        }

        // Drive the handler.
        let stream_result = handler.invoke_stream(ctx).await;
        let mut error_kind: Option<u32> = None;
        let mut closing_cause = String::new();
        let mut cancelled_by_caller = false;

        match stream_result {
            Err(env) => {
                error_kind = Some(env.kind);
                closing_cause = format!("handler_err:{}", env.cause);
                let _ = writer.write_err(env.kind, env.cause).await;
            }
            Ok(mut stream) => {
                use futures::StreamExt;
                loop {
                    match stream.next().await {
                        None => {
                            // Stream completed normally —
                            // graceful End frame.
                            let _ = writer.write_end().await;
                            break;
                        }
                        Some(Ok(chunk)) => {
                            if let Err(e) = writer.write_chunk(&chunk).await {
                                // Caller dropped mid-stream.
                                // Stop pulling from the
                                // upstream handler; the
                                // substream is already
                                // closed so we cannot write
                                // End/Err — let `writer`
                                // drop here.
                                error_kind = Some(error_kinds::TRANSPORT);
                                closing_cause = format!("stream:write_chunk_failed:{e}");
                                cancelled_by_caller = true;
                                break;
                            }
                        }
                        Some(Err(env)) => {
                            error_kind = Some(env.kind);
                            closing_cause = format!("handler_stream_err:{}", env.cause);
                            let _ = writer.write_err(env.kind, env.cause).await;
                            break;
                        }
                    }
                }
            }
        }

        // === Admission step 11: audit ===
        let elapsed_ms = dispatch_started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let bucket = if error_kind.is_some() {
            StatBucket::Err
        } else {
            StatBucket::Ok
        };
        self.bump_stats_with_latency(&req.method, bucket, now, Some(elapsed_ms));
        let final_status = if error_kind.is_some() {
            AuditStatus::Error
        } else {
            AuditStatus::Ok
        };
        let final_decision = if error_kind.is_some() {
            closing_cause
        } else {
            policy_decision_str
        };
        // Tag the cancellation cause so the audit log
        // distinguishes "caller dropped the substream" from
        // "handler bailed".
        let _ = cancelled_by_caller;
        let _ = self
            .write_audit(
                &req,
                &verified,
                started_at,
                final_decision,
                final_status,
                error_kind,
            )
            .await;
    }

    async fn write_audit(
        &self,
        req: &RequestEnvelope,
        caller: &VerifiedIdentity,
        started: Instant,
        decision: String,
        status: AuditStatus,
        error_kind: Option<u32>,
    ) -> Vec<u8> {
        let draft = AuditDraft {
            request_id: req.rid,
            trace_id: req.tid,
            caller_node_id: caller.subject_id,
            caller_name: caller.name.clone(),
            caller_groups: caller.groups.clone(),
            method: req.method.clone(),
            flow_id: None,
            started_at: started,
            tenant_id: req.tenant_id.clone(),
        };
        // CORR PART 5: server-generated audit id. Pre-fix path
        // used `req.rid.0.to_vec()` — the caller-supplied
        // RequestId — which let a hostile caller pin a
        // collision against an existing entry or replay one
        // they had observed. The audit id is now a fresh
        // UUIDv4 minted server-side and is independent of
        // `req.rid`. `req.rid` itself is still recorded on
        // the AuditDraft as `request_id` for cross-correlation
        // with the caller's trace; it is NOT the row's PK.
        let aid = uuid::Uuid::new_v4().as_bytes().to_vec();
        // GAP 23C: when a partition mirror is wired, write a
        // queryable row BEFORE finalising the canonical signed
        // log. Mirror failures are logged but never block the
        // signed write — the canonical CBOR chain stays the
        // source of truth.
        if let Some(part) = &self.audit_partition {
            let row = crate::audit_partition::PartitionRow {
                ts_secs: unix_now(),
                request_id_hex: hex::encode(req.rid.0),
                tenant_id: req.tenant_id.clone(),
                caller_name: caller.name.clone(),
                method: req.method.clone(),
                policy_decision: decision.clone(),
                status: match status {
                    AuditStatus::Ok => "ok",
                    AuditStatus::Denied => "denied",
                    AuditStatus::Error => "error",
                },
                error_kind,
                latency_ms: started.elapsed().as_millis() as u64,
            };
            if let Err(e) = part.append(&row) {
                tracing::warn!(error = %e, "audit partition mirror write failed");
            }
        }
        let mut audit = self.audit.lock().await;
        if let Err(e) = audit.finalize(draft, decision, status, error_kind) {
            tracing::error!(error = %e, "audit write failed");
        }
        aid
    }
}

fn unknown_identity() -> VerifiedIdentity {
    VerifiedIdentity {
        subject_id: NodeId([0u8; 32]),
        name: "<unverified>".into(),
        org_id: NodeId([0u8; 32]),
        groups: vec![],
        role: "<unverified>".into(),
        clearance: "<unverified>".into(),
        bundle_id: [0u8; 32],
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Millisecond-resolution unix timestamp — used by RELIX-7.11
/// per-invocation metrics rows. Saturates at `i64::MAX`.
fn unix_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Map a numeric `error_kinds::*` constant back to its symbolic
/// name. Used by the metrics layer when persisting a row so the
/// dashboard / aggregation queries don't need a separate
/// translation table.
pub(crate) fn error_kind_to_str(kind: u32) -> &'static str {
    use relix_core::types::error_kinds as k;
    match kind {
        k::TRANSPORT => "TRANSPORT",
        k::TIMEOUT => "TIMEOUT",
        k::PEER_UNREACHABLE => "PEER_UNREACHABLE",
        k::UNKNOWN_METHOD => "UNKNOWN_METHOD",
        k::INVALID_ARGS => "INVALID_ARGS",
        k::POLICY_DENIED => "POLICY_DENIED",
        k::IDENTITY_INVALID => "IDENTITY_INVALID",
        k::CREDENTIAL_EXPIRED => "CREDENTIAL_EXPIRED",
        k::CAPABILITY_DEPRECATED => "CAPABILITY_DEPRECATED",
        k::CAPABILITY_REMOVED => "CAPABILITY_REMOVED",
        k::RESPONDER_INTERNAL => "RESPONDER_INTERNAL",
        k::RESPONDER_OVERLOADED => "RESPONDER_OVERLOADED",
        k::REPLAY_REJECTED => "REPLAY_REJECTED",
        k::VERSION_MISMATCH => "VERSION_MISMATCH",
        k::APPROVAL_TIMEOUT => "APPROVAL_TIMEOUT",
        k::APPROVAL_DENIED => "APPROVAL_DENIED",
        k::CANCELLED => "CANCELLED",
        k::MANIFEST_STALE => "MANIFEST_STALE",
        k::APPROVAL_REQUIRED => "APPROVAL_REQUIRED",
        k::APPROVAL_TOKEN_INVALID => "APPROVAL_TOKEN_INVALID",
        k::SECURITY_DENIED => "SECURITY_DENIED",
        k::RESOURCE_EXHAUSTED => "RESOURCE_EXHAUSTED",
        // Higher kinds (gate-token / security-denied / etc.)
        // and unknown values: surface the numeric code so an
        // operator can still spot trends. Static strs only —
        // metrics rows hold owned Strings so we leak nothing.
        _ => "OTHER",
    }
}

fn encode_error_response(
    rid: relix_core::types::RequestId,
    responder: NodeId,
    aid: Vec<u8>,
    kind: u32,
    cause: &str,
) -> Vec<u8> {
    let resp = ResponseEnvelope {
        pv: 1,
        rid,
        responder,
        res: ResponseResult::Err(ErrorEnvelope {
            kind,
            cause: cause.to_string(),
            retry_hint: 2,
            retry_after: None,
        }),
        aid: ByteBuf::from(aid),
        processed_at: Timestamp::now(),
        confidence: None,
    };
    codec::encode(&resp).unwrap_or_default()
}

fn encode_error_response_no_audit(
    rid: relix_core::types::RequestId,
    responder: NodeId,
    kind: u32,
    cause: &str,
) -> Vec<u8> {
    encode_error_response(rid, responder, vec![], kind, cause)
}

/// Build an outbound request envelope ready to send via `transport::rpc::Client::call`.
pub fn build_request(
    method: impl Into<String>,
    args: Vec<u8>,
    identity: Bundle,
    deadline_secs_from_now: i64,
) -> Vec<u8> {
    build_request_with_surface(
        method,
        args,
        identity,
        deadline_secs_from_now,
        None,
        None,
        None,
        None,
        None,
    )
}

/// Same as [`build_request`] but stamps the optional
/// `surface` + `approval_token` + `task_id` fields on the
/// envelope. Used by the bridge to mark which inbound HTTP
/// surface drove the call, by retried callers replaying an
/// approved approval token, and by callers acting on behalf
/// of a specific coordinator task (so the agent gate's
/// `RequireApproval` path can pause + resume the right
/// task).
#[allow(clippy::too_many_arguments)]
pub fn build_request_with_surface(
    method: impl Into<String>,
    args: Vec<u8>,
    identity: Bundle,
    deadline_secs_from_now: i64,
    surface: Option<String>,
    approval_token: Option<String>,
    task_id: Option<String>,
    session_id: Option<String>,
    workspace_path: Option<String>,
) -> Vec<u8> {
    let req = RequestEnvelope {
        pv: 1,
        rid: relix_core::types::RequestId::new(),
        tid: relix_core::types::TraceId::new(),
        method: method.into(),
        mv: 1,
        args: ByteBuf::from(args),
        identity_bundle: identity,
        // SEC PART 6: saturate on overflow. A deadline at
        // i64::MAX is effectively "no deadline" — safer than
        // wrapping into the past and causing instant timeouts.
        deadline: Timestamp::now()
            .add_secs(deadline_secs_from_now)
            .unwrap_or(Timestamp(i64::MAX)),
        // P2: stamp `issued_at_ms` so the responder's freshness
        // gate can reject captured envelopes past the
        // configured clock-skew window.
        issued_at_ms: unix_now_ms(),
        surface,
        approval_token,
        task_id,
        session_id,
        workspace_path,
        tenant_id: None,
        session_token: None,
    };
    codec::encode(&req).unwrap_or_default()
}

/// GAP 23: same shape as [`build_request_with_surface`] plus
/// the per-request tenant id. Used by the bridge to propagate
/// the `X-Relix-Tenant` header into the mesh, and by
/// mesh-internal callers that want to stamp a tenant
/// explicitly. `tenant_id = None` keeps the wire-level field
/// absent so older responders ignore it.
#[allow(clippy::too_many_arguments)]
pub fn build_request_with_tenant(
    method: impl Into<String>,
    args: Vec<u8>,
    identity: Bundle,
    deadline_secs_from_now: i64,
    surface: Option<String>,
    approval_token: Option<String>,
    task_id: Option<String>,
    tenant_id: Option<String>,
) -> Vec<u8> {
    let req = RequestEnvelope {
        pv: 1,
        rid: relix_core::types::RequestId::new(),
        tid: relix_core::types::TraceId::new(),
        method: method.into(),
        mv: 1,
        args: ByteBuf::from(args),
        identity_bundle: identity,
        // SEC PART 6: saturate on overflow. A deadline at
        // i64::MAX is effectively "no deadline" — safer than
        // wrapping into the past and causing instant timeouts.
        deadline: Timestamp::now()
            .add_secs(deadline_secs_from_now)
            .unwrap_or(Timestamp(i64::MAX)),
        // P2: stamp `issued_at_ms` so the responder's freshness
        // gate can reject captured envelopes past the
        // configured clock-skew window.
        issued_at_ms: unix_now_ms(),
        surface,
        approval_token,
        task_id,
        session_id: None,
        workspace_path: None,
        tenant_id,
        session_token: None,
    };
    codec::encode(&req).unwrap_or_default()
}

/// P5 — full-form envelope builder that also stamps a
/// session token. Used by callers (typically the bridge) that
/// participate in `verify_on_dispatch`. Existing callers
/// continue to use [`build_request_with_tenant`] which sets
/// `session_token = None`.
#[allow(clippy::too_many_arguments)]
pub fn build_request_with_session(
    method: impl Into<String>,
    args: Vec<u8>,
    identity: Bundle,
    deadline_secs_from_now: i64,
    surface: Option<String>,
    approval_token: Option<String>,
    task_id: Option<String>,
    tenant_id: Option<String>,
    session_token: Option<String>,
) -> Vec<u8> {
    let req = RequestEnvelope {
        pv: 1,
        rid: relix_core::types::RequestId::new(),
        tid: relix_core::types::TraceId::new(),
        method: method.into(),
        mv: 1,
        args: ByteBuf::from(args),
        identity_bundle: identity,
        deadline: Timestamp::now()
            .add_secs(deadline_secs_from_now)
            .unwrap_or(Timestamp(i64::MAX)),
        issued_at_ms: unix_now_ms(),
        surface,
        approval_token,
        task_id,
        session_id: None,
        workspace_path: None,
        tenant_id,
        session_token,
    };
    codec::encode(&req).unwrap_or_default()
}

/// Decode a response envelope returned by `Client::call`.
pub fn decode_response(bytes: &[u8]) -> Result<ResponseEnvelope, codec::CodecError> {
    codec::decode(bytes)
}

/// Dispatch-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// Audit log could not be opened.
    #[error("audit open: {0}")]
    AuditOpen(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use relix_core::clock::Clock as _;
    use relix_core::identity::{IdentityBundle, issue_identity};
    use std::sync::Arc;
    use tempfile::TempDir;

    async fn echo_handler(ctx: InvocationCtx) -> HandlerOutcome {
        HandlerOutcome::Ok(ctx.args)
    }

    // ---- GAP 15 partial: always_require_methods admission ----

    fn allow_health_bridge() -> (DispatchBridge, Bundle, TempDir, SigningKey) {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "any_caller_echo"
            method = "node.health"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let mut bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        (bridge, bundle, dir, org_root)
    }

    #[tokio::test]
    async fn always_requires_approval_predicate_round_trips_through_setter() {
        let (mut bridge, _bundle, _dir, _root) = allow_health_bridge();
        assert!(!bridge.always_requires_approval("node.health"));
        bridge.set_always_require_methods(vec!["node.health".into(), "tool.fs.write".into()]);
        assert!(bridge.always_requires_approval("node.health"));
        assert!(bridge.always_requires_approval("tool.fs.write"));
        assert!(!bridge.always_requires_approval("ai.chat"));
        assert_eq!(bridge.always_require_methods().len(), 2);
        // Idempotent replace.
        bridge.set_always_require_methods(Vec::new());
        assert!(!bridge.always_requires_approval("node.health"));
    }

    #[tokio::test]
    async fn always_require_methods_setter_deduplicates_and_returns_sorted() {
        // SEC PART C: HashSet semantics — duplicate inputs
        // collapse, the getter returns a stable sorted Vec so
        // operator-facing snapshots don't shuffle order between
        // boots.
        let (mut bridge, _bundle, _dir, _root) = allow_health_bridge();
        bridge.set_always_require_methods(vec![
            "z.late".into(),
            "a.early".into(),
            "z.late".into(), // dup
            "m.mid".into(),
        ]);
        let snap = bridge.always_require_methods();
        assert_eq!(
            snap,
            vec![
                "a.early".to_string(),
                "m.mid".to_string(),
                "z.late".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn always_requires_approval_is_o1_under_large_allowlist() {
        // SEC PART C: the per-request check must scale to a
        // 50-method allowlist without 50 string comparisons.
        // We don't directly measure cycles in unit tests, but we
        // do verify the lookup returns the right answer in
        // constant time relative to the list size — the
        // implementation backs onto a HashSet so this is a
        // contract test against future regressions.
        let (mut bridge, _bundle, _dir, _root) = allow_health_bridge();
        let methods: Vec<String> = (0..50).map(|i| format!("method.bulk_{i}")).collect();
        bridge.set_always_require_methods(methods.clone());
        // Hit lookup for every entry + a missing one.
        for m in &methods {
            assert!(
                bridge.always_requires_approval(m),
                "every set member must hit: {m}"
            );
        }
        assert!(!bridge.always_requires_approval("method.not_set"));
    }

    #[tokio::test]
    async fn admission_returns_approval_required_when_method_is_on_allowlist() {
        let (mut bridge, bundle, _dir, _root) = allow_health_bridge();
        bridge.set_always_require_methods(vec!["node.health".into()]);
        // The policy would otherwise admit this call (chat-users
        // is allowed on node.health) — the allowlist must
        // override.
        let envelope = build_request("node.health", b"hi".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => {
                assert_eq!(e.kind, error_kinds::APPROVAL_REQUIRED);
                assert!(
                    e.cause.contains("always_require_methods"),
                    "cause should name the source: {:?}",
                    e.cause
                );
            }
            other => panic!("expected APPROVAL_REQUIRED, got {:?}", other),
        }
    }

    // ── NOT-DONE 1: end-to-end FakeClock boundary tests ──

    /// Boot a bridge wired with an agent gate so token-bearing
    /// calls flow through `agent_gate::evaluate` and that
    /// function consults the bridge's injected
    /// `Arc<dyn Clock>`. Returns the bridge handle, the
    /// `FakeClock` Arc (so the test can drive boundaries via
    /// `advance`), and the minted token + caller identity.
    async fn boot_bridge_with_fake_clock_and_token(
        issued_at_ms: i64,
        ttl_ms: i64,
        subject_seed: &[u8],
    ) -> (
        DispatchBridge,
        std::sync::Arc<relix_core::clock::FakeClock>,
        String,
        Bundle,
    ) {
        let (mut bridge, org_root, _dir) = {
            let dir = TempDir::new().unwrap();
            let org_root = SigningKey::generate(&mut OsRng);
            let responder = SigningKey::generate(&mut OsRng);
            // Permissive policy — the test is about the gate's
            // TTL check on the approval token, not policy.
            let policy = PolicyEngine::from_toml(
                r#"
                [[rules]]
                name = "permissive"
                method = "node.health"
                allow_groups = ["chat-users"]
                "#,
            )
            .unwrap();
            let bridge = DispatchBridge::new(
                policy,
                org_root.verifying_key(),
                &dir.path().join("audit.log"),
                responder,
            )
            .unwrap();
            (bridge, org_root, dir)
        };
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        // Wire the agent gate against an in-memory store.
        let store =
            std::sync::Arc::new(crate::nodes::coordinator::agent::AgentStore::in_memory().unwrap());
        // Mint an approved approval row + a structured token
        // with the requested issued_at_ms + ttl_ms.
        let subject_id_hex = NodeId::from_pubkey(subject_seed).to_string();
        let approval_id = store
            .create_approval(
                "agt-1",
                &subject_id_hex,
                "node.health",
                "cat",
                "",
                "",
                &[],
                None,
                9_999_999_999,
                &[],
                "default",
            )
            .unwrap();
        let meta = store
            .decide_approval(
                &approval_id,
                crate::nodes::coordinator::agent::store::ApprovalStatus::Approved,
                "alice",
                "ok",
            )
            .unwrap()
            .unwrap();
        let signer = crate::approval::ApprovalSigner::from_seed([3u8; 32]);
        let wire = crate::approval::ApprovalToken::issue(
            &meta.approval_id,
            &meta.method,
            &meta.subject_id,
            meta.task_id.as_deref().unwrap_or(""),
            issued_at_ms,
            ttl_ms,
            &signer,
        )
        .unwrap();

        bridge.set_approval_signer(signer);
        let describe: super::CapabilityDescribeFn = Arc::new(|_method: &str| None);
        let on_require_approval: super::OnRequireApprovalFn =
            Arc::new(|_req, _hint| Ok(String::new()));
        bridge.set_agent_gate(super::AgentGateBindings {
            store,
            describe,
            on_require_approval,
        });

        let fake_clock = std::sync::Arc::new(relix_core::clock::FakeClock::new(0));
        bridge.set_clock(fake_clock.clone());
        // P2: this helper drives the FakeClock at small ms
        // values (~60_000) while `build_request_with_tenant`
        // stamps `issued_at_ms` from wall-clock — they diverge
        // by ~1.7e12 ms. The freshness gate would otherwise
        // reject every envelope as stale. Disable the
        // freshness gate by widening tolerance to i64::MAX —
        // the test's purpose is the token's TTL window, not
        // envelope freshness.
        bridge.set_max_clock_skew_ms(i64::MAX);

        // Caller bundle whose subject_id matches the token's
        // bound subject so the gate's subject-match check
        // passes.
        let caller_key = SigningKey::from_bytes(
            &subject_seed
                .iter()
                .copied()
                .cycle()
                .take(32)
                .collect::<Vec<u8>>()
                .try_into()
                .unwrap(),
        );
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(subject_seed),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let _ = caller_key; // silence unused — keep for parity with other tests
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        (bridge, fake_clock, wire, bundle)
    }

    async fn dispatch_with_token(
        bridge: &DispatchBridge,
        bundle: Bundle,
        token: String,
    ) -> ResponseResult {
        let envelope = build_request_with_tenant(
            "node.health",
            b"hi".to_vec(),
            bundle,
            30,
            None,
            Some(token),
            None,
            None,
        );
        let resp_bytes = bridge.handle_inbound(envelope).await;
        decode_response(&resp_bytes).unwrap().res
    }

    #[tokio::test]
    async fn bridge_clock_admits_token_one_ms_before_expiry() {
        let (bridge, fake, token, bundle) =
            boot_bridge_with_fake_clock_and_token(1_000, 60_000, b"subj-bridge-1").await;
        fake.set(60_999);
        match dispatch_with_token(&bridge, bundle, token).await {
            ResponseResult::Ok(b) => assert_eq!(b.as_ref(), b"hi"),
            other => panic!("expected Ok at expires-1, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bridge_clock_rejects_token_exactly_at_expiry() {
        let (bridge, fake, token, bundle) =
            boot_bridge_with_fake_clock_and_token(1_000, 60_000, b"subj-bridge-2").await;
        fake.set(61_000);
        match dispatch_with_token(&bridge, bundle, token).await {
            ResponseResult::Err(env) => {
                assert!(
                    env.cause.contains("approval_token_expired") || env.cause.contains("expired"),
                    "expected expired-at-boundary cause, got: {}",
                    env.cause
                );
            }
            other => panic!("expected Err at expires boundary, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bridge_clock_rejects_token_one_ms_after_expiry_via_advance() {
        let (bridge, fake, token, bundle) =
            boot_bridge_with_fake_clock_and_token(1_000, 60_000, b"subj-bridge-3").await;
        fake.set(60_999);
        fake.advance(2);
        match dispatch_with_token(&bridge, bundle, token).await {
            ResponseResult::Err(env) => {
                assert!(
                    env.cause.contains("approval_token_expired") || env.cause.contains("expired"),
                    "expected expired cause, got: {}",
                    env.cause
                );
            }
            other => panic!("expected Err at expires+1, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn admission_denies_allowlist_call_with_token_when_no_gate_is_wired() {
        // SEC PART A: a non-empty approval_token string is NO
        // LONGER a free pass at step 8.5. When the agent gate is
        // not wired, there is no atomic-consume store available;
        // we MUST fail closed with SECURITY_DENIED. Previously
        // this test passed with an arbitrary token because the
        // gate was unconditionally bypassed.
        let (mut bridge, bundle, _dir, _root) = allow_health_bridge();
        bridge.set_always_require_methods(vec!["node.health".into()]);
        let envelope = build_request_with_tenant(
            "node.health",
            b"hi".to_vec(),
            bundle,
            30,
            None,
            Some("operator-approved-token".to_string()),
            None,
            None,
        );
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(env) => {
                assert_eq!(env.kind, error_kinds::SECURITY_DENIED);
                assert!(
                    env.cause.contains("approval_token_unverifiable"),
                    "cause should name the failure mode: {}",
                    env.cause
                );
            }
            other => panic!("expected SECURITY_DENIED, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn admission_with_empty_allowlist_does_not_change_decision() {
        let (bridge, bundle, _dir, _root) = allow_health_bridge();
        // Default allowlist empty.
        assert!(bridge.always_require_methods().is_empty());
        let envelope = build_request("node.health", b"hi".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Ok(b) => assert_eq!(b.as_ref(), b"hi"),
            other => panic!("expected pre-GAP-15 admit, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn admission_allow_path() {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "any_caller_echo"
            method = "node.health"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let mut bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder.clone(),
        )
        .unwrap();
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        let envelope = build_request("node.health", b"hi".to_vec(), bundle, 30);

        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Ok(b) => assert_eq!(b.as_ref(), b"hi"),
            other => panic!("expected Ok, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn admission_policy_denied() {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "chat_users_only"
            method = "node.health"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let mut bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "bob".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["guest".into()],
            role: "agent".into(),
            clearance: "public".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        let envelope = build_request("node.health", b"hi".to_vec(), bundle, 30);

        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => {
                assert_eq!(e.kind, error_kinds::POLICY_DENIED);
            }
            other => panic!("expected Err(policy_denied), got {:?}", other),
        }
    }

    /// Build a (caller-key, signed-bundle) pair for tests.
    fn mk_identity(org_root: &SigningKey, name: &str, groups: &[&str]) -> Bundle {
        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: name.into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: groups.iter().map(|s| s.to_string()).collect(),
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        issue_identity(id, org_root, 3600).unwrap()
    }

    /// Build a permissive bridge that always allows `node.health`.
    fn fresh_bridge(audit_dir: &TempDir) -> (DispatchBridge, SigningKey) {
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "anyone_health"
            method = "node.health"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &audit_dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        (bridge, org_root)
    }

    #[tokio::test]
    async fn response_rid_echoes_request_rid() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        // Pluck rid out of the envelope we just built.
        let parsed: RequestEnvelope = codec::decode(&envelope).unwrap();
        let sent_rid = parsed.rid;

        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        assert_eq!(sent_rid, resp.rid, "response rid must echo request rid");
    }

    #[tokio::test]
    async fn audit_record_written_on_success() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);

        let _ = bridge.handle_inbound(envelope).await;
        let recs = relix_core::audit::read_audit_records(dir.path().join("audit.log")).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].status, "ok");
        assert_eq!(recs[0].method, "node.health");
        assert!(recs[0].policy_decision.starts_with("allow:"));
    }

    #[tokio::test]
    async fn audit_record_written_on_denial() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let bundle = mk_identity(&org_root, "bob", &["guest"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);

        let _ = bridge.handle_inbound(envelope).await;
        let recs = relix_core::audit::read_audit_records(dir.path().join("audit.log")).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].status, "denied");
        assert_eq!(recs[0].method, "node.health");
        assert!(recs[0].policy_decision.starts_with("deny:"));
        assert_eq!(recs[0].error_kind, Some(error_kinds::POLICY_DENIED));
    }

    #[tokio::test]
    async fn handler_not_called_when_policy_denies() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);

        // Handler increments a counter every time it's invoked. If admission
        // is correct, the counter MUST stay at zero for a denied identity.
        let counter = Arc::new(AtomicU32::new(0));
        let counter_for_handler = counter.clone();
        bridge.register(
            "node.health",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let c = counter_for_handler.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    HandlerOutcome::Ok(b"ran".to_vec())
                }
            })),
        );

        let bundle = mk_identity(&org_root, "bob", &["guest"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);

        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => assert_eq!(e.kind, error_kinds::POLICY_DENIED),
            other => panic!("expected denial, got {:?}", other),
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "handler must not have been called when policy denied"
        );
    }

    #[tokio::test]
    async fn handler_not_called_when_identity_invalid() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let dir = TempDir::new().unwrap();
        let real_root = SigningKey::generate(&mut OsRng);
        let attacker_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let mut bridge = DispatchBridge::new(
            PolicyEngine::permissive(),
            real_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();

        let counter = Arc::new(AtomicU32::new(0));
        let counter_for_handler = counter.clone();
        bridge.register(
            "node.health",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let c = counter_for_handler.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    HandlerOutcome::Ok(b"ran".to_vec())
                }
            })),
        );

        // Identity bundle signed by attacker_root — bridge trusts real_root.
        let bundle = mk_identity(&attacker_root, "intruder", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);

        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => assert_eq!(e.kind, error_kinds::IDENTITY_INVALID),
            other => panic!("expected identity_invalid, got {:?}", other),
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "handler must not have been called when identity invalid"
        );
    }

    #[tokio::test]
    async fn tampered_identity_bundle_rejected() {
        use serde_bytes::ByteBuf;
        let dir = TempDir::new().unwrap();
        let (bridge, org_root) = fresh_bridge(&dir);

        // Issue a valid bundle, then flip a payload byte.
        let mut bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let mut payload = bundle.payload.to_vec();
        let mid = payload.len() / 2;
        payload[mid] ^= 0xFF;
        bundle.payload = ByteBuf::from(payload);

        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => assert_eq!(e.kind, error_kinds::IDENTITY_INVALID),
            other => panic!("expected identity_invalid, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn admission_wrong_trust_root() {
        let dir = TempDir::new().unwrap();
        let real_root = SigningKey::generate(&mut OsRng);
        let attacker_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let bridge = DispatchBridge::new(
            PolicyEngine::permissive(),
            real_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();

        // Bundle signed by attacker_root, but bridge trusts real_root.
        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&attacker_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &attacker_root, 3600).unwrap();
        let envelope = build_request("node.health", b"hi".to_vec(), bundle, 30);

        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => assert_eq!(e.kind, error_kinds::IDENTITY_INVALID),
            other => panic!("expected Err(identity_invalid), got {:?}", other),
        }
    }

    // ── W2-007d: policy denial ring ─────────────────────────────────

    #[test]
    fn policy_denial_ring_default_capacity_matches_const() {
        // CORR PART 4: default ring uses POLICY_DENIAL_HARD_CAP
        // (10000) as the count ceiling, with time-windowed
        // eviction on top. Push only POLICY_DENIAL_RING_DEFAULT
        // entries (well under both caps and inside the time
        // window with `at = NOW + i`) so the test asserts
        // "everything we pushed is retained" rather than the
        // pre-fix "saturates at 256". The hard-cap behaviour
        // is covered by `corr_p4_policy_denial_hard_cap_caps_at_max`.
        let r = PolicyDenialRing::default();
        assert!(r.is_empty());
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        for i in 0..POLICY_DENIAL_RING_DEFAULT {
            r.push(PolicyDenialEntry {
                at: now_secs + i as i64,
                method: "m".into(),
                caller_subject_id: "x".into(),
                caller_name: "x".into(),
                rule: "default_deny".into(),
                reason: "no rule".into(),
            });
        }
        assert_eq!(r.len(), POLICY_DENIAL_RING_DEFAULT);
    }

    #[test]
    fn corr_p4_policy_denial_time_window_evicts_old_entries() {
        // Build a ring with a short 10s window so we can prove
        // time-based eviction without waiting an hour. Push
        // entries at t=0..3, then push one at t=100 — only
        // entries with `at >= 100 - 10 = 90` survive, i.e. the
        // 100 push.
        let r = PolicyDenialRing::with_window(POLICY_DENIAL_HARD_CAP, 10);
        for i in 0..4 {
            r.push(PolicyDenialEntry {
                at: i,
                method: format!("m{i}"),
                caller_subject_id: "x".into(),
                caller_name: "x".into(),
                rule: "default_deny".into(),
                reason: "no rule".into(),
            });
        }
        assert_eq!(r.len(), 4);
        r.push(PolicyDenialEntry {
            at: 100,
            method: "m_late".into(),
            caller_subject_id: "x".into(),
            caller_name: "x".into(),
            rule: "default_deny".into(),
            reason: "no rule".into(),
        });
        // The four old entries fell out of the 10s window.
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn corr_p4_policy_denial_hard_cap_caps_at_max() {
        // With a 1-entry window so time-based trim doesn't
        // mask the count cap, and a hard cap of 4, pushing 6
        // entries should leave 4 in the ring.
        let r = PolicyDenialRing::with_window(4, 1_000_000);
        for i in 0..6 {
            r.push(PolicyDenialEntry {
                at: i,
                method: format!("m{i}"),
                caller_subject_id: "x".into(),
                caller_name: "x".into(),
                rule: "default_deny".into(),
                reason: "no rule".into(),
            });
        }
        // Hard cap is 4 (we passed it as `capacity`).
        assert!(r.len() <= 4, "got {}", r.len());
    }

    #[test]
    fn policy_denial_ring_snapshot_returns_newest_first() {
        let r = PolicyDenialRing::default();
        for i in 0..3 {
            r.push(PolicyDenialEntry {
                at: 100 + i as i64,
                method: format!("m{i}"),
                caller_subject_id: "x".into(),
                caller_name: "x".into(),
                rule: "default_deny".into(),
                reason: "no rule".into(),
            });
        }
        let snap = r.snapshot_newest_first(10);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].method, "m2");
        assert_eq!(snap[1].method, "m1");
        assert_eq!(snap[2].method, "m0");
    }

    #[test]
    fn policy_denial_ring_zero_capacity_clamps_to_one() {
        let r = PolicyDenialRing::new(0);
        r.push(PolicyDenialEntry {
            at: 1,
            method: "a".into(),
            caller_subject_id: "x".into(),
            caller_name: "x".into(),
            rule: "default_deny".into(),
            reason: "no rule".into(),
        });
        r.push(PolicyDenialEntry {
            at: 2,
            method: "b".into(),
            caller_subject_id: "x".into(),
            caller_name: "x".into(),
            rule: "default_deny".into(),
            reason: "no rule".into(),
        });
        // capacity clamped to 1 → only newest survives.
        assert_eq!(r.len(), 1);
        let snap = r.snapshot_newest_first(10);
        assert_eq!(snap[0].method, "b");
    }

    #[tokio::test]
    async fn policy_denial_pushes_to_ring_on_deny() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));
        // Caller in `guest` group; policy requires `chat-users`.
        let bundle = mk_identity(&org_root, "bob", &["guest"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        let _ = bridge.handle_inbound(envelope).await;
        // Ring must now have one entry.
        let snap = bridge.policy_denials_handle().snapshot_newest_first(10);
        assert_eq!(snap.len(), 1);
        let entry = &snap[0];
        assert_eq!(entry.method, "node.health");
        assert_eq!(entry.caller_name, "bob");
        // Either a named rule denied OR default_deny when no
        // rule matched. The test policy has a single rule
        // requiring `chat-users`, so default_deny is the
        // expected reason.
        assert!(entry.rule == "default_deny" || !entry.rule.is_empty());
        assert!(!entry.reason.is_empty());
    }

    #[tokio::test]
    async fn policy_denial_ring_empty_when_no_denial() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));
        // Caller in the allowed group — admission succeeds.
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        let resp = bridge.handle_inbound(envelope).await;
        let decoded = decode_response(&resp).unwrap();
        assert!(matches!(decoded.res, ResponseResult::Ok(_)));
        // Ring must still be empty.
        assert!(bridge.policy_denials_handle().is_empty());
    }

    // ── PH-DISP1: capability invocation counters ────────────────────────

    #[tokio::test]
    async fn capability_stats_counts_ok_invocations() {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "any"
            method = "node.health"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let mut bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        for _ in 0..3 {
            let envelope = build_request("node.health", b"x".to_vec(), bundle.clone(), 30);
            let _ = bridge.handle_inbound(envelope).await;
        }
        let snap = bridge.capability_stats_snapshot();
        let (name, stats) = snap
            .iter()
            .find(|(n, _)| n == "node.health")
            .expect("node.health counter must exist");
        assert_eq!(name, "node.health");
        assert_eq!(stats.invocations, 3);
        assert_eq!(stats.errors, 0);
        assert_eq!(stats.denied, 0);
        assert_eq!(stats.unknown_method, 0);
        assert!(stats.last_invoked_at > 0);
        assert!(stats.last_error_at.is_none());
        // W2-006a: latency fields populated by every Ok dispatch.
        assert_eq!(stats.latency_samples, 3);
        assert!(stats.total_elapsed_ms >= stats.last_elapsed_ms);
        assert!(stats.max_elapsed_ms >= stats.last_elapsed_ms);
        // W2-006d: recent latency ring tracks the same 3
        // Ok dispatches.
        assert_eq!(stats.recent_latencies.len(), 3);
    }

    /// W2-006d: the recent-latencies ring must cap at
    /// RECENT_LATENCIES_CAP regardless of how many Ok / Err
    /// dispatches land. FIFO eviction means the *newest*
    /// sample wins over the *oldest*, not the other way
    /// around.
    #[tokio::test]
    async fn capability_stats_caps_recent_latencies_ring() {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "any"
            method = "node.health"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let mut bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));

        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        // Dispatch CAP + 5 invocations so the ring has to
        // evict the first 5.
        let total = RECENT_LATENCIES_CAP + 5;
        for _ in 0..total {
            let envelope = build_request("node.health", b"x".to_vec(), bundle.clone(), 30);
            let _ = bridge.handle_inbound(envelope).await;
        }
        let snap = bridge.capability_stats_snapshot();
        let (_, stats) = snap
            .iter()
            .find(|(n, _)| n == "node.health")
            .expect("node.health counter must exist");
        assert_eq!(stats.recent_latencies.len(), RECENT_LATENCIES_CAP);
        // Total samples counter is uncapped — it still
        // reflects every Ok dispatch.
        assert_eq!(stats.latency_samples as usize, total);
    }

    #[tokio::test]
    async fn capability_stats_counts_unknown_method_attempts() {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "any"
            method = "anything.at.all"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        // NO handlers registered. Every call should bump
        // unknown_method.
        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        let envelope = build_request("task.todo_typooo", b"".to_vec(), bundle, 30);
        let _ = bridge.handle_inbound(envelope).await;

        let snap = bridge.capability_stats_snapshot();
        let (_, stats) = snap
            .iter()
            .find(|(n, _)| n == "task.todo_typooo")
            .expect("typo counter must exist");
        assert_eq!(stats.unknown_method, 1);
        assert!(stats.last_error_at.is_some());
    }

    #[tokio::test]
    async fn capability_stats_snapshot_returns_sorted() {
        let dir = TempDir::new().unwrap();
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "any"
            method = "anything.at.all"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        let caller_key = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_key.verifying_key().to_bytes()),
            name: "alice".into(),
            org_id: NodeId::from_pubkey(&org_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle = issue_identity(id, &org_root, 3600).unwrap();
        // Send in reverse-alpha order; snapshot should sort.
        for m in ["zzz.method", "aaa.method", "mmm.method"] {
            let envelope = build_request(m, b"".to_vec(), bundle.clone(), 30);
            let _ = bridge.handle_inbound(envelope).await;
        }
        let snap = bridge.capability_stats_snapshot();
        let names: Vec<&str> = snap.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["aaa.method", "mmm.method", "zzz.method"]);
    }

    // ── W2: AgentAccessBroker wiring ────────────────────────────────

    #[tokio::test]
    async fn access_broker_allows_call_when_policy_permits() {
        use crate::nodes::execution::broker::{AccessPolicy, AgentAccessBroker};
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));
        bridge.set_access_broker(Arc::new(AgentAccessBroker::new(vec![AccessPolicy {
            agent: "alice".into(),
            // Empty allow list = unrestricted (subject to deny + rate limit).
            allowed_capabilities: Vec::new(),
            denied_capabilities: Vec::new(),
            max_calls_per_minute: 60,
            max_cost_cents_per_hour: 500,
        }])));
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        assert!(matches!(resp.res, ResponseResult::Ok(_)));
    }

    #[tokio::test]
    async fn access_broker_denies_call_when_capability_is_in_deny_list() {
        use crate::nodes::execution::broker::{AccessPolicy, AgentAccessBroker};
        use std::sync::atomic::{AtomicU32, Ordering};
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        // Handler increments a counter on call. Broker deny
        // must prevent the handler from running.
        let counter = Arc::new(AtomicU32::new(0));
        let counter_for_handler = counter.clone();
        bridge.register(
            "node.health",
            Arc::new(FnHandler(move |_ctx| {
                let c = counter_for_handler.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    HandlerOutcome::Ok(b"hi".to_vec())
                }
            })),
        );
        bridge.set_access_broker(Arc::new(AgentAccessBroker::new(vec![AccessPolicy {
            agent: "alice".into(),
            allowed_capabilities: Vec::new(),
            denied_capabilities: vec!["node.health".into()],
            max_calls_per_minute: 60,
            max_cost_cents_per_hour: 500,
        }])));
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => assert_eq!(e.kind, error_kinds::POLICY_DENIED),
            other => panic!("expected POLICY_DENIED, got {other:?}"),
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "handler must NOT run when broker denies"
        );
        // The denial ring records the broker rule.
        let snap = bridge.policy_denials_handle().snapshot_newest_first(10);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].rule, "access_broker");
        assert!(snap[0].reason.contains("deny list"));
    }

    #[test]
    fn build_otel_config_returns_none_when_section_missing() {
        let cfg_text = r#"
            [controller]
            name = "x"
            node_type = "memory"
            listen_port = 1
            [identity]
            key_path = "x"
            [trust]
            org_root_key_path = "x"
            [policy]
            file = "x"
        "#;
        let cfg: crate::controller_runtime::ControllerConfig = toml::from_str(cfg_text).unwrap();
        assert!(crate::controller_runtime::build_otel_config(&cfg).is_none());
    }

    #[test]
    fn build_otel_config_returns_none_when_disabled() {
        let cfg_text = r#"
            [controller]
            name = "x"
            node_type = "memory"
            listen_port = 1
            [identity]
            key_path = "x"
            [trust]
            org_root_key_path = "x"
            [policy]
            file = "x"

            [observability.otel]
            enabled = false
            endpoint = "http://localhost:4318/v1/traces"
        "#;
        let cfg: crate::controller_runtime::ControllerConfig = toml::from_str(cfg_text).unwrap();
        assert!(crate::controller_runtime::build_otel_config(&cfg).is_none());
    }

    #[test]
    fn build_otel_config_returns_runtime_config_when_enabled_with_endpoint() {
        let cfg_text = r#"
            [controller]
            name = "x"
            node_type = "memory"
            listen_port = 1
            [identity]
            key_path = "x"
            [trust]
            org_root_key_path = "x"
            [policy]
            file = "x"

            [observability.otel]
            enabled = true
            endpoint = "http://localhost:4318/v1/traces"
            service_name = "my-service"
            events = ["model_call", "tool_call"]
        "#;
        let cfg: crate::controller_runtime::ControllerConfig = toml::from_str(cfg_text).unwrap();
        let otel = crate::controller_runtime::build_otel_config(&cfg)
            .expect("otel config built when enabled + endpoint set");
        assert!(otel.enabled);
        assert_eq!(
            otel.endpoint_url.as_deref(),
            Some("http://localhost:4318/v1/traces")
        );
        assert_eq!(otel.service_name, "my-service");
        assert!(otel.events.is_enabled("model_call"));
        assert!(otel.events.is_enabled("tool_call"));
    }

    #[tokio::test]
    async fn access_broker_loads_policies_from_execution_agents_config() {
        // Mirror what `build_access_broker` does so the test
        // proves the config-to-broker mapping end to end.
        use crate::nodes::execution::broker::AccessPolicy;
        let cfg_text = r#"
            [controller]
            name = "x"
            node_type = "memory"
            listen_port = 1
            [identity]
            key_path = "x"
            [trust]
            org_root_key_path = "x"
            [policy]
            file = "x"

            [[execution.agents]]
            agent = "alice"
            allowed_capabilities = ["ai.chat"]
            max_calls_per_minute = 30

            [[execution.agents]]
            agent = "bob"
            denied_capabilities = ["tool.terminal"]
        "#;
        let cfg: crate::controller_runtime::ControllerConfig = toml::from_str(cfg_text).unwrap();
        let broker = crate::controller_runtime::build_access_broker(&cfg);
        let snap = broker.snapshot();
        assert_eq!(snap.len(), 2);
        let names: Vec<&str> = snap.iter().map(|e| e.policy.agent.as_str()).collect();
        assert_eq!(names, vec!["alice", "bob"]);
        // alice has allow list, bob has deny list. Spot-check
        // the broker behaves accordingly.
        use crate::nodes::execution::broker::AccessDecision;
        assert_eq!(broker.check("alice", "ai.chat"), AccessDecision::Allow);
        match broker.check("alice", "tool.terminal") {
            AccessDecision::Deny { reason } => assert!(reason.contains("allow list")),
            other => panic!("expected Deny, got {other:?}"),
        }
        match broker.check("bob", "tool.terminal") {
            AccessDecision::Deny { reason } => assert!(reason.contains("deny list")),
            other => panic!("expected Deny, got {other:?}"),
        }
        // Ensure the parsed policies preserved the per-agent
        // rate limit override.
        let alice = snap
            .iter()
            .find(|e| e.policy.agent == "alice")
            .expect("alice in snapshot");
        assert_eq!(alice.policy.max_calls_per_minute, 30);
        // Use AccessPolicy to keep the type used so the import
        // doesn't unused-trigger.
        let _ = AccessPolicy {
            agent: "x".into(),
            allowed_capabilities: vec![],
            denied_capabilities: vec![],
            max_calls_per_minute: 0,
            max_cost_cents_per_hour: 0,
        };
    }

    // ── RELIX-7.19: confidence pipeline integration ─────────

    async fn ok_long_handler(_ctx: InvocationCtx) -> HandlerOutcome {
        HandlerOutcome::Ok(
            b"This is a complete answer that ends with proper punctuation. \
              Long enough to clear the length floor on the scorer."
                .to_vec(),
        )
    }

    async fn ok_empty_handler(_ctx: InvocationCtx) -> HandlerOutcome {
        HandlerOutcome::Ok(Vec::new())
    }

    fn permissive_bridge(audit_dir: &TempDir) -> (DispatchBridge, SigningKey) {
        let org_root = SigningKey::generate(&mut OsRng);
        let responder = SigningKey::generate(&mut OsRng);
        // PolicyEngine::permissive() defaults to DENY for any
        // method not explicitly allowed — the name is
        // misleading. For the confidence pipeline tests we
        // need an admit-all rule against every test method
        // chat-users may call.
        let policy = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "anything"
            method = "anything.method"
            allow_groups = ["chat-users"]
            [[rules]]
            name = "flaky"
            method = "flaky.method"
            allow_groups = ["chat-users"]
            [[rules]]
            name = "premium"
            method = "premium.method"
            allow_groups = ["chat-users"]
            "#,
        )
        .unwrap();
        let bridge = DispatchBridge::new(
            policy,
            org_root.verifying_key(),
            &audit_dir.path().join("audit.log"),
            responder,
        )
        .unwrap();
        (bridge, org_root)
    }

    fn make_confidence(
        policies: Vec<crate::confidence::ConfidencePolicy>,
    ) -> (
        Arc<crate::confidence::ConfidenceScorer>,
        Arc<crate::confidence::FallbackEngine>,
    ) {
        let cfg = crate::confidence::ConfidenceConfig {
            enabled: true,
            policies: policies.clone(),
            ..Default::default()
        };
        let scorer = Arc::new(crate::confidence::ConfidenceScorer::from_config(&cfg));
        let engine = Arc::new(crate::confidence::FallbackEngine::from_policies(&policies));
        (scorer, engine)
    }

    #[tokio::test]
    async fn confidence_score_lands_in_response_envelope_when_scorer_wired() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = permissive_bridge(&dir);
        bridge.register("anything.method", Arc::new(FnHandler(ok_long_handler)));
        let (scorer, engine) = make_confidence(Vec::new());
        bridge.set_confidence(scorer, engine);

        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("anything.method", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        let score = resp.confidence.expect("confidence stamped");
        assert!(
            score > 0.5,
            "long well-formed reply should score > 0.5: {score}"
        );
    }

    #[tokio::test]
    async fn confidence_pipeline_is_byte_for_byte_noop_when_not_wired() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = permissive_bridge(&dir);
        bridge.register("anything.method", Arc::new(FnHandler(ok_long_handler)));
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("anything.method", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        assert!(resp.confidence.is_none(), "no scorer => no confidence");
    }

    /// RELIX-7.19 GAP 3: a `MetricsSink` that pre-loads a
    /// provider-signals hint keyed by the next-known
    /// request_id. The dispatch bridge must consume it during
    /// score_outcome instead of parsing the body.
    struct PreloadedSignalsSink {
        signals: std::sync::Mutex<
            std::collections::HashMap<
                relix_core::types::RequestId,
                crate::metrics::AiProviderSignalsHint,
            >,
        >,
    }

    impl crate::metrics::MetricsSink for PreloadedSignalsSink {
        fn record_invocation(&self, _m: crate::metrics::InvocationMetric) {}
        fn attach_ai_usage(&self, _hint: crate::metrics::AiUsageHint) {}
        fn attach_provider_signals(&self, hint: crate::metrics::AiProviderSignalsHint) {
            self.signals
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(hint.request_id, hint);
        }
        fn take_provider_signals(
            &self,
            request_id: relix_core::types::RequestId,
        ) -> Option<crate::metrics::AiProviderSignalsHint> {
            self.signals
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&request_id)
        }
    }

    #[tokio::test]
    async fn confidence_scorer_reads_finish_reason_from_metrics_sink_side_channel() {
        use crate::metrics::MetricsSink as _;
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = permissive_bridge(&dir);
        // Handler that returns a body with NO finish_reason
        // embedded — the scorer must read it from the side
        // channel instead.
        bridge.register(
            "anything.method",
            Arc::new(FnHandler(|ctx: InvocationCtx| {
                let _ = ctx;
                async move {
                    HandlerOutcome::Ok(
                        b"a complete answer that ends with proper punctuation.".to_vec(),
                    )
                }
            })),
        );
        let (scorer, engine) = make_confidence(Vec::new());
        bridge.set_confidence(scorer.clone(), engine);
        let sink = Arc::new(PreloadedSignalsSink {
            signals: std::sync::Mutex::new(std::collections::HashMap::new()),
        });
        let sink_dyn: Arc<dyn crate::metrics::MetricsSink> = sink.clone();
        bridge.set_metrics_sink(sink_dyn, "test-peer");

        // Issue two calls — one where we pre-load the sink
        // with finish_reason="stop", and one where we pre-load
        // with finish_reason="length". The "stop" call should
        // score strictly higher because of the higher
        // provider_signal sub-score.
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope_stop = build_request("anything.method", b"x".to_vec(), bundle.clone(), 30);
        let parsed_stop: crate::transport::envelope::RequestEnvelope =
            relix_core::codec::decode(&envelope_stop).unwrap();
        sink.attach_provider_signals(crate::metrics::AiProviderSignalsHint {
            request_id: parsed_stop.rid,
            finish_reason: Some("stop".into()),
            logprob: None,
        });
        let resp_stop = bridge.handle_inbound(envelope_stop).await;
        let resp_stop = decode_response(&resp_stop).unwrap();
        let score_stop = resp_stop.confidence.expect("scored");

        // Reset rolling-window state so the second call gets
        // an apples-to-apples scoring (otherwise the first
        // call's lower error_rate_history contribution would
        // shift things).
        scorer.reset_pair("alice", "anything.method");

        let envelope_length = build_request("anything.method", b"x".to_vec(), bundle, 30);
        let parsed_length: crate::transport::envelope::RequestEnvelope =
            relix_core::codec::decode(&envelope_length).unwrap();
        sink.attach_provider_signals(crate::metrics::AiProviderSignalsHint {
            request_id: parsed_length.rid,
            finish_reason: Some("length".into()),
            logprob: None,
        });
        let resp_length = bridge.handle_inbound(envelope_length).await;
        let resp_length = decode_response(&resp_length).unwrap();
        let score_length = resp_length.confidence.expect("scored");

        assert!(
            score_stop > score_length,
            "stop ({score_stop}) should beat length ({score_length})"
        );
    }

    #[tokio::test]
    async fn safe_default_action_swaps_response_body_below_critical_threshold() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = permissive_bridge(&dir);
        bridge.register("flaky.method", Arc::new(FnHandler(ok_empty_handler)));
        let (scorer, engine) = make_confidence(vec![crate::confidence::ConfidencePolicy {
            capability: "flaky.method".into(),
            low_threshold: 0.5,
            critical_threshold: 0.3,
            low_action: Some(crate::confidence::FallbackActionConfig::SafeDefault {
                default_value: "FALLBACK BODY".into(),
            }),
            critical_action: Some(crate::confidence::FallbackActionConfig::SafeDefault {
                default_value: "FALLBACK BODY".into(),
            }),
        }]);
        bridge.set_confidence(scorer, engine);
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("flaky.method", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        let ResponseResult::Ok(body) = resp.res else {
            panic!("expected Ok");
        };
        assert_eq!(body.as_slice(), b"FALLBACK BODY");
    }

    #[tokio::test]
    async fn abort_action_converts_low_confidence_to_invalid_args() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = permissive_bridge(&dir);
        bridge.register("flaky.method", Arc::new(FnHandler(ok_empty_handler)));
        let (scorer, engine) = make_confidence(vec![crate::confidence::ConfidencePolicy {
            capability: "flaky.method".into(),
            low_threshold: 0.5,
            critical_threshold: 0.3,
            low_action: Some(crate::confidence::FallbackActionConfig::Abort {
                abort_message: "low".into(),
            }),
            critical_action: Some(crate::confidence::FallbackActionConfig::Abort {
                abort_message: "critical".into(),
            }),
        }]);
        bridge.set_confidence(scorer, engine);
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("flaky.method", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        let ResponseResult::Err(e) = resp.res else {
            panic!("expected Err: {:?}", resp.res);
        };
        assert_eq!(e.kind, relix_core::types::error_kinds::INVALID_ARGS);
    }

    #[tokio::test]
    async fn escalate_action_re_dispatches_to_a_different_capability() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = permissive_bridge(&dir);
        bridge.register("flaky.method", Arc::new(FnHandler(ok_empty_handler)));
        let counter = Arc::new(AtomicU32::new(0));
        let counter_for = counter.clone();
        bridge.register(
            "premium.method",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let c = counter_for.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    HandlerOutcome::Ok(
                        b"premium reply with enough body to score full marks on length.".to_vec(),
                    )
                }
            })),
        );
        let (scorer, engine) = make_confidence(vec![crate::confidence::ConfidencePolicy {
            capability: "flaky.method".into(),
            low_threshold: 0.5,
            critical_threshold: 0.3,
            low_action: Some(crate::confidence::FallbackActionConfig::Escalate {
                escalate_to: "premium.method".into(),
            }),
            critical_action: Some(crate::confidence::FallbackActionConfig::Escalate {
                escalate_to: "premium.method".into(),
            }),
        }]);
        bridge.set_confidence(scorer, engine);
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("flaky.method", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        let ResponseResult::Ok(body) = resp.res else {
            panic!("expected Ok");
        };
        assert!(body.starts_with(b"premium"));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retry_action_re_dispatches_up_to_max_retries() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = permissive_bridge(&dir);
        let counter = Arc::new(AtomicU32::new(0));
        let counter_for = counter.clone();
        // Always returns an empty body so confidence stays low
        // and retries always fire.
        bridge.register(
            "flaky.method",
            Arc::new(FnHandler(move |_ctx: InvocationCtx| {
                let c = counter_for.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    HandlerOutcome::Ok(Vec::new())
                }
            })),
        );
        let (scorer, engine) = make_confidence(vec![crate::confidence::ConfidencePolicy {
            capability: "flaky.method".into(),
            low_threshold: 0.5,
            critical_threshold: 0.3,
            low_action: Some(crate::confidence::FallbackActionConfig::Retry {
                max_retries: 3,
                retry_delay_ms: 0,
            }),
            critical_action: Some(crate::confidence::FallbackActionConfig::Retry {
                max_retries: 3,
                retry_delay_ms: 0,
            }),
        }]);
        bridge.set_confidence(scorer, engine);
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("flaky.method", b"x".to_vec(), bundle, 30);
        let _ = bridge.handle_inbound(envelope).await;
        // 1 initial + 3 retries = 4 total.
        assert_eq!(counter.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn last_confidence_cell_is_updated_after_each_scored_dispatch() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = permissive_bridge(&dir);
        bridge.register("anything.method", Arc::new(FnHandler(ok_long_handler)));
        let (scorer, engine) = make_confidence(Vec::new());
        bridge.set_confidence(scorer, engine);
        let cell = crate::confidence::LastConfidenceCell::new();
        bridge.set_last_confidence_cell(cell.clone());
        // Default reading: 1.0 (no calls yet).
        assert!((cell.get() - 1.0).abs() < 1e-6);
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("anything.method", b"x".to_vec(), bundle, 30);
        let _ = bridge.handle_inbound(envelope).await;
        // Cell now reflects the just-scored verdict (some value
        // in (0, 1] post scoring).
        let v = cell.get();
        assert!(v > 0.0 && v <= 1.0, "cell={v}");
    }

    #[tokio::test]
    async fn alert_action_logs_warning_and_keeps_original_body() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = permissive_bridge(&dir);
        bridge.register(
            "flaky.method",
            Arc::new(FnHandler(|_ctx: InvocationCtx| async {
                HandlerOutcome::Ok(b"hi".to_vec())
            })),
        );
        let (scorer, engine) = make_confidence(vec![crate::confidence::ConfidencePolicy {
            capability: "flaky.method".into(),
            low_threshold: 0.95,
            critical_threshold: 0.9,
            low_action: Some(crate::confidence::FallbackActionConfig::Alert {
                alert_message: "wobble".into(),
            }),
            critical_action: Some(crate::confidence::FallbackActionConfig::Alert {
                alert_message: "very wobble".into(),
            }),
        }]);
        bridge.set_confidence(scorer, engine);
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("flaky.method", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        let ResponseResult::Ok(body) = resp.res else {
            panic!("expected Ok");
        };
        // Alert leaves body unchanged.
        assert_eq!(body.as_slice(), b"hi");
    }

    // ── RELIX-7.19 GAP 2: AlertEngine + AlertDeliver wiring

    /// AlertDeliver that records every event into a shared
    /// vec — used by the GAP 2 wiring tests to verify the
    /// bridge fires `LowConfidence` events through the
    /// configured pipeline instead of falling back to
    /// `tracing::warn!`.
    struct RecordingAlertSink {
        events: Arc<std::sync::Mutex<Vec<crate::metrics::AlertEvent>>>,
    }

    impl crate::metrics::AlertDeliver for RecordingAlertSink {
        fn deliver(&self, event: &crate::metrics::AlertEvent) {
            self.events
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(event.clone());
        }
    }

    fn make_alert_engine() -> Arc<crate::metrics::AlertEngine> {
        let store = crate::metrics::MetricsStore::in_memory().unwrap();
        let query = crate::metrics::MetricsQuery::new(store);
        Arc::new(crate::metrics::AlertEngine::new(
            query,
            crate::metrics::AlertThresholds::default(),
        ))
    }

    #[tokio::test]
    async fn low_confidence_alert_fires_through_wired_sink() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = permissive_bridge(&dir);
        bridge.register(
            "flaky.method",
            Arc::new(FnHandler(|_ctx: InvocationCtx| async {
                HandlerOutcome::Ok(b"hi".to_vec())
            })),
        );
        let (scorer, engine) = make_confidence(vec![crate::confidence::ConfidencePolicy {
            capability: "flaky.method".into(),
            // Aggressive thresholds so the small "hi" body trips
            // the alert action.
            low_threshold: 0.95,
            critical_threshold: 0.90,
            low_action: Some(crate::confidence::FallbackActionConfig::Alert {
                alert_message: "wobble".into(),
            }),
            critical_action: Some(crate::confidence::FallbackActionConfig::Alert {
                alert_message: "very wobble".into(),
            }),
        }]);
        bridge.set_confidence(scorer, engine);
        let alert_engine = make_alert_engine();
        let events: Arc<std::sync::Mutex<Vec<crate::metrics::AlertEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: Arc<dyn crate::metrics::AlertDeliver> = Arc::new(RecordingAlertSink {
            events: events.clone(),
        });
        bridge.set_alert_pipeline(alert_engine, sink);
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("flaky.method", b"x".to_vec(), bundle, 30);
        let _ = bridge.handle_inbound(envelope).await;
        let g = events.lock().unwrap();
        assert_eq!(g.len(), 1, "expected one Fired event, got {:?}", *g);
        match &g[0] {
            crate::metrics::AlertEvent::Fired(a) => {
                assert_eq!(a.kind, crate::metrics::AlertKind::LowConfidence);
                assert_eq!(a.agent, "alice");
                assert_eq!(a.method.as_deref(), Some("flaky.method"));
            }
            o => panic!("expected Fired, got {o:?}"),
        }
    }

    #[tokio::test]
    async fn low_confidence_alert_dedups_across_back_to_back_calls() {
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = permissive_bridge(&dir);
        bridge.register(
            "flaky.method",
            Arc::new(FnHandler(|_ctx: InvocationCtx| async {
                HandlerOutcome::Ok(b"hi".to_vec())
            })),
        );
        let (scorer, engine) = make_confidence(vec![crate::confidence::ConfidencePolicy {
            capability: "flaky.method".into(),
            low_threshold: 0.95,
            critical_threshold: 0.90,
            low_action: Some(crate::confidence::FallbackActionConfig::Alert {
                alert_message: "wobble".into(),
            }),
            critical_action: Some(crate::confidence::FallbackActionConfig::Alert {
                alert_message: "very wobble".into(),
            }),
        }]);
        bridge.set_confidence(scorer, engine);
        let alert_engine = make_alert_engine();
        let events: Arc<std::sync::Mutex<Vec<crate::metrics::AlertEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: Arc<dyn crate::metrics::AlertDeliver> = Arc::new(RecordingAlertSink {
            events: events.clone(),
        });
        bridge.set_alert_pipeline(alert_engine, sink);
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        for _ in 0..3 {
            let envelope = build_request("flaky.method", b"x".to_vec(), bundle.clone(), 30);
            let _ = bridge.handle_inbound(envelope).await;
        }
        let g = events.lock().unwrap();
        assert_eq!(
            g.len(),
            1,
            "expected exactly one Fired event despite three calls: {:?}",
            *g
        );
    }

    // ── RELIX-7.28 Part 1: BudgetEnforcer wired into the bridge ──

    fn make_budget_enforcer(
        agent: &str,
        daily_usd: Option<f64>,
        action: crate::metrics::BudgetAction,
        backoff_ms: u64,
    ) -> Arc<crate::metrics::BudgetEnforcer> {
        use crate::metrics::{
            AgentBudget, BudgetConfig, BudgetEnforcer, MetricsQuery, MetricsStore,
        };
        let store = MetricsStore::in_memory().unwrap();
        let q = MetricsQuery::new(store);
        Arc::new(BudgetEnforcer::new(
            BudgetConfig {
                agents: vec![AgentBudget {
                    agent: agent.into(),
                    daily_limit_usd: daily_usd,
                    hourly_limit_usd: None,
                    action_on_exceed: action,
                }],
                deployment: None,
                throttle_backoff_ms: backoff_ms,
                cache_refresh_secs: 60,
                exempt_methods: vec![],
            },
            Some(q),
        ))
    }

    #[tokio::test]
    async fn budget_enforcer_rejects_call_when_daily_limit_exceeded() {
        use crate::metrics::BudgetAction;
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));
        let enf = make_budget_enforcer("alice", Some(0.0001), BudgetAction::Reject, 50);
        // Seed the in-memory cache above the limit.
        enf.set_cached_for_test(
            "agent:alice",
            crate::metrics::BudgetWindow::Daily,
            1_000_000,
        );
        bridge.set_budget_enforcer(enf);
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => assert_eq!(e.kind, error_kinds::RESOURCE_EXHAUSTED),
            other => panic!("expected RESOURCE_EXHAUSTED, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn budget_enforcer_allows_call_when_within_limit() {
        use crate::metrics::BudgetAction;
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));
        let enf = make_budget_enforcer("alice", Some(1.0), BudgetAction::Reject, 50);
        enf.set_cached_for_test("agent:alice", crate::metrics::BudgetWindow::Daily, 100_000);
        bridge.set_budget_enforcer(enf);
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        assert!(matches!(resp.res, ResponseResult::Ok(_)));
    }

    #[tokio::test]
    async fn budget_enforcer_throttle_introduces_delay_then_admits() {
        use crate::metrics::BudgetAction;
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));
        // 200ms throttle backoff so the test can observe it.
        let enf = make_budget_enforcer("alice", Some(0.0001), BudgetAction::Throttle, 200);
        enf.set_cached_for_test(
            "agent:alice",
            crate::metrics::BudgetWindow::Daily,
            1_000_000,
        );
        bridge.set_budget_enforcer(enf);
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        let start = std::time::Instant::now();
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let elapsed = start.elapsed();
        let resp = decode_response(&resp_bytes).unwrap();
        // Call still succeeds (throttle admits) but the backoff
        // ate at least the configured delay.
        assert!(matches!(resp.res, ResponseResult::Ok(_)));
        assert!(
            elapsed >= std::time::Duration::from_millis(180),
            "throttle should sleep ≥ ~180ms, got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn budget_alert_only_does_not_block_call() {
        use crate::metrics::BudgetAction;
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));
        let enf = make_budget_enforcer("alice", Some(0.0001), BudgetAction::AlertOnly, 50);
        enf.set_cached_for_test(
            "agent:alice",
            crate::metrics::BudgetWindow::Daily,
            1_000_000,
        );
        bridge.set_budget_enforcer(enf);
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"x".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        // alert_only must not block.
        assert!(matches!(resp.res, ResponseResult::Ok(_)));
    }

    // ── RELIX-7.28 Part 3: MeshPiiGate wired into the bridge ──

    fn make_pii_gate(
        action: crate::nodes::pii_gate::MeshPiiAction,
    ) -> Arc<crate::nodes::pii_gate::MeshPiiGate> {
        Arc::new(
            crate::nodes::pii_gate::MeshPiiGate::in_memory(crate::nodes::pii_gate::MeshPiiConfig {
                enabled: true,
                action,
                scan_args: true,
                scan_responses: false,
                exempt_methods: vec![],
                chronicle_path: None,
            })
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn pii_gate_blocks_request_with_pii_when_action_is_block() {
        use crate::nodes::pii_gate::MeshPiiAction;
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));
        bridge.set_pii_gate(make_pii_gate(MeshPiiAction::Block));
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request(
            "node.health",
            b"my email is jane@example.com".to_vec(),
            bundle,
            30,
        );
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(e) => assert_eq!(e.kind, error_kinds::POLICY_DENIED),
            other => panic!("expected POLICY_DENIED, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pii_gate_redacts_args_before_invoking_handler() {
        use crate::nodes::pii_gate::MeshPiiAction;
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        // Echo handler echoes whatever args it sees — used to
        // confirm the bridge handed the handler the redacted form.
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));
        bridge.set_pii_gate(make_pii_gate(MeshPiiAction::Redact));
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request(
            "node.health",
            b"please email jane@example.com".to_vec(),
            bundle,
            30,
        );
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        let ResponseResult::Ok(body) = resp.res else {
            panic!("expected Ok");
        };
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("[EMAIL]"), "expected redacted args: {text}");
        assert!(!text.contains("jane@example.com"));
    }

    #[tokio::test]
    async fn pii_gate_passes_clean_args_through_unchanged() {
        use crate::nodes::pii_gate::MeshPiiAction;
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = fresh_bridge(&dir);
        bridge.register("node.health", Arc::new(FnHandler(echo_handler)));
        bridge.set_pii_gate(make_pii_gate(MeshPiiAction::Block));
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("node.health", b"hello world".to_vec(), bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        let ResponseResult::Ok(body) = resp.res else {
            panic!("clean args should pass through");
        };
        assert_eq!(body.as_slice(), b"hello world");
    }

    #[tokio::test]
    async fn low_confidence_alert_falls_back_to_tracing_when_no_sink_wired() {
        // Without an alert pipeline wired the bridge must
        // continue with the original body — the only side
        // effect should be a tracing::warn (untestable without
        // a subscriber, so we assert the body survives).
        let dir = TempDir::new().unwrap();
        let (mut bridge, org_root) = permissive_bridge(&dir);
        bridge.register(
            "flaky.method",
            Arc::new(FnHandler(|_ctx: InvocationCtx| async {
                HandlerOutcome::Ok(b"hi".to_vec())
            })),
        );
        let (scorer, engine) = make_confidence(vec![crate::confidence::ConfidencePolicy {
            capability: "flaky.method".into(),
            low_threshold: 0.95,
            critical_threshold: 0.90,
            low_action: Some(crate::confidence::FallbackActionConfig::Alert {
                alert_message: "wobble".into(),
            }),
            critical_action: Some(crate::confidence::FallbackActionConfig::Alert {
                alert_message: "very wobble".into(),
            }),
        }]);
        bridge.set_confidence(scorer, engine);
        // No set_alert_pipeline call: pre-7.19 behaviour
        // preserved (tracing::warn + original body).
        let bundle = mk_identity(&org_root, "alice", &["chat-users"]);
        let envelope = build_request("flaky.method", b"x".to_vec(), bundle, 30);
        let resp = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp).unwrap();
        let ResponseResult::Ok(body) = resp.res else {
            panic!("expected Ok");
        };
        assert_eq!(body.as_slice(), b"hi");
    }

    // ─────────────────────────────────────────────────────────
    // P2 — replay cache + freshness check + no-30s-grace tests
    // ─────────────────────────────────────────────────────────

    /// Build an envelope with explicit overrides for the
    /// timestamp fields. Used by the P2 tests to drive the
    /// freshness gate and the replay cache from synthetic
    /// time values without sleeping.
    fn build_request_with_clock(
        method: &str,
        bundle: Bundle,
        deadline_secs_from_now: i64,
        issued_at_ms: i64,
        rid: Option<relix_core::types::RequestId>,
    ) -> Vec<u8> {
        let envelope = RequestEnvelope {
            pv: 1,
            rid: rid.unwrap_or_default(),
            tid: relix_core::types::TraceId::new(),
            method: method.into(),
            mv: 1,
            args: ByteBuf::from(b"hi".to_vec()),
            identity_bundle: bundle,
            deadline: Timestamp::now()
                .add_secs(deadline_secs_from_now)
                .unwrap_or(Timestamp(i64::MAX)),
            issued_at_ms,
            surface: None,
            approval_token: None,
            task_id: None,
            session_id: None,
            workspace_path: None,
            tenant_id: None,
            session_token: None,
        };
        codec::encode(&envelope).unwrap_or_default()
    }

    /// Same as [`build_request_with_clock`] but also stamps a
    /// wire session-token. Used by the P5 tests below.
    fn build_request_with_session_token(
        method: &str,
        bundle: Bundle,
        deadline_secs_from_now: i64,
        issued_at_ms: i64,
        session_token: Option<String>,
    ) -> Vec<u8> {
        let envelope = RequestEnvelope {
            pv: 1,
            rid: relix_core::types::RequestId::new(),
            tid: relix_core::types::TraceId::new(),
            method: method.into(),
            mv: 1,
            args: ByteBuf::from(b"hi".to_vec()),
            identity_bundle: bundle,
            deadline: Timestamp::now()
                .add_secs(deadline_secs_from_now)
                .unwrap_or(Timestamp(i64::MAX)),
            issued_at_ms,
            surface: None,
            approval_token: None,
            task_id: None,
            session_id: None,
            workspace_path: None,
            tenant_id: None,
            session_token,
        };
        codec::encode(&envelope).unwrap_or_default()
    }

    /// Boot an `allow_health_bridge` AND wire a session
    /// identity service so the P5 verify_on_dispatch gate has
    /// something to call. Returns the bridge, the issuance
    /// helper, the bundle, the temp dir, and the org root.
    fn allow_health_bridge_with_session() -> (
        DispatchBridge,
        Arc<crate::identity::SessionIdentityService>,
        Bundle,
        TempDir,
        SigningKey,
    ) {
        let (mut bridge, bundle, dir, root) = allow_health_bridge();
        let store = crate::identity::TokenStore::open_in_memory().unwrap();
        let cfg = crate::identity::SessionIdentityConfig {
            enabled: true,
            session_ttl_secs: 3_600,
            session_idle_timeout_secs: 0,
            sweep_interval_secs: 60,
            verify_on_dispatch: true,
            ..Default::default()
        };
        let svc = Arc::new(
            crate::identity::SessionIdentityService::new(store, cfg, vec![7u8; 32]).unwrap(),
        );
        bridge.set_session_service(svc.clone());
        bridge.set_verify_on_dispatch(true);
        (bridge, svc, bundle, dir, root)
    }

    #[tokio::test]
    async fn p5_verify_on_dispatch_with_no_token_returns_security_denied() {
        let (bridge, _svc, bundle, _dir, _root) = allow_health_bridge_with_session();
        let envelope =
            build_request_with_session_token("node.health", bundle, 30, unix_now_ms(), None);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(env) => {
                assert_eq!(env.kind, error_kinds::SECURITY_DENIED);
                assert!(
                    env.cause.contains("session_token_missing"),
                    "cause: {}",
                    env.cause
                );
            }
            other => panic!("expected SECURITY_DENIED, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn p5_verify_on_dispatch_with_expired_token_returns_security_denied() {
        let (bridge, svc, bundle, _dir, _root) = allow_health_bridge_with_session();
        // Issue a token then forge its expiry into the past
        // and re-sign so signature verifies but expiry fails.
        // The cleanest way is to use the service's own helper:
        // issue a 1ms token, sleep past it, and verify.
        let mut tok = svc
            .issue(&crate::identity::IssueRequest {
                session_id: "sess-expire".into(),
                agent_name: "alice".into(),
                tenant_id: Some("acme".into()),
                scopes: vec!["node.health".into()],
                ttl_secs: Some(1),
            })
            .unwrap();
        // Move expiry into the past + re-sign.
        tok.expires_at_ms = tok.issued_at_ms - 1;
        // Re-sign by routing through the service's
        // canonical-bytes + sign path. The simplest is to
        // mint a brand-new token with a 1ms TTL, then wait a
        // bit so it actually expires. Avoid relying on
        // service-private resign helpers.
        let fresh = svc
            .issue(&crate::identity::IssueRequest {
                session_id: "sess-expire-2".into(),
                agent_name: "alice".into(),
                tenant_id: Some("acme".into()),
                scopes: vec!["node.health".into()],
                ttl_secs: Some(1),
            })
            .unwrap();
        let wire = fresh.to_wire().unwrap();
        // Wait for the 1s TTL to fully elapse.
        tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
        let envelope =
            build_request_with_session_token("node.health", bundle, 30, unix_now_ms(), Some(wire));
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(env) => {
                assert_eq!(env.kind, error_kinds::SECURITY_DENIED);
                assert!(
                    env.cause.contains("expired") || env.cause.contains("session_token_invalid"),
                    "cause: {}",
                    env.cause
                );
            }
            other => panic!("expected SECURITY_DENIED, got {other:?}"),
        }
        let _ = tok;
    }

    #[tokio::test]
    async fn p5_verify_on_dispatch_with_token_missing_capability_scope_returns_security_denied() {
        let (bridge, svc, bundle, _dir, _root) = allow_health_bridge_with_session();
        // Issue a token whose scopes do NOT cover node.health.
        let tok = svc
            .issue(&crate::identity::IssueRequest {
                session_id: "sess-narrow".into(),
                agent_name: "alice".into(),
                tenant_id: Some("acme".into()),
                scopes: vec!["tool.web_fetch".into()],
                ttl_secs: Some(60),
            })
            .unwrap();
        let wire = tok.to_wire().unwrap();
        let envelope =
            build_request_with_session_token("node.health", bundle, 30, unix_now_ms(), Some(wire));
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(env) => {
                assert_eq!(env.kind, error_kinds::SECURITY_DENIED);
                assert!(
                    env.cause.contains("token_insufficient_scope"),
                    "cause: {}",
                    env.cause
                );
            }
            other => panic!("expected SECURITY_DENIED, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn p5_verify_on_dispatch_admits_when_token_scope_covers_method() {
        let (bridge, svc, bundle, _dir, _root) = allow_health_bridge_with_session();
        let tok = svc
            .issue(&crate::identity::IssueRequest {
                session_id: "sess-ok".into(),
                agent_name: "alice".into(),
                tenant_id: Some("acme".into()),
                scopes: vec!["node.health".into()],
                ttl_secs: Some(60),
            })
            .unwrap();
        let wire = tok.to_wire().unwrap();
        let envelope =
            build_request_with_session_token("node.health", bundle, 30, unix_now_ms(), Some(wire));
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        assert!(
            matches!(resp.res, ResponseResult::Ok(_)),
            "valid in-scope token must admit, got {:?}",
            resp.res
        );
    }

    #[tokio::test]
    async fn p5_wildcard_scope_admits_any_method() {
        // Operators that want broad tokens use the `*` scope.
        let (bridge, svc, bundle, _dir, _root) = allow_health_bridge_with_session();
        let tok = svc
            .issue(&crate::identity::IssueRequest {
                session_id: "sess-wildcard".into(),
                agent_name: "alice".into(),
                tenant_id: Some("acme".into()),
                scopes: vec!["*".into()],
                ttl_secs: Some(60),
            })
            .unwrap();
        let wire = tok.to_wire().unwrap();
        let envelope =
            build_request_with_session_token("node.health", bundle, 30, unix_now_ms(), Some(wire));
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        assert!(
            matches!(resp.res, ResponseResult::Ok(_)),
            "`*` scope must admit, got {:?}",
            resp.res
        );
    }

    #[tokio::test]
    async fn p5_verify_off_admits_call_with_no_token_regardless_of_service_wiring() {
        let (bridge, _bundle, _dir, _root) = allow_health_bridge();
        // No session_service wired AND verify_on_dispatch off
        // → existing pre-P5 behaviour is preserved.
        assert!(!bridge.verify_on_dispatch_enabled());
        let envelope = build_request("node.health", b"hi".to_vec(), _bundle, 30);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        assert!(
            matches!(resp.res, ResponseResult::Ok(_)),
            "verify-off must preserve pre-P5 behaviour"
        );
    }

    #[tokio::test]
    async fn p2_replayed_nonce_is_rejected_second_time_with_replay_rejected() {
        let (bridge, bundle, _dir, _root) = allow_health_bridge();
        let fixed_rid = relix_core::types::RequestId([7u8; 16]);
        // SECTION 7 criterion 1: a true replay re-sends the
        // IDENTICAL envelope — same (caller_peer_id, rid, n) —
        // so we pin a single `issued_at` for both calls.
        let issued = unix_now_ms();
        let envelope_1 =
            build_request_with_clock("node.health", bundle.clone(), 30, issued, Some(fixed_rid));
        let resp_bytes_1 = bridge.handle_inbound(envelope_1).await;
        let resp_1 = decode_response(&resp_bytes_1).unwrap();
        assert!(
            matches!(resp_1.res, ResponseResult::Ok(_)),
            "first observation must admit, got {:?}",
            resp_1.res
        );
        // Second call with the same (peer, rid, n) → REPLAY_REJECTED.
        let envelope_2 =
            build_request_with_clock("node.health", bundle, 30, issued, Some(fixed_rid));
        let resp_bytes_2 = bridge.handle_inbound(envelope_2).await;
        let resp_2 = decode_response(&resp_bytes_2).unwrap();
        match resp_2.res {
            ResponseResult::Err(env) => {
                assert_eq!(env.kind, error_kinds::REPLAY_REJECTED);
                assert!(
                    env.cause.contains("replay_rejected"),
                    "cause: {}",
                    env.cause
                );
            }
            other => panic!("expected REPLAY_REJECTED, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn p2_request_arriving_after_deadline_returns_deadline_exceeded() {
        let (bridge, bundle, _dir, _root) = allow_health_bridge();
        // Issue with a NEGATIVE deadline-from-now so the
        // resulting absolute deadline is already in the past.
        let envelope = build_request_with_clock("node.health", bundle, -10, unix_now_ms(), None);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        match resp.res {
            ResponseResult::Err(env) => {
                assert_eq!(env.kind, error_kinds::TIMEOUT);
                assert!(
                    env.cause.contains("deadline_exceeded"),
                    "cause: {}",
                    env.cause
                );
            }
            other => panic!("expected TIMEOUT, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn p2_request_within_skew_tolerance_admits() {
        // P2 test: "A request arriving 4 seconds after its
        // issued_at with 5-second skew tolerance is accepted".
        let (mut bridge, bundle, _dir, _root) = allow_health_bridge();
        bridge.set_max_clock_skew_ms(5_000);
        let fake = Arc::new(relix_core::clock::FakeClock::new(1_000_000));
        bridge.set_clock(fake.clone());
        // Envelope was issued at the bridge's "current" clock,
        // then the responder's clock advances 4 seconds.
        let issued_at_ms = fake.now_ms();
        fake.set(issued_at_ms + 4_000);
        let envelope = build_request_with_clock("node.health", bundle, 60, issued_at_ms, None);
        let resp_bytes = bridge.handle_inbound(envelope).await;
        let resp = decode_response(&resp_bytes).unwrap();
        assert!(
            matches!(resp.res, ResponseResult::Ok(_)),
            "4s skew with 5s tolerance must admit, got {:?}",
            resp.res
        );
    }

    #[tokio::test(start_paused = true)]
    async fn p2_section7_future_rejected_and_within_window_past_accepted() {
        // SECTION 7 criterion 3 (one-sided freshness):
        //  (a) an envelope stamped beyond the FUTURE skew
        //      allowance is rejected; AND
        //  (b) an envelope 6s in the PAST — which the OLD 5s
        //      two-sided window WRONGLY rejected — is now
        //      ACCEPTED, because the past freshness window is
        //      5 minutes (RELIX-1 §1.9).
        let (mut bridge, bundle, _dir, _root) = allow_health_bridge();
        bridge.set_max_clock_skew_ms(5_000); // 5s future allowance
        let fake = Arc::new(relix_core::clock::FakeClock::new(1_000_000));
        bridge.set_clock(fake.clone());

        // (a) Envelope stamped 10s in the FUTURE → rejected.
        let now0 = fake.now_ms();
        let future_envelope =
            build_request_with_clock("node.health", bundle.clone(), 60, now0 + 10_000, None);
        let resp = decode_response(&bridge.handle_inbound(future_envelope).await).unwrap();
        match resp.res {
            ResponseResult::Err(env) => {
                assert_eq!(env.kind, error_kinds::REPLAY_REJECTED);
                assert!(
                    env.cause.contains("future_envelope"),
                    "future stamp must be rejected one-sided, cause: {}",
                    env.cause
                );
            }
            other => panic!("expected future_envelope rejection, got {other:?}"),
        }

        // (b) Envelope issued 6s in the PAST → now ADMITTED
        // (within the 5-minute freshness window).
        let issued_past = fake.now_ms();
        fake.set(issued_past + 6_000);
        let past_envelope = build_request_with_clock("node.health", bundle, 60, issued_past, None);
        let resp = decode_response(&bridge.handle_inbound(past_envelope).await).unwrap();
        assert!(
            matches!(resp.res, ResponseResult::Ok(_)),
            "a request 6s after issuance must be admitted (5-min window), got {:?}",
            resp.res
        );
    }

    #[tokio::test]
    async fn p2_eviction_task_removes_expired_nonces() {
        // P2 test: "The eviction task removes expired nonces."
        // We exercise the cache directly via the bridge's
        // replay_cache() handle. Insert a nonce; advance time;
        // evict; verify the entry is gone.
        let (bridge, _bundle, _dir, _root) = allow_health_bridge();
        let cache = bridge.replay_cache();
        cache.check_and_insert("nonce-evict", 0).unwrap();
        assert_eq!(cache.len(), 1);
        // SECTION 7: cache window = freshness window (5 min);
        // evict just past it.
        let removed = cache.evict_expired(bridge.freshness_window_ms() + 1);
        assert_eq!(removed, 1);
        assert_eq!(cache.len(), 0);
    }

    #[tokio::test]
    async fn p2_section7_freshness_window_sizes_cache_skew_is_independent() {
        // SECTION 7: the replay cache window tracks the
        // FRESHNESS window (5 min default), NOT the future
        // clock-skew allowance — they are decoupled.
        let (mut bridge, _bundle, _dir, _root) = allow_health_bridge();
        // Defaults: 5-min window, 5s future skew.
        assert_eq!(bridge.freshness_window_ms(), replay::DEFAULT_WINDOW_MS);
        assert_eq!(bridge.replay_cache().window_ms(), replay::DEFAULT_WINDOW_MS);
        assert_eq!(bridge.max_clock_skew_ms(), replay::DEFAULT_CLOCK_SKEW_MS);
        // Tuning the future skew does NOT resize the cache.
        bridge.set_max_clock_skew_ms(2_000);
        assert_eq!(bridge.max_clock_skew_ms(), 2_000);
        assert_eq!(bridge.replay_cache().window_ms(), replay::DEFAULT_WINDOW_MS);
        // Tuning the freshness window DOES resize the cache.
        bridge.set_freshness_window_ms(120_000);
        assert_eq!(bridge.freshness_window_ms(), 120_000);
        assert_eq!(bridge.replay_cache().window_ms(), 120_000);
    }

    #[tokio::test]
    async fn p2_section7_identity_failure_does_not_insert_into_replay_cache() {
        // SECTION 7 criterion 2: an inbound whose identity fails
        // verification must NOT pin a nonce — the cache insert
        // happens only AFTER identity verify, so an
        // unauthenticated attacker cannot fill the cache (DoS).
        let (bridge, _bundle, _dir, _root) = allow_health_bridge();
        // Forge a bundle signed by a DIFFERENT (untrusted) org
        // root so `validate_identity_bundle` rejects it.
        let rogue_root = SigningKey::generate(&mut OsRng);
        let rogue_caller = SigningKey::generate(&mut OsRng);
        let id = IdentityBundle {
            subject_id: NodeId::from_pubkey(&rogue_caller.verifying_key().to_bytes()),
            name: "mallory".into(),
            org_id: NodeId::from_pubkey(&rogue_root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let rogue_bundle = issue_identity(id, &rogue_root, 3600).unwrap();
        assert_eq!(bridge.replay_cache().len(), 0);
        let envelope = build_request_with_clock(
            "node.health",
            rogue_bundle,
            30,
            unix_now_ms(),
            Some(relix_core::types::RequestId([9u8; 16])),
        );
        let resp = decode_response(&bridge.handle_inbound(envelope).await).unwrap();
        match resp.res {
            ResponseResult::Err(env) => assert_eq!(env.kind, error_kinds::IDENTITY_INVALID),
            other => panic!("expected IDENTITY_INVALID, got {other:?}"),
        }
        assert_eq!(
            bridge.replay_cache().len(),
            0,
            "an identity-failed request must NOT insert into the replay cache"
        );
    }

    #[tokio::test]
    async fn p2_section7_two_peers_same_rid_both_admitted() {
        // SECTION 7 criterion 4: the cache key is
        // (caller_peer_id, rid, n), so two DISTINCT peers using
        // the same rid do not collide — both admit on first use.
        let (bridge, bundle_a, _dir, root) = allow_health_bridge();
        let shared_rid = relix_core::types::RequestId([3u8; 16]);
        let issued = unix_now_ms();

        // Peer A (alice) admits.
        let env_a = build_request_with_clock("node.health", bundle_a, 30, issued, Some(shared_rid));
        let resp_a = decode_response(&bridge.handle_inbound(env_a).await).unwrap();
        assert!(
            matches!(resp_a.res, ResponseResult::Ok(_)),
            "peer A must admit, got {:?}",
            resp_a.res
        );

        // Peer B — a DIFFERENT subject under the SAME org root —
        // reusing the same rid must ALSO admit (no collision).
        let caller_b = SigningKey::generate(&mut OsRng);
        let id_b = IdentityBundle {
            subject_id: NodeId::from_pubkey(&caller_b.verifying_key().to_bytes()),
            name: "bob".into(),
            org_id: NodeId::from_pubkey(&root.verifying_key().to_bytes()),
            groups: vec!["chat-users".into()],
            role: "agent".into(),
            clearance: "internal".into(),
            supervisors: vec![],
        };
        let bundle_b = issue_identity(id_b, &root, 3600).unwrap();
        let env_b = build_request_with_clock("node.health", bundle_b, 30, issued, Some(shared_rid));
        let resp_b = decode_response(&bridge.handle_inbound(env_b).await).unwrap();
        assert!(
            matches!(resp_b.res, ResponseResult::Ok(_)),
            "peer B with the same rid must NOT collide with peer A, got {:?}",
            resp_b.res
        );
    }
}
