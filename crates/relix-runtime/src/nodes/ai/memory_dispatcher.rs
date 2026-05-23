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

    /// Whether the dispatcher is wired to perform RAG retrieval.
    /// `ai.chat` queries this before embedding the prompt — when
    /// `false`, no embedding work happens. Default `false`.
    fn rag_enabled(&self) -> bool {
        false
    }

    /// Top-K limit passed through to `memory.search` when RAG
    /// is enabled. Default 5.
    fn rag_top_k(&self) -> usize {
        5
    }

    /// Cosine-similarity floor for RAG results. Hits below this
    /// score are dropped before formatting. Default 0.70.
    fn rag_min_score(&self) -> f32 {
        0.70
    }

    /// Search the vector memory for chunks semantically similar
    /// to `embedding` (the AI node's local embedding of the user
    /// prompt). Implementations format hits as the "Relevant
    /// context from memory" block the spec prescribes, or return
    /// `None` on any failure or when no hits pass `min_score`.
    /// `ai.chat` proceeds without a RAG block on `None`.
    async fn fetch_rag(
        &self,
        _subject_id: &str,
        _embedding: &[f32],
        _top_k: usize,
        _min_score: f32,
    ) -> Option<String> {
        None
    }
}

/// A long-lived dispatcher that calls `memory.agent_read`,
/// `memory.recent_for_session`, and (when RAG is enabled)
/// `memory.search` on the memory peer. The AI controller builds
/// this once at startup; the ai.chat handler captures an
/// `Arc<OnceCell<_>>` of it.
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
    /// Per-call deadline. `memory.agent_read`,
    /// `memory.recent_for_session`, and `memory.search` are all
    /// cheap reads; 5s is plenty and keeps the chat call snappy
    /// even when memory is degraded.
    deadline_secs: i64,
    /// How many recent turns `fetch_history` requests. Sent
    /// to `memory.recent_for_session` as the `N` field.
    max_history_turns: usize,
    /// Whether `ai.chat` should perform RAG retrieval against
    /// the vector memory. When `false`, `fetch_rag` is a no-op
    /// and the handler skips the local embed call entirely.
    rag_enabled: bool,
    /// Top-K limit passed through to `memory.search` when RAG
    /// is enabled.
    rag_top_k: usize,
    /// Cosine-similarity floor for RAG results.
    rag_min_score: f32,
}

impl MemoryDispatcher {
    /// Construct. Caller owns the MeshClient + identity. The
    /// `max_history_turns` value caps how many turns
    /// `fetch_history` asks for; memory enforces its own ceiling
    /// (`max_recent` in the memory config). RAG retrieval is
    /// gated by `rag_enabled`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mesh: MeshClient,
        alias: String,
        identity: Bundle,
        deadline_secs: i64,
        max_history_turns: usize,
        rag_enabled: bool,
        rag_top_k: usize,
        rag_min_score: f32,
    ) -> Self {
        Self {
            mesh,
            alias,
            identity,
            deadline_secs,
            max_history_turns,
            rag_enabled,
            rag_top_k,
            rag_min_score,
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

    fn rag_enabled(&self) -> bool {
        self.rag_enabled
    }

    fn rag_top_k(&self) -> usize {
        self.rag_top_k
    }

    fn rag_min_score(&self) -> f32 {
        self.rag_min_score
    }

    /// Search the vector memory for chunks similar to the
    /// precomputed `embedding`. Calls `memory.search` once per
    /// target (`agent` then `user`), merges the hits, filters
    /// by `min_score`, sorts by descending score, takes the
    /// top-K overall, and formats the result as the
    /// "Relevant context from memory" block. Returns `None`
    /// on any transport/decode error or when the merged list
    /// is empty after filtering.
    async fn fetch_rag(
        &self,
        subject_id: &str,
        embedding: &[f32],
        top_k: usize,
        min_score: f32,
    ) -> Option<String> {
        if subject_id.is_empty() || embedding.is_empty() || top_k == 0 {
            return None;
        }
        // Base64 of the LE-packed bytes — same encoding the
        // mesh uses elsewhere for embedding wire payloads.
        use base64::Engine;
        let mut bytes = Vec::with_capacity(embedding.len() * 4);
        for x in embedding {
            bytes.extend_from_slice(&x.to_le_bytes());
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);

        let mut all_hits: Vec<RagHit> = Vec::new();
        for target in ["agent", "user"] {
            // Empty query field — the precomputed embedding is
            // sufficient and memory.search now accepts it.
            let arg = format!(
                "{subject_id}|{target}||{limit}|embedding={b64}",
                limit = top_k
            );
            let envelope = build_request(
                "memory.search",
                arg.into_bytes(),
                self.identity.clone(),
                self.deadline_secs,
            );
            let resp_bytes = match self.mesh.call(&self.alias, envelope).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::debug!(
                        alias = %self.alias,
                        subject_id = %subject_id,
                        target = target,
                        error = %e,
                        "ai.chat rag fetch failed (silent skip)"
                    );
                    continue;
                }
            };
            let Some(resp) = decode_response(&resp_bytes).ok() else {
                continue;
            };
            let body = match resp.res {
                ResponseResult::Ok(b) => b.to_vec(),
                ResponseResult::Err(env) => {
                    tracing::debug!(
                        alias = %self.alias,
                        subject_id = %subject_id,
                        target = target,
                        cause = %env.cause,
                        "ai.chat rag peer returned err (silent skip)"
                    );
                    continue;
                }
                ResponseResult::StreamHandle(_) => continue,
            };
            parse_rag_hits(&body, target, min_score, &mut all_hits);
        }

        if all_hits.is_empty() {
            return None;
        }
        // Sort merged hits by descending score, take top_k.
        all_hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        all_hits.truncate(top_k);
        Some(format_rag_block(&all_hits))
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

/// One vector-search hit, parsed off the `memory.search` wire.
#[derive(Clone, Debug)]
pub struct RagHit {
    pub score: f32,
    pub target: &'static str,
    pub chunk: String,
}

/// Parse a `memory.search` response body into `RagHit`s and push
/// those at or above `min_score` onto `out`. Body shape is one
/// hit per line as `<embedding_id>\t<score>\t<chunk>\n`, then a
/// final `count=N\n` row. Malformed rows are skipped silently —
/// RAG never fails the chat call.
pub fn parse_rag_hits(body: &[u8], target: &'static str, min_score: f32, out: &mut Vec<RagHit>) {
    let Ok(text) = std::str::from_utf8(body) else {
        return;
    };
    for line in text.lines() {
        if line.starts_with("count=") || line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\t');
        let _id = parts.next();
        let score_str = parts.next();
        let chunk = parts.next();
        let (Some(score_str), Some(chunk)) = (score_str, chunk) else {
            continue;
        };
        let Ok(score) = score_str.trim().parse::<f32>() else {
            continue;
        };
        if score < min_score {
            continue;
        }
        out.push(RagHit {
            score,
            target,
            chunk: chunk.to_string(),
        });
    }
}

/// Format hits into the spec block. Caller has already filtered
/// by `min_score`, sorted by descending score, and truncated to
/// top-K — this function just renders.
pub fn format_rag_block(hits: &[RagHit]) -> String {
    let mut s = String::with_capacity(64 + hits.len() * 80);
    s.push_str("--- Relevant context from memory ---\n");
    for hit in hits {
        s.push_str(&format!(
            "[score: {:.2}] ({target}) {chunk}\n",
            hit.score,
            target = hit.target,
            chunk = hit.chunk
        ));
    }
    s.push_str("---");
    s
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
