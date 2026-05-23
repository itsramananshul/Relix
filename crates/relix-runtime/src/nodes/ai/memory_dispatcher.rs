//! Outbound call from the AI node to the memory peer for the
//! frozen-snapshot memory pattern.
//!
//! `ai.chat` reads `memory.agent_read` once per call (at the start
//! of the chat session — that's why it's "frozen-snapshot") and
//! prepends the agent + user memory blocks to the system prompt
//! before invoking the underlying LLM provider. Mid-session memory
//! writes go to the memory store immediately but the running chat
//! session's prompt does NOT re-render — the snapshot refreshes on
//! the next session.
//!
//! Failure mode is **silent skip**: if the memory peer is
//! unreachable, the response decode fails, or the parsed bytes
//! don't match the documented header shape, we proceed without
//! memory. A chat call must never fail because memory is
//! unavailable.

use crate::dispatch::{build_request, decode_response};
use crate::manifest::MeshClient;
use crate::transport::envelope::ResponseResult;
use async_trait::async_trait;
use relix_core::bundle::Bundle;

/// Async hook the AI handler uses to fetch frozen-snapshot
/// memory for a subject. Production implementations dial the
/// memory peer over libp2p; tests stub this directly to
/// exercise the injection path without a live mesh.
#[async_trait]
pub trait MemoryFetcher: Send + Sync {
    /// Return `(agent_memory, user_memory)` for `subject_id` on
    /// success, or `None` on any failure. The caller silently
    /// skips memory injection on `None`.
    async fn fetch(&self, subject_id: &str) -> Option<(String, String)>;

    /// Return recent conversation turns for a session as a
    /// `role: text\n` block (oldest first; same wire format as
    /// `memory.recent_for_session`), or `None` on any failure
    /// or when no auto-fetch is wired. Default returns `None`
    /// so existing test stubs keep working unchanged.
    async fn fetch_history(&self, _session_id: &str) -> Option<String> {
        None
    }
}

/// A long-lived dispatcher that calls `memory.agent_read` and
/// `memory.recent_for_session` on the memory peer. The AI
/// controller builds this once at startup; the ai.chat handler
/// captures an `Arc<OnceCell<_>>` of it.
#[derive(Clone)]
pub struct MemoryDispatcher {
    mesh: MeshClient,
    /// Peer alias the mesh client uses to dial memory. Operator
    /// configures it in `[ai.memory_peer] alias = ...`. Defaults
    /// to `"memory"`.
    alias: String,
    /// Identity bundle signing the outbound request. Same bundle
    /// the heartbeat sender uses — loaded from
    /// `<identity.key_path>.bundle` at controller startup.
    identity: Bundle,
    /// Per-call deadline. `memory.agent_read` and
    /// `memory.recent_for_session` are both cheap reads; 5s is
    /// plenty and keeps the chat call snappy even when memory
    /// is degraded.
    deadline_secs: i64,
    /// How many recent turns `fetch_history` requests. Sent
    /// to `memory.recent_for_session` as the `N` field.
    max_history_turns: usize,
}

impl MemoryDispatcher {
    /// Construct. Caller owns the MeshClient + identity. The
    /// `max_history_turns` value caps how many turns
    /// `fetch_history` asks for; memory enforces its own ceiling
    /// (`max_recent` in the memory config).
    pub fn new(
        mesh: MeshClient,
        alias: String,
        identity: Bundle,
        deadline_secs: i64,
        max_history_turns: usize,
    ) -> Self {
        Self {
            mesh,
            alias,
            identity,
            deadline_secs,
            max_history_turns,
        }
    }
}

#[async_trait]
impl MemoryFetcher for MemoryDispatcher {
    /// Fetch agent + user memory for a `subject_id`. `None` on
    /// any failure (network, decode, format mismatch). The caller
    /// should silently skip memory injection in that case.
    async fn fetch(&self, subject_id: &str) -> Option<(String, String)> {
        let envelope = build_request(
            "memory.agent_read",
            subject_id.as_bytes().to_vec(),
            self.identity.clone(),
            self.deadline_secs,
        );
        let resp_bytes = match self.mesh.call(&self.alias, envelope).await {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(
                    alias = %self.alias,
                    subject_id = %subject_id,
                    error = %e,
                    "ai.chat memory fetch failed (silent skip)"
                );
                return None;
            }
        };
        let resp = decode_response(&resp_bytes).ok()?;
        let body = match resp.res {
            ResponseResult::Ok(b) => b.to_vec(),
            ResponseResult::Err(env) => {
                tracing::debug!(
                    alias = %self.alias,
                    subject_id = %subject_id,
                    cause = %env.cause,
                    "ai.chat memory peer returned err (silent skip)"
                );
                return None;
            }
            ResponseResult::StreamHandle(_) => return None,
        };
        parse_agent_read_body(&body)
    }

    /// Fetch the last N conversation turns for a session. Wire
    /// format mirrors `memory.recent_for_session`: arg
    /// `session_id|N`, response body `role: text\n` per turn,
    /// oldest first. `None` on any transport, decode, or
    /// responder error — `ai.chat` proceeds without history
    /// rather than failing.
    async fn fetch_history(&self, session_id: &str) -> Option<String> {
        if session_id.is_empty() {
            return None;
        }
        let arg = format!("{session_id}|{}", self.max_history_turns);
        let envelope = build_request(
            "memory.recent_for_session",
            arg.into_bytes(),
            self.identity.clone(),
            self.deadline_secs,
        );
        let resp_bytes = match self.mesh.call(&self.alias, envelope).await {
            Ok(b) => b,
            Err(e) => {
                tracing::debug!(
                    alias = %self.alias,
                    session_id = %session_id,
                    error = %e,
                    "ai.chat history fetch failed (silent skip)"
                );
                return None;
            }
        };
        let resp = decode_response(&resp_bytes).ok()?;
        match resp.res {
            ResponseResult::Ok(b) => {
                let text = std::str::from_utf8(b.as_ref()).ok()?;
                if text.is_empty() {
                    None
                } else {
                    Some(text.to_string())
                }
            }
            ResponseResult::Err(env) => {
                tracing::debug!(
                    alias = %self.alias,
                    session_id = %session_id,
                    cause = %env.cause,
                    "ai.chat history peer returned err (silent skip)"
                );
                None
            }
            ResponseResult::StreamHandle(_) => None,
        }
    }
}

/// Parse the wire format emitted by `memory.agent_read`:
/// `agent_bytes=N|user_bytes=M\n<N bytes><M bytes>`.
///
/// Returns `None` on any malformed input — frozen-snapshot
/// memory injection silently skips on any error.
pub fn parse_agent_read_body(body: &[u8]) -> Option<(String, String)> {
    let nl_pos = body.iter().position(|b| *b == b'\n')?;
    let header = std::str::from_utf8(&body[..nl_pos]).ok()?;
    let (agent_kv, user_kv) = header.split_once('|')?;
    let agent_len = agent_kv
        .strip_prefix("agent_bytes=")?
        .parse::<usize>()
        .ok()?;
    let user_len = user_kv.strip_prefix("user_bytes=")?.parse::<usize>().ok()?;
    let payload = &body[nl_pos + 1..];
    if payload.len() != agent_len + user_len {
        return None;
    }
    let agent = std::str::from_utf8(&payload[..agent_len]).ok()?.to_string();
    let user = std::str::from_utf8(&payload[agent_len..agent_len + user_len])
        .ok()?
        .to_string();
    Some((agent, user))
}

/// Format the agent + user memory as the labeled block the spec
/// prescribes. Returns `None` when BOTH targets are empty — in
/// that case the caller should skip memory injection entirely
/// (no value in adding an empty block to the system prompt).
pub fn format_memory_block(agent_mem: &str, user_mem: &str) -> Option<String> {
    if agent_mem.trim().is_empty() && user_mem.trim().is_empty() {
        return None;
    }
    let mut s = String::with_capacity(64 + agent_mem.len() + user_mem.len());
    s.push_str("--- AGENT MEMORY ---\n");
    s.push_str(agent_mem);
    if !agent_mem.is_empty() && !agent_mem.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("\n--- USER MEMORY ---\n");
    s.push_str(user_mem);
    if !user_mem.is_empty() && !user_mem.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("--------------------");
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_typical_body() {
        let body = b"agent_bytes=5|user_bytes=6\nhelloworld!";
        let (a, u) = parse_agent_read_body(body).unwrap();
        assert_eq!(a, "hello");
        assert_eq!(u, "world!");
    }

    #[test]
    fn parse_empty_body() {
        let body = b"agent_bytes=0|user_bytes=0\n";
        let (a, u) = parse_agent_read_body(body).unwrap();
        assert_eq!(a, "");
        assert_eq!(u, "");
    }

    #[test]
    fn parse_rejects_truncated_payload() {
        // Header claims 10+10 bytes, payload provides less.
        let body = b"agent_bytes=10|user_bytes=10\nshort";
        assert!(parse_agent_read_body(body).is_none());
    }

    #[test]
    fn parse_rejects_missing_header() {
        let body = b"helloworld";
        assert!(parse_agent_read_body(body).is_none());
    }

    #[test]
    fn parse_rejects_malformed_lengths() {
        let body = b"agent_bytes=abc|user_bytes=0\n";
        assert!(parse_agent_read_body(body).is_none());
    }

    #[test]
    fn format_block_both_present() {
        let s = format_memory_block("agent notes", "user notes").unwrap();
        assert!(s.contains("--- AGENT MEMORY ---"));
        assert!(s.contains("agent notes"));
        assert!(s.contains("--- USER MEMORY ---"));
        assert!(s.contains("user notes"));
        assert!(s.ends_with("--------------------"));
    }

    #[test]
    fn format_block_only_agent() {
        let s = format_memory_block("agent notes", "").unwrap();
        assert!(s.contains("--- AGENT MEMORY ---"));
        assert!(s.contains("agent notes"));
        // USER block heading is still present so the model sees
        // the section structure even when one half is empty.
        assert!(s.contains("--- USER MEMORY ---"));
    }

    #[test]
    fn format_block_both_empty_returns_none() {
        assert!(format_memory_block("", "").is_none());
        assert!(format_memory_block("   ", "\n").is_none());
    }
}
