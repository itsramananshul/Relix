//! Outbound RPC client for the discord controller. Wraps one
//! [`MeshClient`] + [`Bundle`] over which the controller
//! dispatches `memory.*`, `ai.chat`, and `task.*` calls.
//! Mirror of nodes/telegram/client.rs.

use std::sync::Arc;

use async_trait::async_trait;
use relix_core::bundle::Bundle;
use tokio::sync::OnceCell;

use crate::dispatch::{build_request, decode_response};
use crate::manifest::MeshClient;
use crate::transport::envelope::ResponseResult;

#[async_trait]
pub trait DiscordOutbound: Send + Sync + 'static {
    async fn memory_recent(&self, session_id: &str, n: usize) -> Vec<(String, String)>;
    async fn memory_write(&self, session_id: &str, role: &str, text: &str);
    async fn memory_agent_read(&self, subject_id: &str) -> (String, String);
    async fn memory_agent_clear(&self, subject_id: &str);
    async fn ai_chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String>;
    /// RELIX-7.7 GAP 2 — dispatch the chat envelope to an
    /// explicit `(peer, capability)`. Defaults to `ai_chat`.
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
    /// RELIX-7.7 GAP 2 — call `routing.resolve` on the
    /// coordinator. `None` means no rule matched / unreachable.
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
}

/// Lazily-populated outbound client. The controller's polling
/// loop pulls a clone from this cell on every tick; while empty
/// the loop posts a static "I'm not wired to the mesh yet" reply.
pub type DiscordOutboundClientCell = Arc<OnceCell<Arc<DiscordOutboundClient>>>;

pub struct DiscordOutboundClient {
    pub mesh: MeshClient,
    pub identity: Bundle,
    pub memory_alias: String,
    pub memory_deadline_secs: i64,
    pub ai_alias: String,
    pub ai_deadline_secs: i64,
    pub coord_alias: String,
    pub coord_deadline_secs: i64,
}

#[derive(Debug)]
pub enum OutboundError {
    Mesh(String),
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

impl DiscordOutboundClient {
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
                tracing::warn!(error = %e, "discord: memory.recent_for_session failed");
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

    pub async fn memory_write(&self, session_id: &str, role: &str, text: &str) {
        let sanitised: String = text
            .chars()
            .map(|c| match c {
                '|' | '\r' | '\n' | '\t' => ' ',
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
            tracing::warn!(error = %e, "discord: memory.write_turn failed");
        }
    }

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
                tracing::warn!(error = %e, "discord: memory.agent_read failed");
                return (String::new(), String::new());
            }
        };
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
        let agent: String = payload.chars().take(agent_bytes).collect();
        let user: String = payload.chars().skip(agent_bytes).take(user_bytes).collect();
        (agent, user)
    }

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
                tracing::warn!(target = target, error = %e, "discord: memory.agent_write clear failed");
            }
        }
    }

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
                tracing::warn!(error = %e, "discord: ai.chat failed");
                None
            }
        }
    }

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
                    "discord: dispatch_chat failed"
                );
                None
            }
        }
    }

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
                tracing::debug!(error = %e, "discord: routing.resolve unreachable");
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

    pub async fn task_create(
        &self,
        title: &str,
        flow_template: &str,
        params_json: &str,
        owner_subject_id: &str,
    ) -> Option<String> {
        let title_clean = title.replace(['|', '\t', '\r', '\n'], " ");
        // origin_surface=discord — the task.create wire format is
        // `title|flow_template|params_json|owner_subject_id||||<origin>`.
        let arg =
            format!("{title_clean}|{flow_template}|{params_json}|{owner_subject_id}||||discord");
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
                tracing::warn!(error = %e, "discord: task.create failed");
                None
            }
        }
    }

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
            tracing::warn!(error = %e, "discord: task.update failed");
        }
    }

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
            tracing::warn!(error = %e, "discord: task.event failed");
        }
    }
}

#[async_trait]
impl DiscordOutbound for DiscordOutboundClient {
    async fn memory_recent(&self, session_id: &str, n: usize) -> Vec<(String, String)> {
        DiscordOutboundClient::memory_recent(self, session_id, n).await
    }
    async fn memory_write(&self, session_id: &str, role: &str, text: &str) {
        DiscordOutboundClient::memory_write(self, session_id, role, text).await
    }
    async fn memory_agent_read(&self, subject_id: &str) -> (String, String) {
        DiscordOutboundClient::memory_agent_read(self, subject_id).await
    }
    async fn memory_agent_clear(&self, subject_id: &str) {
        DiscordOutboundClient::memory_agent_clear(self, subject_id).await
    }
    async fn ai_chat(&self, session_id: &str, prompt: &str, history: &str) -> Option<String> {
        DiscordOutboundClient::ai_chat(self, session_id, prompt, history).await
    }
    async fn dispatch_chat(
        &self,
        peer: &str,
        capability: &str,
        session_id: &str,
        prompt: &str,
        history: &str,
    ) -> Option<String> {
        DiscordOutboundClient::dispatch_chat(self, peer, capability, session_id, prompt, history)
            .await
    }
    async fn routing_resolve(
        &self,
        channel: &str,
        sender: &str,
        subject: &str,
        content: &str,
    ) -> Option<(String, String)> {
        DiscordOutboundClient::routing_resolve(self, channel, sender, subject, content).await
    }
    async fn task_create(
        &self,
        title: &str,
        flow_template: &str,
        params_json: &str,
        owner_subject_id: &str,
    ) -> Option<String> {
        DiscordOutboundClient::task_create(
            self,
            title,
            flow_template,
            params_json,
            owner_subject_id,
        )
        .await
    }
    async fn task_update_status(&self, task_id: &str, status: &str, result: &str) {
        DiscordOutboundClient::task_update_status(self, task_id, status, result).await
    }
    async fn task_event(&self, task_id: &str, event_type: &str, payload: &str) {
        DiscordOutboundClient::task_event(self, task_id, event_type, payload).await
    }
}
