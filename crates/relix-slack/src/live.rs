//! Live HTTPS implementation of [`SlackApi`] backed by `reqwest`
//! + rustls. No openssl, no native-tls.
//!
//! Wire posture:
//!
//! - Base URL: `https://slack.com/api/`.
//! - `Authorization: Bearer xoxb-...` header.
//! - All Web API methods are POST. Body type is
//!   `application/json` (Slack's modern API accepts it; the
//!   older `application/x-www-form-urlencoded` flavour is not
//!   used here).
//! - `auth.test` → bot identity (user_id, team_id, bot_id, user).
//! - `conversations.history` → channel messages with `oldest`
//!   cursor and `limit=50`.
//! - `chat.postMessage` → send a text reply, optionally threaded.
//! - `chat.update` → in-place edit by `(channel, ts)`.
//!
//! Error model: **Slack returns HTTP 200 even on errors**, with
//! `ok: false` and an `error` string in the body. The generic
//! request helper inspects `ok` and maps `false` to
//! `SlackApiError::ClientError`. HTTP 4xx is rare in practice
//! (the gateway only returns it for completely malformed
//! requests) but handled the same way.
//!
//! Retry posture:
//!
//! - 429 → honour `Retry-After` header (integer seconds, clamped
//!   1..30s).
//! - 5xx → exponential backoff (1s, 2s, 4s — max 3 retries).
//! - `ok=false` → not retried (config / permissions problem the
//!   operator must fix — same posture as a non-retryable 4xx).

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{IncomingMessage, OutgoingMessage, SlackApi, SlackApiError};

/// The bot's own identity as reported by `auth.test`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BotIdentity {
    pub user_id: String,
    pub team_id: String,
    pub bot_id: String,
    pub username: String,
}

/// Default API root.
pub const DEFAULT_API_BASE: &str = "https://slack.com/api";

const MAX_RETRIES: u32 = 3;
const PER_CALL_TIMEOUT_SECS: u64 = 30;
const MAX_RETRY_AFTER_SECS: u64 = 30;
const DEFAULT_FETCH_LIMIT: u32 = 50;

#[derive(Clone)]
pub struct LiveSlackApi {
    http: reqwest::Client,
    base_url: String,
    token: String,
    /// FIX 50: optional proactive rate-limit tracker.
    /// `chat_post_message` / `chat_update` calls
    /// `acquire(channel).await` before issuing the HTTP
    /// request.
    rate_limiter: Option<relix_core::channel_rate_limit::ChannelRateLimiter>,
}

impl LiveSlackApi {
    pub fn new(token: String) -> Self {
        Self::with_base_url(token, DEFAULT_API_BASE.into())
    }

    pub fn with_base_url(token: String, base_url: String) -> Self {
        let base = base_url.trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(PER_CALL_TIMEOUT_SECS))
            .build()
            .expect("reqwest::Client::builder succeeds with default config");
        Self {
            http,
            base_url: base,
            token,
            rate_limiter: None,
        }
    }

    /// FIX 50: install a proactive rate-limit tracker. Pass
    /// `ChannelRateLimiter::new(SLACK_PER_CHANNEL, None, clock)`
    /// to honour the Tier 3 1-msg/s/channel cap.
    pub fn with_rate_limiter(
        mut self,
        limiter: relix_core::channel_rate_limit::ChannelRateLimiter,
    ) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    /// FIX 50: per-channel rate-limit gate.
    async fn rate_limit_acquire(&self, channel: &str) {
        if let Some(lim) = self.rate_limiter.as_ref() {
            let state = lim.acquire(channel).await;
            if matches!(
                state,
                relix_core::channel_rate_limit::RateLimitState::Throttled
            ) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    /// Drive a request with the retry / 429 / 5xx / ok=false
    /// loop. Generic over the response body so a single helper
    /// covers every method.
    async fn request<T>(&self, method: &str, body: &serde_json::Value) -> Result<T, SlackApiError>
    where
        T: serde::de::DeserializeOwned,
    {
        let url = format!("{}/{method}", self.base_url);
        let mut attempt: u32 = 0;
        loop {
            let resp = self
                .http
                .post(&url)
                .header(reqwest::header::AUTHORIZATION, self.auth_header())
                .header(
                    reqwest::header::USER_AGENT,
                    concat!("Relix (relix.local, ", env!("CARGO_PKG_VERSION"), ")"),
                )
                .json(body)
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    if attempt >= MAX_RETRIES {
                        return Err(SlackApiError::Transient(format!(
                            "{method}: network error after {MAX_RETRIES} retries: {e}"
                        )));
                    }
                    backoff(attempt).await;
                    attempt += 1;
                    continue;
                }
            };
            let status = resp.status();
            if status.as_u16() == 429 {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(1);
                let clamped = retry_after.clamp(1, MAX_RETRY_AFTER_SECS);
                if attempt >= MAX_RETRIES {
                    return Err(SlackApiError::Transient(format!(
                        "{method}: 429 retry_after={retry_after}s after {MAX_RETRIES} retries"
                    )));
                }
                tokio::time::sleep(Duration::from_secs(clamped)).await;
                attempt += 1;
                continue;
            }
            if status.is_server_error() {
                if attempt >= MAX_RETRIES {
                    return Err(SlackApiError::Transient(format!(
                        "{method}: {} after {MAX_RETRIES} retries",
                        status.as_u16()
                    )));
                }
                backoff(attempt).await;
                attempt += 1;
                continue;
            }
            // 200 or non-429 4xx — try to parse as Slack envelope.
            let body_text = resp.text().await.unwrap_or_default();
            // Check for ok=false envelope before attempting the
            // T-typed decode. We don't model every Slack response
            // shape; the envelope is the failure carrier and the
            // payload is whatever T expects.
            if let Ok(env) = serde_json::from_str::<SlackEnvelope>(&body_text)
                && !env.ok
            {
                let err = env.error.unwrap_or_else(|| "(no error field)".into());
                return Err(SlackApiError::ClientError(format!("{method}: {err}")));
            }
            if !status.is_success() {
                return Err(SlackApiError::ClientError(format!(
                    "{method}: HTTP {} {body_text}",
                    status.as_u16()
                )));
            }
            return match serde_json::from_str::<T>(&body_text) {
                Ok(v) => Ok(v),
                Err(e) => Err(SlackApiError::Transient(format!("{method}: decode: {e}"))),
            };
        }
    }
}

async fn backoff(attempt: u32) {
    let base_ms = 1000u64.checked_shl(attempt).unwrap_or(8000).min(8000);
    tokio::time::sleep(Duration::from_millis(base_ms)).await;
}

// ── Wire envelopes ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SlackEnvelope {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

// ── auth.test ────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct SlackAuthTest {
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    team_id: String,
    #[serde(default)]
    bot_id: String,
    #[serde(default)]
    user: String,
}

// ── conversations.history ────────────────────────────────────

#[derive(Debug, Serialize)]
struct SlackHistoryReq<'a> {
    channel: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    oldest: &'a str,
    limit: u32,
}

#[derive(Debug, Default, Deserialize)]
struct SlackHistoryResp {
    #[serde(default)]
    messages: Vec<SlackMessage>,
}

#[derive(Debug, Deserialize)]
struct SlackMessage {
    #[serde(default)]
    ts: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    text: String,
    /// `bot_message`, `channel_join`, etc. Plain user messages
    /// have no `subtype`. We filter every subtype out — they're
    /// channel events the bot shouldn't reply to.
    #[serde(default)]
    subtype: Option<String>,
    /// Bot id set on any bot-authored message (including our own
    /// replies). Skipped at parse time.
    #[serde(default)]
    bot_id: Option<String>,
    /// Slack sometimes attaches a `username` (e.g. for bot
    /// messages); we don't fetch users.info just to populate
    /// this. Empty when absent.
    #[serde(default)]
    username: Option<String>,
}

fn slack_message_to_incoming(m: SlackMessage, channel: &str) -> Option<IncomingMessage> {
    // Skip subtypes (system events).
    if m.subtype.is_some() {
        return None;
    }
    // Skip messages with a bot_id (including our own replies).
    let is_bot = m.bot_id.is_some();
    if is_bot {
        return None;
    }
    Some(IncomingMessage {
        ts: m.ts,
        channel_id: channel.to_string(),
        user_id: m.user.unwrap_or_default(),
        username: m.username.unwrap_or_default(),
        is_bot,
        text: m.text,
    })
}

// ── chat.postMessage ─────────────────────────────────────────

#[derive(Debug, Serialize)]
struct SlackPostMessage<'a> {
    channel: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    thread_ts: &'a str,
    /// PART 2: Block Kit layout. Slack accepts `blocks` as a
    /// JSON array on the same `chat.postMessage` body that
    /// carries `text`; clients that can't render blocks fall
    /// back to the `text` field, which is what makes
    /// notifications work on desktop / mobile pre-launch.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    blocks: &'a [serde_json::Value],
}

#[derive(Debug, Default, Deserialize)]
struct SlackPostResp {
    #[serde(default)]
    #[allow(dead_code)]
    ts: String,
}

// ── chat.update ──────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct SlackUpdateMessage<'a> {
    channel: &'a str,
    ts: &'a str,
    text: &'a str,
}

#[async_trait]
impl SlackApi for LiveSlackApi {
    async fn auth_test(&self) -> Result<BotIdentity, SlackApiError> {
        let res: SlackAuthTest = self
            .request("auth.test", &serde_json::Value::Object(Default::default()))
            .await?;
        Ok(BotIdentity {
            user_id: res.user_id,
            team_id: res.team_id,
            bot_id: res.bot_id,
            username: res.user,
        })
    }

    async fn conversations_history(
        &self,
        channel: &str,
        oldest: &str,
    ) -> Result<Vec<IncomingMessage>, SlackApiError> {
        let body = serde_json::to_value(SlackHistoryReq {
            channel,
            oldest,
            limit: DEFAULT_FETCH_LIMIT,
        })
        .map_err(|e| SlackApiError::Transient(format!("conversations.history body: {e}")))?;
        let resp: SlackHistoryResp = self.request("conversations.history", &body).await?;
        // Slack returns newest-first. Filter at the parse layer
        // (subtype / bot_id) then reverse so the controller sees
        // chronological order — matches the Telegram + Discord
        // contract.
        let mut out: Vec<IncomingMessage> = resp
            .messages
            .into_iter()
            .filter_map(|m| slack_message_to_incoming(m, channel))
            .collect();
        out.reverse();
        Ok(out)
    }

    async fn chat_post_message(&self, out: &OutgoingMessage) -> Result<(), SlackApiError> {
        // FIX 50: per-channel rate-limit gate.
        self.rate_limit_acquire(&out.channel_id).await;
        let body = serde_json::to_value(SlackPostMessage {
            channel: &out.channel_id,
            text: &out.text,
            thread_ts: &out.thread_ts,
            blocks: &out.blocks,
        })
        .map_err(|e| SlackApiError::Transient(format!("chat.postMessage body: {e}")))?;
        let _: SlackPostResp = self.request("chat.postMessage", &body).await?;
        Ok(())
    }

    async fn chat_update(&self, channel: &str, ts: &str, text: &str) -> Result<(), SlackApiError> {
        // FIX 50: per-channel rate-limit gate.
        self.rate_limit_acquire(channel).await;
        let body = serde_json::to_value(SlackUpdateMessage { channel, ts, text })
            .map_err(|e| SlackApiError::Transient(format!("chat.update body: {e}")))?;
        let _: SlackPostResp = self.request("chat.update", &body).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_filters_subtype_messages() {
        let raw = serde_json::json!({
            "ts": "1.0",
            "user": "U0",
            "text": "alice joined",
            "subtype": "channel_join"
        });
        let m: SlackMessage = serde_json::from_value(raw).unwrap();
        assert!(slack_message_to_incoming(m, "C0").is_none());
    }

    #[test]
    fn parse_filters_messages_with_bot_id() {
        let raw = serde_json::json!({
            "ts": "1.0",
            "text": "hello from another bot",
            "bot_id": "B0123",
            "username": "otherbot"
        });
        let m: SlackMessage = serde_json::from_value(raw).unwrap();
        assert!(slack_message_to_incoming(m, "C0").is_none());
    }

    #[test]
    fn parse_keeps_normal_user_messages() {
        let raw = serde_json::json!({
            "ts": "1700000000.000100",
            "user": "U0",
            "text": "hi",
            "username": "alice"
        });
        let m: SlackMessage = serde_json::from_value(raw).unwrap();
        let inc = slack_message_to_incoming(m, "C0").unwrap();
        assert_eq!(inc.ts, "1700000000.000100");
        assert_eq!(inc.user_id, "U0");
        assert_eq!(inc.text, "hi");
        assert!(!inc.is_bot);
    }

    #[test]
    fn parse_keeps_message_without_username() {
        let raw = serde_json::json!({
            "ts": "1.0",
            "user": "U0",
            "text": "hi"
        });
        let m: SlackMessage = serde_json::from_value(raw).unwrap();
        let inc = slack_message_to_incoming(m, "C0").unwrap();
        assert_eq!(inc.username, "");
    }

    #[test]
    fn backoff_is_capped() {
        let v = 1000u64.checked_shl(10).unwrap_or(8000).min(8000);
        assert_eq!(v, 8000);
    }

    #[test]
    fn envelope_decodes_ok_false_with_error() {
        let raw = r#"{"ok": false, "error": "invalid_auth"}"#;
        let env: SlackEnvelope = serde_json::from_str(raw).unwrap();
        assert!(!env.ok);
        assert_eq!(env.error.as_deref(), Some("invalid_auth"));
    }
}
