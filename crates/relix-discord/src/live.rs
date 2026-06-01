//! Live HTTPS implementation of [`DiscordApi`] backed by `reqwest`
//! + rustls. No openssl, no native-tls.
//!
//! Wire posture:
//!
//! - Base URL: `https://discord.com/api/v10`.
//! - Authorization header: `Bot <token>` (NOT `Bearer`).
//! - `get_me`   → `GET /users/@me`.
//! - `get_messages` → `GET /channels/{channel_id}/messages?after={id}&limit=50`.
//! - `send_message` → `POST /channels/{channel_id}/messages` with
//!   `{"content": "...", "message_reference": {...}}` for replies.
//! - `send_typing` → `POST /channels/{channel_id}/typing` (no body).
//! - `delete_message` → `DELETE /channels/{channel_id}/messages/{id}`.
//!
//! Retry posture:
//!
//! - 429 → honour `retry_after` (a FLOAT in seconds — Discord
//!   differs from Telegram which uses integer); ceiling 30s.
//! - 5xx → exponential backoff (1s, 2s, 4s — max 3 retries).
//! - Other 4xx → never retried (config / permissions problem).

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{DiscordApi, DiscordApiError, IncomingMessage, OutgoingMessage};

/// The bot's own identity as reported by `GET /users/@me`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BotIdentity {
    pub user_id: String,
    pub username: String,
}

/// Default API root. Exposed for tests via
/// `LiveDiscordApi::with_base_url`.
pub const DEFAULT_API_BASE: &str = "https://discord.com/api/v10";

const MAX_RETRIES: u32 = 3;
const PER_CALL_TIMEOUT_SECS: u64 = 30;
/// Discord's `retry_after` is a float in seconds. We clamp to this
/// ceiling so a misbehaving server can't pin the controller for
/// minutes.
const MAX_RETRY_AFTER_SECS: u64 = 30;
/// Per-poll batch cap. 50 keeps the JSON response small and matches
/// the Discord default; a poll cycle that returns 50 messages also
/// suggests the controller is behind, so it should keep up rather
/// than buffer more.
const DEFAULT_FETCH_LIMIT: u32 = 50;

#[derive(Clone)]
pub struct LiveDiscordApi {
    http: reqwest::Client,
    base_url: String,
    token: String,
    /// FIX 50: optional proactive rate-limit tracker.
    rate_limiter: Option<relix_core::channel_rate_limit::ChannelRateLimiter>,
}

impl LiveDiscordApi {
    /// New client pointed at `https://discord.com/api/v10`.
    pub fn new(token: String) -> Self {
        Self::with_base_url(token, DEFAULT_API_BASE.into())
    }

    /// New client pointed at an arbitrary base URL. Used by tests
    /// that spin a localhost server emulating Discord's REST
    /// surface.
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
    /// `ChannelRateLimiter::new(DISCORD_PER_CHANNEL, Some(DISCORD_GLOBAL), clock)`
    /// to honour the documented 5/s per-channel + 50/s global
    /// caps.
    pub fn with_rate_limiter(
        mut self,
        limiter: relix_core::channel_rate_limit::ChannelRateLimiter,
    ) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    /// FIX 50: per-channel rate-limit gate. Called before
    /// every outbound REST POST.
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
        format!("Bot {}", self.token)
    }

    /// Drive a request with the retry / 429 / 5xx loop. Generic
    /// over the response body so a single helper covers GET / POST /
    /// DELETE.
    async fn request<T>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
        expect_body: bool,
    ) -> Result<T, DiscordApiError>
    where
        T: serde::de::DeserializeOwned + Default,
    {
        let url = format!("{}{}", self.base_url, path);
        let mut attempt: u32 = 0;
        loop {
            let mut req = self
                .http
                .request(method.clone(), &url)
                .header(reqwest::header::AUTHORIZATION, self.auth_header())
                .header(
                    reqwest::header::USER_AGENT,
                    concat!("Relix (relix.local, ", env!("CARGO_PKG_VERSION"), ")"),
                );
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    if attempt >= MAX_RETRIES {
                        return Err(DiscordApiError::Transient(format!(
                            "{} {path}: network error after {MAX_RETRIES} retries: {e}",
                            method.as_str()
                        )));
                    }
                    backoff(attempt).await;
                    attempt += 1;
                    continue;
                }
            };
            let status = resp.status();
            if status.is_success() {
                if !expect_body {
                    return Ok(T::default());
                }
                return match resp.json::<T>().await {
                    Ok(v) => Ok(v),
                    Err(e) => Err(DiscordApiError::Transient(format!(
                        "{} {path}: decode: {e}",
                        method.as_str()
                    ))),
                };
            }
            let body_text = resp.text().await.unwrap_or_default();
            if status.as_u16() == 429 {
                let parsed: Option<DcRateLimit> = serde_json::from_str(&body_text).ok();
                let secs = parsed
                    .as_ref()
                    .map(|p| p.retry_after.max(0.0))
                    .unwrap_or(1.0);
                let clamped = (secs.ceil() as u64).clamp(1, MAX_RETRY_AFTER_SECS);
                if attempt >= MAX_RETRIES {
                    return Err(DiscordApiError::Transient(format!(
                        "{} {path}: 429 retry_after={secs:.2}s after {MAX_RETRIES} retries",
                        method.as_str()
                    )));
                }
                tokio::time::sleep(Duration::from_secs(clamped)).await;
                attempt += 1;
                continue;
            }
            if status.is_server_error() {
                if attempt >= MAX_RETRIES {
                    return Err(DiscordApiError::Transient(format!(
                        "{} {path}: {} after {MAX_RETRIES} retries",
                        method.as_str(),
                        status.as_u16()
                    )));
                }
                backoff(attempt).await;
                attempt += 1;
                continue;
            }
            // Other 4xx — surface verbatim.
            let parsed: Option<DcErrorEnvelope> = serde_json::from_str(&body_text).ok();
            let msg = parsed
                .and_then(|e| e.message)
                .unwrap_or_else(|| body_text.clone());
            return Err(DiscordApiError::ClientError(format!(
                "{} {path}: {} {msg}",
                method.as_str(),
                status.as_u16()
            )));
        }
    }
}

async fn backoff(attempt: u32) {
    let base_ms = 1000u64.checked_shl(attempt).unwrap_or(8000).min(8000);
    tokio::time::sleep(Duration::from_millis(base_ms)).await;
}

// ── Wire envelopes ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DcRateLimit {
    #[serde(default)]
    retry_after: f64,
}

#[derive(Debug, Deserialize)]
struct DcErrorEnvelope {
    #[serde(default)]
    message: Option<String>,
}

/// Default-returning placeholder for endpoints that don't return a
/// useful JSON body (typing, delete). Lets the generic helper stay
/// uniform without forcing every call site to declare a phantom.
#[derive(Debug, Default, Deserialize)]
struct EmptyResponse;

// ── getMe ────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
struct DcUser {
    id: String,
    #[serde(default)]
    username: Option<String>,
}

// ── getMessages ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct DcMessage {
    id: String,
    channel_id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    author: Option<DcAuthor>,
}

#[derive(Debug, Deserialize)]
struct DcAuthor {
    id: String,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    bot: bool,
}

/// PART 3: lift one Discord message to the channel's
/// [`IncomingMessage`] shape, OR drop it when the author is a
/// bot.
///
/// Filtering at the parse layer (rather than at every call site)
/// is the only safe shape — if any downstream caller forgets the
/// `is_bot` check, the bot can end up replying to its own message
/// in a tight loop. Returning `Option` here makes the filter
/// invariant local to the wire-decode path.
fn dc_message_to_incoming(m: DcMessage) -> Option<IncomingMessage> {
    let (user_id, username, is_bot) = match m.author {
        Some(a) => (a.id, a.username.unwrap_or_default(), a.bot),
        None => (String::new(), String::new(), false),
    };
    if is_bot {
        return None;
    }
    Some(IncomingMessage {
        message_id: m.id,
        channel_id: m.channel_id,
        user_id,
        username,
        is_bot,
        content: m.content,
    })
}

// ── sendMessage ──────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct DcSendMessage<'a> {
    content: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_reference: Option<DcMessageReference<'a>>,
    /// PART 3: Discord components array. Approval messages
    /// stamp this with an Action Row carrying two buttons; the
    /// buttons' `custom_id` encodes the approval id.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    components: &'a [serde_json::Value],
}

#[derive(Debug, Serialize)]
struct DcMessageReference<'a> {
    message_id: &'a str,
}

#[async_trait]
impl DiscordApi for LiveDiscordApi {
    async fn get_me(&self) -> Result<BotIdentity, DiscordApiError> {
        let user: DcUser = self
            .request(reqwest::Method::GET, "/users/@me", None, true)
            .await?;
        Ok(BotIdentity {
            user_id: user.id,
            username: user.username.unwrap_or_default(),
        })
    }

    async fn bootstrap_watermark(
        &self,
        channel_id: &str,
    ) -> Result<Option<String>, DiscordApiError> {
        // Fetch just the most-recent message; we don't surface
        // its content, only its snowflake. limit=1 keeps the
        // payload tiny.
        let path = format!("/channels/{channel_id}/messages?limit=1");
        let raw: Vec<DcMessage> = self
            .request(reqwest::Method::GET, &path, None, true)
            .await?;
        Ok(raw.into_iter().next().map(|m| m.id))
    }

    async fn get_messages(
        &self,
        channel_id: &str,
        after_message_id: &str,
    ) -> Result<Vec<IncomingMessage>, DiscordApiError> {
        let path = if after_message_id.is_empty() {
            format!("/channels/{channel_id}/messages?limit={DEFAULT_FETCH_LIMIT}")
        } else {
            format!(
                "/channels/{channel_id}/messages?after={after_message_id}&limit={DEFAULT_FETCH_LIMIT}"
            )
        };
        let raw: Vec<DcMessage> = self
            .request(reqwest::Method::GET, &path, None, true)
            .await?;
        // Discord returns newest-first. Reverse so the controller
        // processes in chronological order — matches Telegram's
        // long-poll contract. PART 3: `dc_message_to_incoming`
        // now returns `Option` so bot-authored messages are
        // dropped at the parse layer; we filter_map before the
        // reverse so the chronological order survives.
        let mut out: Vec<IncomingMessage> =
            raw.into_iter().filter_map(dc_message_to_incoming).collect();
        out.reverse();
        Ok(out)
    }

    async fn send_message(&self, out: &OutgoingMessage) -> Result<(), DiscordApiError> {
        // FIX 50: per-channel rate-limit gate.
        self.rate_limit_acquire(&out.channel_id).await;
        let path = format!("/channels/{}/messages", out.channel_id);
        let body = serde_json::to_value(DcSendMessage {
            content: &out.content,
            message_reference: if out.reply_to_message_id.is_empty() {
                None
            } else {
                Some(DcMessageReference {
                    message_id: &out.reply_to_message_id,
                })
            },
            components: &out.components,
        })
        .map_err(|e| DiscordApiError::Transient(format!("sendMessage build body: {e}")))?;
        let _: EmptyResponse = self
            .request(reqwest::Method::POST, &path, Some(&body), false)
            .await?;
        Ok(())
    }

    async fn send_typing(&self, channel_id: &str) -> Result<(), DiscordApiError> {
        // FIX 50: per-channel rate-limit gate.
        self.rate_limit_acquire(channel_id).await;
        let path = format!("/channels/{channel_id}/typing");
        let _: EmptyResponse = self
            .request(reqwest::Method::POST, &path, None, false)
            .await?;
        Ok(())
    }

    async fn delete_message(
        &self,
        channel_id: &str,
        message_id: &str,
    ) -> Result<(), DiscordApiError> {
        let path = format!("/channels/{channel_id}/messages/{message_id}");
        let _: EmptyResponse = self
            .request(reqwest::Method::DELETE, &path, None, false)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_decode_strips_optional_author() {
        let raw = serde_json::json!({
            "id": "9000",
            "channel_id": "100",
            "content": "hi",
            "author": { "id": "42", "username": "alice", "bot": false }
        });
        let m: DcMessage = serde_json::from_value(raw).unwrap();
        let inc = dc_message_to_incoming(m).expect("non-bot author must surface");
        assert_eq!(inc.message_id, "9000");
        assert_eq!(inc.channel_id, "100");
        assert_eq!(inc.user_id, "42");
        assert_eq!(inc.username, "alice");
        assert!(!inc.is_bot);
        assert_eq!(inc.content, "hi");
    }

    #[test]
    fn message_decode_handles_missing_author() {
        let raw = serde_json::json!({
            "id": "9000",
            "channel_id": "100",
            "content": "system message"
        });
        let m: DcMessage = serde_json::from_value(raw).unwrap();
        let inc = dc_message_to_incoming(m).expect("missing author => non-bot");
        assert!(inc.user_id.is_empty());
        assert!(!inc.is_bot);
    }

    #[test]
    fn bot_authored_message_is_dropped_at_parse_layer() {
        let raw = serde_json::json!({
            "id": "9000",
            "channel_id": "100",
            "content": "I am a bot",
            "author": { "id": "999", "username": "relixbot", "bot": true }
        });
        let m: DcMessage = serde_json::from_value(raw).unwrap();
        assert!(
            dc_message_to_incoming(m).is_none(),
            "bot-authored messages must NEVER reach IncomingMessage — otherwise the \
             channel can reply to itself in a loop"
        );
    }

    #[test]
    fn backoff_is_capped() {
        let v = 1000u64.checked_shl(10).unwrap_or(8000).min(8000);
        assert_eq!(v, 8000);
    }
}
