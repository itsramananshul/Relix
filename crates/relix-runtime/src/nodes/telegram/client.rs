//! Outbound RPC client for the telegram controller.
//!
//! Wraps one [`MeshClient`] + [`Bundle`] over which the
//! controller dispatches `memory.*`, `ai.chat`, and `task.*`
//! calls to the configured peers. Modeled on the AI node's
//! and the memory curator's dispatchers.
//!
//! Returned errors are kept narrow on purpose — the long-poll
//! loop converts them into user-facing fallback messages so
//! the operator's chat never silently stalls.

use std::sync::Arc;

use async_trait::async_trait;
use relix_core::bundle::Bundle;
use tokio::sync::OnceCell;

use crate::dispatch::{build_request, decode_response};
use crate::manifest::MeshClient;
use crate::transport::envelope::ResponseResult;

/// Outbound operations the controller needs from a peer
/// mesh. Production uses [`TelegramOutboundClient`]; tests
/// drop in a stub that records calls + replays scripted
/// responses.
#[async_trait]
pub trait TelegramOutbound: Send + Sync + 'static {
    async fn memory_recent(&self, session_id: &str, n: usize) -> Vec<(String, String)>;
    async fn memory_write(&self, session_id: &str, role: &str, text: &str);
    async fn memory_agent_read(&self, subject_id: &str) -> (String, String);
    async fn memory_agent_clear(&self, subject_id: &str);
    async fn ai_chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String>;
    /// RELIX-7.7 GAP 2 — dispatch the chat call to an
    /// explicit (peer, capability). Defaults to the static
    /// `ai_chat` path so test stubs don't need to override.
    async fn dispatch_chat(
        &self,
        _peer: &str,
        _capability: &str,
        session_id: &str,
        prompt: &str,
        history: &str,
    ) -> Option<String> {
        self.ai_chat(session_id, prompt, history).await
    }
    /// RELIX-7.7 GAP 2 — ask the coordinator's
    /// `routing.resolve` for the right (peer, capability).
    /// `None` means "no rule matched", in which case the
    /// caller falls back to `(ai, ai.chat)`.
    async fn routing_resolve(
        &self,
        _channel: &str,
        _sender: &str,
        _subject: &str,
        _content: &str,
    ) -> Option<(String, String)> {
        None
    }
    async fn task_create(
        &self,
        title: &str,
        flow_template: &str,
        params_json: &str,
        owner_subject_id: &str,
    ) -> Option<String>;
    async fn task_update_status(&self, task_id: &str, status: &str, result: &str);
    async fn task_event(&self, task_id: &str, event_type: &str, payload: &str);
    /// Call `coord.approval.decide` for an `approval_id`. The
    /// returned string is the body the coordinator emits:
    /// `ok\n` for reject, `ok|<token>\n` for approve. Returns
    /// `None` on transport / parse failure so the
    /// telegram handler can surface a friendly error.
    async fn approval_decide(
        &self,
        approval_id: &str,
        decision: &str,
        decided_by: &str,
        note: &str,
    ) -> Option<String>;
    /// FIX 7 — `coord.approval.get`: fetch the approval row
    /// (including its `authorized_approvers` list) so the
    /// Telegram controller can verify the caller's chat_id
    /// before recording a decision. Returns the raw JSON body
    /// the cap emits or `None` on transport / parse failure
    /// (the controller maps that to "approval not found").
    async fn approval_get(&self, _approval_id: &str) -> Option<serde_json::Value> {
        None
    }
    /// FIX 7 — `approval.record_decision`: route the decision
    /// through the documented coordinator cap that flips the
    /// approval row + fires the cancel-escalation signal.
    /// Returns the raw response body on success, `None` on
    /// transport failure so the controller can surface a
    /// friendly error.
    async fn approval_record_decision(
        &self,
        _approval_id: &str,
        _decision: &str,
        _note: &str,
    ) -> Option<String> {
        None
    }
    async fn task_list(
        &self,
        status_filter: Option<&str>,
        limit: usize,
    ) -> Vec<(String, String, String)>;
    /// Call `tool.audio.transcribe` with the raw audio bytes.
    /// Returns the transcribed text on success, `None` on
    /// transport / decode failure OR when no audio peer is
    /// configured. Callers map `None` to the operator-facing
    /// fallback message.
    async fn tool_audio_transcribe(&self, audio_bytes: Vec<u8>) -> Option<String>;
}

/// Lazily-populated outbound client. The controller's long-
/// poll loop pulls a clone from this cell on every tick;
/// while empty the loop posts a static "I'm not wired to
/// the mesh yet" reply.
pub type TelegramOutboundClientCell = Arc<OnceCell<Arc<TelegramOutboundClient>>>;

/// Outbound mesh client used by the telegram controller.
/// Holds a single [`MeshClient`] keyed by alias —
/// `memory`, `ai`, `coordinator` — and reuses one signed
/// identity bundle for every call.
pub struct TelegramOutboundClient {
    pub mesh: MeshClient,
    pub identity: Bundle,
    pub memory_alias: String,
    pub memory_deadline_secs: i64,
    pub ai_alias: String,
    pub ai_deadline_secs: i64,
    pub coord_alias: String,
    pub coord_deadline_secs: i64,
    /// Audio (tool.audio.transcribe) peer alias. `None` means
    /// the operator did not configure a `[telegram.audio_peer]`
    /// — voice messages then surface a fallback reply instead
    /// of being transcribed.
    pub audio_alias: Option<String>,
    pub audio_deadline_secs: i64,
}

/// Errors surfaced to the controller loop. Kept narrow on
/// purpose — controller maps every variant to the same
/// user-facing fallback message.
#[derive(Debug)]
pub enum OutboundError {
    /// Transport / responder failure or a non-OK envelope.
    Mesh(String),
    /// Response body wasn't valid UTF-8.
    Decode(String),
}

impl std::fmt::Display for OutboundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OutboundError::Mesh(m) => write!(f, "mesh: {m}"),
            OutboundError::Decode(m) => write!(f, "decode: {m}"),
        }
    }
}

impl std::error::Error for OutboundError {}

impl TelegramOutboundClient {
    async fn call(
        &self,
        alias: &str,
        method: &str,
        deadline_secs: i64,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, OutboundError> {
        let envelope = build_request(method, body, self.identity.clone(), deadline_secs);
        let resp = self
            .mesh
            .call(alias, envelope)
            .await
            .map_err(|e| OutboundError::Mesh(format!("call: {e}")))?;
        let env =
            decode_response(&resp).map_err(|e| OutboundError::Mesh(format!("decode: {e}")))?;
        match env.res {
            ResponseResult::Ok(b) => Ok(b.to_vec()),
            ResponseResult::Err(env) => Err(OutboundError::Mesh(format!(
                "responder err kind={} cause={}",
                env.kind, env.cause
            ))),
            ResponseResult::StreamHandle(_) => Err(OutboundError::Mesh(
                "unexpected stream handle on a unary response".into(),
            )),
        }
    }

    async fn call_text(
        &self,
        alias: &str,
        method: &str,
        deadline_secs: i64,
        body: Vec<u8>,
    ) -> Result<String, OutboundError> {
        let bytes = self.call(alias, method, deadline_secs, body).await?;
        String::from_utf8(bytes).map_err(|e| OutboundError::Decode(format!("utf8: {e}")))
    }

    // ── memory.* ─────────────────────────────────────────

    /// Direct (non-trait) accessor for the recent-for-session
    /// path. Kept inherent so callers that hold the concrete
    /// type can skip the trait dispatch.
    pub async fn memory_recent(&self, session_id: &str, n: usize) -> Vec<(String, String)> {
        let arg = format!("{session_id}|{n}");
        let body = match self
            .call_text(
                &self.memory_alias,
                "memory.recent_for_session",
                self.memory_deadline_secs,
                arg.into_bytes(),
            )
            .await
        {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "telegram: memory.recent_for_session failed");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for line in body.lines() {
            if let Some((role, rest)) = line.split_once(": ") {
                out.push((role.to_string(), rest.to_string()));
            }
        }
        out
    }

    /// `memory.write_turn` — best-effort persistence.
    /// Failures are logged at warn; the controller never
    /// fails the user-facing reply for a write blip.
    pub async fn memory_write(&self, session_id: &str, role: &str, text: &str) {
        // Sanitise the text on this side: the wire format
        // uses `|` as a separator and the memory node's
        // `add` rejects content containing the entry
        // delimiter §, but bare `|` in chat text is the
        // common case we need to scrub.
        let sanitised: String = text
            .chars()
            .map(|c| match c {
                '|' => ' ',
                '\r' | '\n' | '\t' => ' ',
                other => other,
            })
            .collect();
        let arg = format!("{session_id}|{role}|{sanitised}");
        if let Err(e) = self
            .call(
                &self.memory_alias,
                "memory.write_turn",
                self.memory_deadline_secs,
                arg.into_bytes(),
            )
            .await
        {
            tracing::warn!(error = %e, "telegram: memory.write_turn failed");
        }
    }

    /// `memory.agent_read` — returns the agent + user
    /// memory blobs for the subject. Errors return empty
    /// blobs so /memory still renders a placeholder.
    pub async fn memory_agent_read(&self, subject_id: &str) -> (String, String) {
        let body = match self
            .call_text(
                &self.memory_alias,
                "memory.agent_read",
                self.memory_deadline_secs,
                subject_id.as_bytes().to_vec(),
            )
            .await
        {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "telegram: memory.agent_read failed");
                return (String::new(), String::new());
            }
        };
        // Wire: `agent_bytes=N|user_bytes=M\n<agent><user>`
        let (header, payload) = match body.split_once('\n') {
            Some(t) => t,
            None => return (String::new(), String::new()),
        };
        let mut agent_bytes: usize = 0;
        let mut user_bytes: usize = 0;
        for kv in header.split('|') {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "agent_bytes" => agent_bytes = v.parse().unwrap_or(0),
                    "user_bytes" => user_bytes = v.parse().unwrap_or(0),
                    _ => {}
                }
            }
        }
        let agent = payload.chars().take(agent_bytes).collect::<String>();
        let user = payload
            .chars()
            .skip(agent_bytes)
            .take(user_bytes)
            .collect::<String>();
        (agent, user)
    }

    /// `memory.agent_write` with action=clear on both
    /// agent + user targets. Best-effort; logs and moves
    /// on if either half fails.
    pub async fn memory_agent_clear(&self, subject_id: &str) {
        for target in ["agent", "user"] {
            let arg = format!("{subject_id}|{target}|clear|");
            if let Err(e) = self
                .call(
                    &self.memory_alias,
                    "memory.agent_write",
                    self.memory_deadline_secs,
                    arg.into_bytes(),
                )
                .await
            {
                tracing::warn!(target = target, error = %e, "telegram: memory.agent_write clear failed");
            }
        }
    }

    // ── ai.chat ──────────────────────────────────────────

    /// `ai.chat` — returns the model reply text. The
    /// controller maps `None` to the operator-friendly
    /// fallback message.
    pub async fn ai_chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String> {
        let arg = format!("{session_id}|{prompt}|{history}");
        match self
            .call_text(
                &self.ai_alias,
                "ai.chat",
                self.ai_deadline_secs,
                arg.into_bytes(),
            )
            .await
        {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!(error = %e, "telegram: ai.chat failed");
                None
            }
        }
    }

    /// RELIX-7.7 GAP 2 — dispatch the chat envelope to an
    /// explicit `(peer, capability)`. Used after a
    /// `routing.resolve` call.
    pub async fn dispatch_chat(
        &self,
        peer: &str,
        capability: &str,
        session_id: &str,
        prompt: &str,
        history: &str,
    ) -> Option<String> {
        let arg = format!("{session_id}|{prompt}|{history}");
        match self
            .call_text(peer, capability, self.ai_deadline_secs, arg.into_bytes())
            .await
        {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!(
                    peer = peer,
                    capability = capability,
                    error = %e,
                    "telegram: dispatch_chat failed"
                );
                None
            }
        }
    }

    /// RELIX-7.7 GAP 2 — call the coordinator's
    /// `routing.resolve` capability. Returns `(target, cap)` on
    /// a match, `None` otherwise.
    pub async fn routing_resolve(
        &self,
        channel: &str,
        sender: &str,
        subject: &str,
        content: &str,
    ) -> Option<(String, String)> {
        let body = serde_json::json!({
            "channel": channel,
            "sender": sender,
            "subject": subject,
            "content": content,
        });
        let bytes = serde_json::to_vec(&body).ok()?;
        let resp = match self
            .call_text(
                &self.coord_alias,
                "routing.resolve",
                self.coord_deadline_secs,
                bytes,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "telegram: routing.resolve unreachable");
                return None;
            }
        };
        let parsed: serde_json::Value = serde_json::from_str(&resp).ok()?;
        let decision = parsed.get("decision")?;
        if decision.is_null() {
            return None;
        }
        let target = decision.get("target_agent")?.as_str()?.to_string();
        let cap = decision.get("capability")?.as_str()?.to_string();
        Some((target, cap))
    }

    // ── task.* ───────────────────────────────────────────

    /// `task.create` — returns the new `task_id` on success.
    pub async fn task_create(
        &self,
        title: &str,
        flow_template: &str,
        params_json: &str,
        owner_subject_id: &str,
    ) -> Option<String> {
        // Trim the title to avoid surprises from chat messages
        // pasted verbatim; the coordinator's title column is
        // just text so this is purely cosmetic.
        let title_clean = title.replace(['|', '\t', '\r', '\n'], " ");
        let arg =
            format!("{title_clean}|{flow_template}|{params_json}|{owner_subject_id}||||telegram");
        match self
            .call_text(
                &self.coord_alias,
                "task.create",
                self.coord_deadline_secs,
                arg.into_bytes(),
            )
            .await
        {
            Ok(id) => Some(id.trim().to_string()),
            Err(e) => {
                tracing::warn!(error = %e, "telegram: task.create failed");
                None
            }
        }
    }

    /// `task.update` — best-effort status flip.
    pub async fn task_update_status(&self, task_id: &str, status: &str, result: &str) {
        let arg = format!("{task_id}|{status}|{result}");
        if let Err(e) = self
            .call(
                &self.coord_alias,
                "task.update",
                self.coord_deadline_secs,
                arg.into_bytes(),
            )
            .await
        {
            tracing::warn!(error = %e, "telegram: task.update failed");
        }
    }

    /// FIX 7: `coord.approval.get` — fetch an approval row
    /// by id. Returns the parsed JSON body or `None` on
    /// transport / parse failure (the cap returns INVALID_ARGS
    /// "not found" for missing ids, which surfaces here as a
    /// `Mesh` error → mapped to `None` by the caller).
    pub async fn approval_get(&self, approval_id: &str) -> Option<serde_json::Value> {
        match self
            .call_text(
                &self.coord_alias,
                "coord.approval.get",
                self.coord_deadline_secs,
                approval_id.as_bytes().to_vec(),
            )
            .await
        {
            Ok(body) => match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(error = %e, "telegram: coord.approval.get decode failed");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "telegram: coord.approval.get failed");
                None
            }
        }
    }

    /// FIX 7: `approval.record_decision` — flip the row + fire
    /// the escalation cancel signal via the documented
    /// coordinator cap (NOT the legacy `coord.approval.decide`
    /// path). Wire args are `{ approval_id, decision, note }`
    /// JSON per `approval/caps.rs::DecisionArgs`.
    pub async fn approval_record_decision(
        &self,
        approval_id: &str,
        decision: &str,
        note: &str,
    ) -> Option<String> {
        let body = serde_json::json!({
            "approval_id": approval_id,
            "decision": decision,
            "note": note,
        });
        let payload = match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "telegram: approval.record_decision encode failed");
                return None;
            }
        };
        match self
            .call_text(
                &self.coord_alias,
                "approval.record_decision",
                self.coord_deadline_secs,
                payload,
            )
            .await
        {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!(error = %e, "telegram: approval.record_decision failed");
                None
            }
        }
    }

    /// `coord.approval.decide` — approve or reject a pending
    /// approval. Returns the response body verbatim
    /// (`ok\n` for reject, `ok|<token>\n` for approve).
    pub async fn approval_decide(
        &self,
        approval_id: &str,
        decision: &str,
        decided_by: &str,
        note: &str,
    ) -> Option<String> {
        let arg = format!("{approval_id}|{decision}|{decided_by}|{note}");
        match self
            .call_text(
                &self.coord_alias,
                "coord.approval.decide",
                self.coord_deadline_secs,
                arg.into_bytes(),
            )
            .await
        {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!(error = %e, "telegram: coord.approval.decide failed");
                None
            }
        }
    }

    /// `task.event` — append a chronicle event. Best-effort.
    pub async fn task_event(&self, task_id: &str, event_type: &str, payload: &str) {
        let arg = format!("{task_id}|{event_type}|{payload}");
        if let Err(e) = self
            .call(
                &self.coord_alias,
                "task.event",
                self.coord_deadline_secs,
                arg.into_bytes(),
            )
            .await
        {
            tracing::warn!(error = %e, "telegram: task.event failed");
        }
    }

    /// `tool.audio.transcribe` — sends the raw audio bytes
    /// (no encoding; the tool node treats the args as binary)
    /// and returns the transcript string. Wire shape: the
    /// audio handler's documented `text=<utf8>` reply is
    /// parsed here so the caller just gets the transcribed
    /// text.
    pub async fn tool_audio_transcribe(&self, audio_bytes: Vec<u8>) -> Option<String> {
        let alias = self.audio_alias.as_deref()?;
        match self
            .call_text(
                alias,
                "tool.audio.transcribe",
                self.audio_deadline_secs,
                audio_bytes,
            )
            .await
        {
            Ok(body) => {
                let trimmed = body.trim();
                let text = trimmed
                    .strip_prefix("text=")
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| trimmed.to_string());
                if text.is_empty() { None } else { Some(text) }
            }
            Err(e) => {
                tracing::warn!(error = %e, "telegram: tool.audio.transcribe failed");
                None
            }
        }
    }

    /// `task.list` paged scan, filtered by status. Returns
    /// `(task_id, status, title)` rows.
    pub async fn task_list(
        &self,
        status_filter: Option<&str>,
        limit: usize,
    ) -> Vec<(String, String, String)> {
        let status = status_filter.unwrap_or("");
        let arg = format!("{limit}|0|{status}");
        let body = match self
            .call_text(
                &self.coord_alias,
                "task.list",
                self.coord_deadline_secs,
                arg.into_bytes(),
            )
            .await
        {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "telegram: task.list failed");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for line in body.lines() {
            let cols: Vec<&str> = line.splitn(3, '\t').collect();
            if cols.len() == 3 {
                out.push((cols[0].into(), cols[1].into(), cols[2].into()));
            }
        }
        out
    }
}

/// Trait pass-through so the controller can hold any
/// `TelegramOutbound` (production: the live client; tests:
/// scripted stub).
#[async_trait]
impl TelegramOutbound for TelegramOutboundClient {
    async fn memory_recent(&self, session_id: &str, n: usize) -> Vec<(String, String)> {
        TelegramOutboundClient::memory_recent(self, session_id, n).await
    }
    async fn memory_write(&self, session_id: &str, role: &str, text: &str) {
        TelegramOutboundClient::memory_write(self, session_id, role, text).await
    }
    async fn memory_agent_read(&self, subject_id: &str) -> (String, String) {
        TelegramOutboundClient::memory_agent_read(self, subject_id).await
    }
    async fn memory_agent_clear(&self, subject_id: &str) {
        TelegramOutboundClient::memory_agent_clear(self, subject_id).await
    }
    async fn ai_chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String> {
        TelegramOutboundClient::ai_chat(self, session_id, prompt, history).await
    }
    async fn dispatch_chat(
        &self,
        peer: &str,
        capability: &str,
        session_id: &str,
        prompt: &str,
        history: &str,
    ) -> Option<String> {
        TelegramOutboundClient::dispatch_chat(self, peer, capability, session_id, prompt, history)
            .await
    }
    async fn routing_resolve(
        &self,
        channel: &str,
        sender: &str,
        subject: &str,
        content: &str,
    ) -> Option<(String, String)> {
        TelegramOutboundClient::routing_resolve(self, channel, sender, subject, content).await
    }
    async fn task_create(
        &self,
        title: &str,
        flow_template: &str,
        params_json: &str,
        owner_subject_id: &str,
    ) -> Option<String> {
        TelegramOutboundClient::task_create(
            self,
            title,
            flow_template,
            params_json,
            owner_subject_id,
        )
        .await
    }
    async fn task_update_status(&self, task_id: &str, status: &str, result: &str) {
        TelegramOutboundClient::task_update_status(self, task_id, status, result).await
    }
    async fn task_event(&self, task_id: &str, event_type: &str, payload: &str) {
        TelegramOutboundClient::task_event(self, task_id, event_type, payload).await
    }
    async fn task_list(
        &self,
        status_filter: Option<&str>,
        limit: usize,
    ) -> Vec<(String, String, String)> {
        TelegramOutboundClient::task_list(self, status_filter, limit).await
    }
    async fn approval_decide(
        &self,
        approval_id: &str,
        decision: &str,
        decided_by: &str,
        note: &str,
    ) -> Option<String> {
        TelegramOutboundClient::approval_decide(self, approval_id, decision, decided_by, note).await
    }
    async fn approval_get(&self, approval_id: &str) -> Option<serde_json::Value> {
        TelegramOutboundClient::approval_get(self, approval_id).await
    }
    async fn approval_record_decision(
        &self,
        approval_id: &str,
        decision: &str,
        note: &str,
    ) -> Option<String> {
        TelegramOutboundClient::approval_record_decision(self, approval_id, decision, note).await
    }
    async fn tool_audio_transcribe(&self, audio_bytes: Vec<u8>) -> Option<String> {
        TelegramOutboundClient::tool_audio_transcribe(self, audio_bytes).await
    }
}

#[cfg(test)]
mod tests {
    // Outbound dispatcher behaviour is exercised in the
    // controller-level integration tests where a stub
    // `TelegramOutbound` trait double replays scripted
    // responses; the live dispatcher itself is a thin pipe
    // over the mesh and doesn't carry test-worthy logic in
    // isolation.
}
