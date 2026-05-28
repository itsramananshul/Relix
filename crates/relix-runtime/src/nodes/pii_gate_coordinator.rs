//! RELIX-7.28 Part 3 — coordinator-side wiring for `pii.*` capabilities.
//!
//! Two unary capabilities:
//!
//! - `pii.scan_stats` — totals by `action_taken` over the requested
//!   window, plus the top methods triggering PII.
//! - `pii.recent_events` — newest N rows, optionally filtered by method.

use std::sync::Arc;

use serde::Deserialize;

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};
use relix_core::types::{ErrorEnvelope, error_kinds};

use super::pii_gate::MeshPiiGate;

#[derive(Debug, Default, Deserialize)]
struct StatsArgs {
    #[serde(default)]
    hours: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct EventsArgs {
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    method: Option<String>,
}

pub fn register(bridge: &mut DispatchBridge, gate: Arc<MeshPiiGate>) {
    register_stats(bridge, gate.clone());
    register_events(bridge, gate);
}

fn register_stats(bridge: &mut DispatchBridge, gate: Arc<MeshPiiGate>) {
    bridge.register(
        "pii.scan_stats",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let gate = gate.clone();
            async move {
                let args: StatsArgs = if ctx.args.is_empty() {
                    StatsArgs::default()
                } else {
                    match serde_json::from_slice(&ctx.args) {
                        Ok(a) => a,
                        Err(e) => return invalid(&format!("decode args: {e}")),
                    }
                };
                let hours = args.hours.unwrap_or(24).clamp(1, 24 * 90);
                match gate.scan_stats(hours) {
                    Ok(stats) => match serde_json::to_vec(&stats) {
                        Ok(b) => HandlerOutcome::Ok(b),
                        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::RESPONDER_INTERNAL,
                            cause: format!("pii.scan_stats encode: {e}"),
                            retry_hint: 0,
                            retry_after: None,
                        }),
                    },
                    Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::RESPONDER_INTERNAL,
                        cause: format!("pii.scan_stats: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

fn register_events(bridge: &mut DispatchBridge, gate: Arc<MeshPiiGate>) {
    bridge.register(
        "pii.recent_events",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let gate = gate.clone();
            async move {
                let args: EventsArgs = if ctx.args.is_empty() {
                    EventsArgs::default()
                } else {
                    match serde_json::from_slice(&ctx.args) {
                        Ok(a) => a,
                        Err(e) => return invalid(&format!("decode args: {e}")),
                    }
                };
                let limit = args.limit.unwrap_or(50).clamp(1, 1000);
                match gate.recent_events(limit, args.method.as_deref()) {
                    Ok(rows) => match serde_json::to_vec(&rows) {
                        Ok(b) => HandlerOutcome::Ok(b),
                        Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                            kind: error_kinds::RESPONDER_INTERNAL,
                            cause: format!("pii.recent_events encode: {e}"),
                            retry_hint: 0,
                            retry_after: None,
                        }),
                    },
                    Err(e) => HandlerOutcome::Err(ErrorEnvelope {
                        kind: error_kinds::RESPONDER_INTERNAL,
                        cause: format!("pii.recent_events: {e}"),
                        retry_hint: 0,
                        retry_after: None,
                    }),
                }
            }
        })),
    );
}

fn invalid(msg: &str) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause: msg.to_string(),
        retry_hint: 0,
        retry_after: None,
    })
}

/// Static descriptor list for the two `pii.*` capabilities.
pub fn pii_capability_descriptors() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "pii.scan_stats",
            "RELIX-7.28 Part 3: PII detection counts by action_taken over the last `hours` \
             (default 24). Returns {window_hours, total_events, blocked, redacted, logged, \
             top_methods: [{method, count}]}.",
        ),
        (
            "pii.recent_events",
            "RELIX-7.28 Part 3: newest N PII detection events from the gate's chronicle, \
             optionally filtered by `method`. Args (JSON): {limit?, method?}; limit defaults \
             to 50.",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptors_cover_both_capabilities() {
        let descs = pii_capability_descriptors();
        let methods: Vec<&str> = descs.iter().map(|(m, _)| *m).collect();
        assert!(methods.contains(&"pii.scan_stats"));
        assert!(methods.contains(&"pii.recent_events"));
    }
}
