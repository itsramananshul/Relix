//! AI node — registers the `ai.chat` capability (M7).
//!
//! ## Alpha mode
//!
//! M7 ships an in-process **stub responder** so the first real chat
//! orchestration can run end-to-end without a paid provider. M8 swaps this
//! for an Anthropic-backed implementation behind the same capability.
//!
//! Stub behavior, given an arg `session_id|user_message`:
//!
//! - Returns a deterministic string of the form
//!   `stub: heard "<user_message>" in <session_id>\n`.
//! - This is enough to demonstrate (a) the SOL flow's variable plumbing,
//!   (b) the responder's policy + audit, (c) the multi-node flow log.
//!
//! ## Wire format (SIMP-016 alpha)
//!
//! Arg:    `session_id|user_message`  (UTF-8; pipe-delimited).
//! Return: `stub: heard "<msg>" in <session>\n`
//!
//! Real model invocation (Anthropic) takes the same arg shape, returns the
//! model's reply text. The SOL flow does not change between M7 stub and M8
//! real provider.
//!
//! ## Config
//!
//! `[ai]` section in the controller TOML:
//!
//! ```toml
//! [ai]
//! mode = "stub"   # M7: deterministic placeholder
//! # mode = "anthropic"  # M8 (not yet implemented)
//! ```
//!
//! `mode = "anthropic"` is reserved for M8 and currently returns a clear
//! `not_yet_implemented` error from the handler so misconfiguration is
//! caught at startup-adjacent time (the controller boots, but the first
//! `ai.chat` call surfaces the gap).

use std::sync::Arc;

use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// Per-node AI configuration.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct AiConfig {
    /// `"stub"` (M7 default) or `"anthropic"` (M8, not yet implemented).
    #[serde(default = "default_mode")]
    pub mode: String,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            mode: default_mode(),
        }
    }
}

fn default_mode() -> String {
    "stub".to_string()
}

/// Register the `ai.chat` capability based on the configured mode.
pub fn register(bridge: &mut DispatchBridge, cfg: AiConfig) {
    let mode = cfg.mode.clone();
    bridge.register(
        "ai.chat",
        Arc::new(FnHandler(move |ctx: InvocationCtx| {
            let mode = mode.clone();
            async move { handle_chat(&mode, &ctx) }
        })),
    );
}

fn handle_chat(mode: &str, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: format!("ai.chat arg utf8: {e}"),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    let (session_id, user_msg) = match s.split_once('|') {
        Some((s, m)) if !s.is_empty() => (s, m),
        _ => {
            return HandlerOutcome::Err(ErrorEnvelope {
                kind: error_kinds::INVALID_ARGS,
                cause: "ai.chat arg must be `session_id|user_message`".to_string(),
                retry_hint: 2,
                retry_after: None,
            });
        }
    };
    match mode {
        "stub" => {
            let reply = format!("stub: heard \"{user_msg}\" in {session_id}\n");
            HandlerOutcome::Ok(reply.into_bytes())
        }
        "anthropic" => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: "ai.chat mode=anthropic not yet implemented (M8)".to_string(),
            retry_hint: 2,
            retry_after: None,
        }),
        other => HandlerOutcome::Err(ErrorEnvelope {
            kind: error_kinds::RESPONDER_INTERNAL,
            cause: format!("ai.chat: unknown mode '{other}'"),
            retry_hint: 2,
            retry_after: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relix_core::identity::VerifiedIdentity;
    use relix_core::types::{NodeId, RequestId, TraceId};

    fn ctx(args: &[u8]) -> InvocationCtx {
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"alice"),
                name: "alice".into(),
                org_id: NodeId::from_pubkey(b"org"),
                groups: vec!["chat-users".into()],
                role: "agent".into(),
                clearance: "internal".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
        }
    }

    #[test]
    fn stub_reply_is_deterministic() {
        let r = handle_chat("stub", &ctx(b"s1|hello"));
        match r {
            HandlerOutcome::Ok(body) => {
                let text = String::from_utf8(body).unwrap();
                assert_eq!(text, "stub: heard \"hello\" in s1\n");
            }
            HandlerOutcome::Err(e) => panic!("unexpected error: {}", e.cause),
        }
    }

    #[test]
    fn anthropic_mode_signals_not_yet_implemented() {
        let r = handle_chat("anthropic", &ctx(b"s1|x"));
        match r {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("not yet implemented")),
            HandlerOutcome::Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn missing_pipe_separator_rejected() {
        let r = handle_chat("stub", &ctx(b"no-separator"));
        match r {
            HandlerOutcome::Err(e) => assert_eq!(e.kind, error_kinds::INVALID_ARGS),
            HandlerOutcome::Ok(_) => panic!("expected invalid_args"),
        }
    }
}
