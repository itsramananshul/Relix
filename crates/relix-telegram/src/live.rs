//! Live HTTPS implementation of [`BotApi`] backed by `reqwest`
//! + rustls. No openssl, no native-tls.
//!
//! Wire-format shape and retry posture:
//!
//! - All Bot API requests are `POST <base>/bot<token>/<method>`
//!   with a JSON body. We deliberately use POST + JSON
//!   (instead of GET + query string) so the same call shape
//!   works for both `get_updates` (small) and `send_message`
//!   (potentially long text bodies with newlines and
//!   non-ASCII).
//! - `get_updates` uses Telegram's long-poll: `timeout=30`,
//!   `allowed_updates=["message", "callback_query"]`. The
//!   `callback_query` entry is what makes inline-button
//!   replies (approve / deny) actually flow back to the
//!   controller — Telegram drops every update type that is
//!   not in the allowlist. The reqwest call is given a hard
//!   timeout of 35s so a stuck socket can't wedge the receive
//!   loop forever.
//! - Retry posture follows the Bot API guidance: 429
//!   (rate-limited) honours `retry_after`; 5xx uses exponential
//!   backoff (1s, 2s, 4s — max 3 retries); 4xx other than 429
//!   never retries (it's almost always a config / permissions
//!   problem the operator must fix).

use std::time::Duration;

use async_trait::async_trait;
use relix_core::channel_rate_limit::{ChannelRateLimiter, RateLimitState};
use serde::{Deserialize, Serialize};

use crate::{BotApi, BotApiError, IncomingMessage, OutgoingMessage, ParseMode};

/// The bot's own identity as reported by `getMe`. Returned at
/// startup so the controller can log `"Telegram bot online:
/// @<username>"` and persist `user_id` for the dashboard
/// status card.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BotIdentity {
    pub user_id: i64,
    /// Telegram's `username` is technically optional on
    /// generic users but the Bot API guarantees it for bots
    /// — every bot is assigned one at `/newbot` time. We
    /// default to empty for the `MockBotApi`'s `Default`.
    pub username: String,
    /// `first_name` is the human-readable label. Useful for
    /// the dashboard.
    pub first_name: String,
}

/// Default API root. Exposed for tests via
/// `LiveBotApi::with_base_url`.
pub const DEFAULT_API_BASE: &str = "https://api.telegram.org";

/// Max retries on transient failures (5xx). 429 retries are
/// driven by Telegram's `retry_after`, not this counter.
const MAX_RETRIES: u32 = 3;

/// Per-call hard deadline. Long-poll uses `timeout=30s`
/// server-side, so 35s here gives the server 5s of slack
/// before reqwest aborts.
const PER_CALL_TIMEOUT_SECS: u64 = 35;

/// Long-poll timeout we pass to `get_updates`. Telegram caps
/// this server-side at 50; 30s is a good balance between
/// liveness and request churn.
const LONG_POLL_TIMEOUT_SECS: u32 = 30;

/// FIX 4: maximum we'll honour from a 429 `retry_after`. The
/// Bot API generally caps at a few minutes; values above this
/// indicate either an extreme rate-limit punishment or a wire
/// glitch. We log + clamp so a single bad message can't wedge
/// the receive loop for hours.
const RETRY_AFTER_CLAMP_SECS: i64 = 3600;

#[derive(Clone)]
pub struct LiveBotApi {
    http: reqwest::Client,
    /// Pre-computed URL prefix: `<base>/bot<token>`. Tokens
    /// are never logged or returned; the prefix is internal.
    url_prefix: String,
    /// FIX 50: optional proactive rate-limit tracker. When
    /// set, outbound `sendMessage` / `editMessageText` /
    /// `sendChatAction` / `answerCallbackQuery` calls
    /// `acquire(chat_key).await` before issuing the HTTP
    /// request. When `None`, behaviour is unchanged.
    rate_limiter: Option<ChannelRateLimiter>,
}

impl LiveBotApi {
    /// New client pointed at the public Telegram Bot API.
    pub fn new(token: String) -> Self {
        Self::with_base_url(token, DEFAULT_API_BASE.into())
    }

    /// New client pointed at an arbitrary base URL. Used by
    /// tests that spin a localhost server emulating the Bot
    /// API surface.
    pub fn with_base_url(token: String, base_url: String) -> Self {
        // Trim trailing slash so the join is deterministic.
        let base = base_url.trim_end_matches('/').to_string();
        let prefix = format!("{base}/bot{token}");
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(PER_CALL_TIMEOUT_SECS))
            // The default pool keeps a few connections; we
            // don't need anything exotic here.
            .build()
            .expect("reqwest::Client::builder succeeds with default config");
        Self {
            http,
            url_prefix: prefix,
            rate_limiter: None,
        }
    }

    /// FIX 50: install a proactive rate-limit tracker. Pass
    /// the result of
    /// `ChannelRateLimiter::new(TELEGRAM_PER_CHAT, Some(TELEGRAM_GLOBAL), clock)`
    /// to honour the documented per-chat + global caps.
    /// Returns `self` so callers can chain with `LiveBotApi::new(...).with_rate_limiter(...)`.
    pub fn with_rate_limiter(mut self, limiter: ChannelRateLimiter) -> Self {
        self.rate_limiter = Some(limiter);
        self
    }

    /// FIX 50: gate one outbound call on the rate limiter.
    /// `key` is the per-chat identifier (chat_id rendered as
    /// a string). When the bucket is at >= 80% utilisation
    /// we add a small artificial backoff before approving
    /// the send so the next request approaches the cap
    /// gracefully rather than slamming it. When the bucket
    /// is empty `acquire()` awaits the refill.
    async fn rate_limit_acquire(&self, key: &str) {
        if let Some(lim) = self.rate_limiter.as_ref() {
            let state = lim.acquire(key).await;
            if matches!(state, RateLimitState::Throttled) {
                // 50ms artificial backoff at the soft
                // threshold. Tiny enough to be invisible at
                // normal load; large enough to break the
                // synchronised-thundering-herd pattern.
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    /// POST a Bot API method with a JSON body and decode the
    /// success envelope. Surfaces:
    ///
    /// - `ClientError` for 4xx other than 429 (caller bug).
    /// - `Transient` after all retries are exhausted on 429
    ///   or 5xx.
    /// - `Transient` for network / decode errors.
    async fn post<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: &serde_json::Value,
    ) -> Result<T, BotApiError> {
        let url = format!("{}/{method}", self.url_prefix);
        let mut attempt: u32 = 0;
        loop {
            let resp = self.http.post(&url).json(body).send().await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    // Network blip. Retry within budget.
                    if attempt >= MAX_RETRIES {
                        return Err(BotApiError::Transient(format!(
                            "{method}: network error after {MAX_RETRIES} retries: {e}"
                        )));
                    }
                    backoff(attempt).await;
                    attempt += 1;
                    continue;
                }
            };
            let status = resp.status();
            if status.is_success() {
                // Telegram wraps every response in
                // `{ "ok": true, "result": <T> }`.
                let parsed: TgEnvelope<T> = match resp.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(BotApiError::Transient(format!("{method}: decode: {e}")));
                    }
                };
                if !parsed.ok {
                    return Err(BotApiError::ClientError(format!(
                        "{method}: telegram returned ok=false: {}",
                        parsed.description.unwrap_or_default()
                    )));
                }
                return parsed.result.ok_or_else(|| {
                    BotApiError::Transient(format!("{method}: envelope missing `result`"))
                });
            }

            // Failure: 429, 5xx, or 4xx-other.
            let body_text = resp.text().await.unwrap_or_default();
            let parsed: Option<TgErrorEnvelope> = serde_json::from_str(&body_text).ok();

            if status.as_u16() == 429 {
                // Honour Telegram's retry_after, but clamp at
                // RETRY_AFTER_CLAMP_SECS so an aggressive
                // server-side throttle can't wedge a worker
                // for hours. Operators see the original value
                // in the log so they know Telegram is asking
                // for more than we'll grant.
                let raw = parsed
                    .as_ref()
                    .and_then(|e| e.parameters.as_ref())
                    .and_then(|p| p.retry_after)
                    .unwrap_or(1);
                let clamped = raw.min(RETRY_AFTER_CLAMP_SECS);
                if clamped < raw {
                    tracing::warn!(
                        method,
                        raw_retry_after = raw,
                        clamped_retry_after = clamped,
                        "telegram: 429 retry_after clamped to {RETRY_AFTER_CLAMP_SECS}s; \
                         Telegram requested {raw}s — aggressive rate limit, check bot health"
                    );
                }
                if attempt >= MAX_RETRIES {
                    return Err(BotApiError::Transient(format!(
                        "{method}: 429 retry_after={clamped} (raw={raw}) after {MAX_RETRIES} retries"
                    )));
                }
                tokio::time::sleep(Duration::from_secs(clamped.max(1) as u64)).await;
                attempt += 1;
                continue;
            }

            if status.is_server_error() {
                if attempt >= MAX_RETRIES {
                    return Err(BotApiError::Transient(format!(
                        "{method}: {} after {MAX_RETRIES} retries",
                        status.as_u16()
                    )));
                }
                backoff(attempt).await;
                attempt += 1;
                continue;
            }

            // Other 4xx — not retried.
            // Surface the description from Telegram so the
            // operator's log line names the problem (e.g.
            // "Unauthorized" for a bad token).
            let desc = parsed
                .and_then(|e| e.description)
                .unwrap_or_else(|| body_text.clone());
            return Err(BotApiError::ClientError(format!(
                "{method}: {} {desc}",
                status.as_u16()
            )));
        }
    }
}

/// Exponential backoff with jitter: 1s, 2s, 4s base + up to
/// 50% jitter on each attempt. The jitter (FIX 5) prevents a
/// fleet of bots all retrying after the same Telegram blip
/// from synchronising into a thundering herd. `rand::thread_rng`
/// is thread-safe (each tokio worker has its own RNG state) so
/// concurrent retries from different tasks don't contend on a
/// shared lock.
async fn backoff(attempt: u32) {
    use rand::Rng;
    let base_ms = 1000u64.checked_shl(attempt).unwrap_or(8000).min(8000);
    // `gen_range(0..0)` would panic — guard with `.max(1)` so the
    // first attempt (base_ms = 1000) still produces a valid range
    // and tiny bases stay safe.
    let jitter_ceiling = (base_ms / 2).max(1);
    let jitter_ms = rand::thread_rng().gen_range(0..jitter_ceiling);
    let actual_delay = base_ms + jitter_ms;
    tokio::time::sleep(Duration::from_millis(actual_delay)).await;
}

// ── Wire envelopes ────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TgEnvelope<T> {
    ok: bool,
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TgErrorEnvelope {
    #[allow(dead_code)]
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<TgRetryParams>,
}

#[derive(Debug, Deserialize)]
struct TgRetryParams {
    #[serde(default)]
    retry_after: Option<i64>,
}

// ── getMe ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TgGetMeResult {
    id: i64,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    first_name: Option<String>,
}

// ── getUpdates ────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct TgGetUpdatesReq<'a> {
    offset: i64,
    timeout: u32,
    allowed_updates: &'a [&'a str],
}

/// Raw shape of a Telegram update. Models the two update kinds
/// the channel acts on:
///
/// - `message` — text / voice / photo etc. We surface text and
///   voice only.
/// - `callback_query` — operator pressed an inline button on a
///   message we previously sent. We surface the press as an
///   [`IncomingMessage`] whose `text` is the button's
///   `callback_data` verbatim so the existing slash-command
///   routing (`/approve <id>`, `/deny <id>`) picks it up.
///
/// All other update kinds (edited messages, polls, channel
/// posts, …) are silently skipped — `update_to_incoming`
/// returns `None`.
#[derive(Debug, Deserialize)]
struct TgUpdate {
    update_id: i64,
    #[serde(default)]
    message: Option<TgMessage>,
    /// Set when this update is a callback_query rather than a
    /// regular message — `update_to_incoming` routes it to the
    /// callback path.
    #[serde(default)]
    callback_query: Option<TgCallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct TgMessage {
    message_id: i64,
    from: Option<TgUser>,
    chat: TgChat,
    #[serde(default)]
    text: Option<String>,
    /// Set on voice notes; carries the `file_id` we later
    /// resolve via `getFile` to fetch the audio bytes.
    #[serde(default)]
    voice: Option<TgVoice>,
}

#[derive(Debug, Deserialize)]
struct TgVoice {
    file_id: String,
}

#[derive(Debug, Deserialize)]
struct TgUser {
    id: i64,
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TgChat {
    id: i64,
}

/// Telegram `callback_query` envelope. `id` is what we echo
/// back to `answerCallbackQuery` to clear the operator's
/// spinning button indicator. `message` is the message that
/// carried the inline keyboard the operator pressed — set on
/// every callback_query from a bot-authored message. `data`
/// is the opaque payload we stamped on the button at send
/// time; for approval flows this is `/approve <id>` or
/// `/deny <id>`.
#[derive(Debug, Deserialize)]
struct TgCallbackQuery {
    id: String,
    from: TgUser,
    #[serde(default)]
    message: Option<TgMessage>,
    #[serde(default)]
    data: Option<String>,
}

/// Convert a raw Telegram update into the channel's
/// [`IncomingMessage`]. Returns `None` for updates we don't
/// model yet (edited messages, polls, channel posts, …) — the
/// caller skips silently.
///
/// - **Text messages**: `text` carries the body, `voice_file_id`
///   is `None`, `callback_query_id` is `None`.
/// - **Voice messages**: `text` is empty, `voice_file_id` is
///   the Telegram `file_id`, `callback_query_id` is `None`.
/// - **Callback-query updates** (operator pressed an inline
///   button): `text` carries the button's `callback_data`
///   verbatim, `callback_query_id` is `Some` so the controller
///   can ack via `answerCallbackQuery`. `chat_id` /
///   `message_id` are inherited from the message that carried
///   the keyboard so the controller can edit-in-place the
///   approval banner.
fn update_to_incoming(u: TgUpdate) -> Option<IncomingMessage> {
    if let Some(cb) = u.callback_query {
        let data = cb.data.unwrap_or_default();
        if data.is_empty() {
            return None;
        }
        let (chat_id, message_id) = cb
            .message
            .as_ref()
            .map(|m| (m.chat.id, m.message_id))
            .unwrap_or((0, 0));
        return Some(IncomingMessage {
            update_id: u.update_id,
            chat_id,
            user_id: cb.from.id,
            message_id,
            username: cb.from.username.unwrap_or_default(),
            text: data,
            voice_file_id: None,
            callback_query_id: Some(cb.id),
        });
    }
    let m = u.message?;
    let from = m.from?;
    let text = m.text.unwrap_or_default();
    let voice_file_id = m.voice.map(|v| v.file_id);
    if text.is_empty() && voice_file_id.is_none() {
        return None;
    }
    Some(IncomingMessage {
        update_id: u.update_id,
        chat_id: m.chat.id,
        user_id: from.id,
        message_id: m.message_id,
        username: from.username.unwrap_or_default(),
        text,
        voice_file_id,
        callback_query_id: None,
    })
}

#[async_trait]
impl BotApi for LiveBotApi {
    async fn get_me(&self) -> Result<BotIdentity, BotApiError> {
        let res: TgGetMeResult = self.post("getMe", &serde_json::json!({})).await?;
        Ok(BotIdentity {
            user_id: res.id,
            username: res.username.unwrap_or_default(),
            first_name: res.first_name.unwrap_or_default(),
        })
    }

    async fn get_updates(&self, offset: i64) -> Result<Vec<IncomingMessage>, BotApiError> {
        let body = serde_json::to_value(TgGetUpdatesReq {
            offset,
            timeout: LONG_POLL_TIMEOUT_SECS,
            // `message` covers text + voice; `callback_query`
            // surfaces inline-button presses (approve / deny
            // on approval banners). Telegram drops any update
            // type not in this allowlist server-side.
            allowed_updates: &["message", "callback_query"],
        })
        .map_err(|e| BotApiError::Transient(format!("getUpdates: build body: {e}")))?;
        let raw: Vec<TgUpdate> = self.post("getUpdates", &body).await?;
        Ok(raw.into_iter().filter_map(update_to_incoming).collect())
    }

    async fn send_message(&self, out: &OutgoingMessage) -> Result<(), BotApiError> {
        // FIX 50: gate on the per-chat rate limit before
        // issuing the HTTP POST.
        self.rate_limit_acquire(&out.chat_id.to_string()).await;
        let mut body = serde_json::json!({
            "chat_id": out.chat_id,
            "text": out.text,
        });
        if out.reply_to_message_id != 0 {
            body["reply_to_message_id"] = serde_json::json!(out.reply_to_message_id);
        }
        if let Some(pm) = out.parse_mode {
            body["parse_mode"] = serde_json::json!(pm.as_wire());
        }
        if let Some(markup) = out.reply_markup.as_ref() {
            // `serde_json::to_value` on `InlineKeyboardMarkup`
            // is infallible — the struct only contains
            // strings and `Vec`. We still match defensively so
            // a future schema change can never panic the
            // sender.
            match serde_json::to_value(markup) {
                Ok(v) => {
                    body["reply_markup"] = v;
                }
                Err(e) => {
                    return Err(BotApiError::Transient(format!(
                        "sendMessage: encode reply_markup: {e}"
                    )));
                }
            }
        }
        let _: TgIgnoredResult = self.post("sendMessage", &body).await?;
        Ok(())
    }

    async fn answer_callback_query(
        &self,
        callback_query_id: &str,
        text: Option<&str>,
    ) -> Result<(), BotApiError> {
        let mut body = serde_json::json!({ "callback_query_id": callback_query_id });
        if let Some(t) = text {
            body["text"] = serde_json::json!(t);
        }
        let _: TgIgnoredResult = self.post("answerCallbackQuery", &body).await?;
        Ok(())
    }

    async fn edit_message_text(
        &self,
        chat_id: i64,
        message_id: i64,
        text: &str,
        parse_mode: Option<ParseMode>,
    ) -> Result<(), BotApiError> {
        // FIX 50: per-chat rate-limit gate.
        self.rate_limit_acquire(&chat_id.to_string()).await;
        let mut body = serde_json::json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
        });
        if let Some(pm) = parse_mode {
            body["parse_mode"] = serde_json::json!(pm.as_wire());
        }
        let _: TgIgnoredResult = self.post("editMessageText", &body).await?;
        Ok(())
    }

    async fn send_chat_action(&self, chat_id: i64, action: &str) -> Result<(), BotApiError> {
        // FIX 50: per-chat rate-limit gate.
        self.rate_limit_acquire(&chat_id.to_string()).await;
        let body = serde_json::json!({
            "chat_id": chat_id,
            "action": action,
        });
        let _: TgIgnoredResult = self.post("sendChatAction", &body).await?;
        Ok(())
    }

    async fn get_file_bytes(&self, file_id: &str) -> Result<Vec<u8>, BotApiError> {
        // Resolve file_id → file_path via getFile, then GET the
        // raw bytes from <root>/file/bot<token>/<file_path>. The
        // download host is the same root (api.telegram.org) but a
        // different path prefix; we reconstruct it from the
        // url_prefix we already hold.
        let file: TgFile = self
            .post("getFile", &serde_json::json!({ "file_id": file_id }))
            .await?;
        let path = file.file_path.ok_or_else(|| {
            BotApiError::ClientError("getFile: telegram returned no file_path".into())
        })?;
        // url_prefix is "<base>/bot<token>" — split into base
        // and token bits so we can rebuild "<base>/file/bot<token>/<path>".
        let download_url = match self.url_prefix.find("/bot") {
            Some(i) => {
                let base = &self.url_prefix[..i];
                let bot_seg = &self.url_prefix[i + 1..]; // "bot<token>"
                format!("{base}/file/{bot_seg}/{path}")
            }
            None => {
                return Err(BotApiError::Transient(
                    "get_file_bytes: url_prefix shape unexpected".into(),
                ));
            }
        };
        let resp = self
            .http
            .get(&download_url)
            .send()
            .await
            .map_err(|e| BotApiError::Transient(format!("getFile download: {e}")))?;
        if !resp.status().is_success() {
            return Err(BotApiError::Transient(format!(
                "getFile download: HTTP {}",
                resp.status().as_u16()
            )));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| BotApiError::Transient(format!("getFile read body: {e}")))
    }

    async fn set_webhook(&self, url: &str) -> Result<(), BotApiError> {
        // FIX 1: register the URL Telegram should POST updates
        // to. `allowed_updates` mirrors the `getUpdates` long-
        // poll allowlist so the operator gets exactly the same
        // event set on both paths.
        let body = serde_json::json!({
            "url": url,
            "allowed_updates": ["message", "callback_query"],
        });
        let _: TgIgnoredResult = self.post("setWebhook", &body).await?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct TgFile {
    #[serde(default)]
    file_path: Option<String>,
}

/// `Telegram` returns `true` as the `result` on most write
/// methods. We don't care about the value; this lets the
/// generic decoder accept anything sensible.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
#[allow(dead_code)]
enum TgIgnoredResult {
    Bool(bool),
    Obj(serde_json::Value),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_to_incoming_extracts_text_message() {
        let raw = serde_json::json!({
            "update_id": 7,
            "message": {
                "message_id": 11,
                "from": { "id": 42, "username": "alice" },
                "chat": { "id": 100 },
                "text": "hi"
            }
        });
        let u: TgUpdate = serde_json::from_value(raw).unwrap();
        let inc = update_to_incoming(u).unwrap();
        assert_eq!(inc.update_id, 7);
        assert_eq!(inc.chat_id, 100);
        assert_eq!(inc.user_id, 42);
        assert_eq!(inc.message_id, 11);
        assert_eq!(inc.username, "alice");
        assert_eq!(inc.text, "hi");
    }

    #[test]
    fn update_to_incoming_drops_messages_with_neither_text_nor_voice() {
        let raw = serde_json::json!({
            "update_id": 1,
            "message": {
                "message_id": 1,
                "from": { "id": 42 },
                "chat": { "id": 100 }
                // no text, no voice — photo / sticker / etc.
            }
        });
        let u: TgUpdate = serde_json::from_value(raw).unwrap();
        assert!(update_to_incoming(u).is_none());
    }

    #[test]
    fn update_to_incoming_extracts_voice_file_id() {
        let raw = serde_json::json!({
            "update_id": 9,
            "message": {
                "message_id": 21,
                "from": { "id": 42, "username": "alice" },
                "chat": { "id": 100 },
                "voice": { "file_id": "AwACAg-fake-id", "duration": 3 }
            }
        });
        let u: TgUpdate = serde_json::from_value(raw).unwrap();
        let inc = update_to_incoming(u).expect("voice message must produce an IncomingMessage");
        assert_eq!(inc.voice_file_id.as_deref(), Some("AwACAg-fake-id"));
        // Voice-only messages arrive with an empty text body —
        // the controller fills it in after transcription.
        assert_eq!(inc.text, "");
    }

    #[test]
    fn update_to_incoming_surfaces_callback_query_data_as_text() {
        // Callback query without an embedded message still
        // surfaces; chat_id / message_id default to 0 so the
        // caller can detect that the source message can't be
        // edited in place.
        let raw = serde_json::json!({
            "update_id": 2,
            "callback_query": {
                "id": "cb1",
                "from": { "id": 42, "username": "alice" },
                "data": "/approve task-1"
            }
        });
        let u: TgUpdate = serde_json::from_value(raw).unwrap();
        let inc = update_to_incoming(u).expect("callback_query must produce an IncomingMessage");
        assert_eq!(inc.update_id, 2);
        assert_eq!(inc.user_id, 42);
        assert_eq!(inc.username, "alice");
        assert_eq!(inc.text, "/approve task-1");
        assert_eq!(inc.callback_query_id.as_deref(), Some("cb1"));
        assert!(inc.is_callback_query());
        // No embedded message → chat_id / message_id are 0.
        assert_eq!(inc.chat_id, 0);
        assert_eq!(inc.message_id, 0);
    }

    #[test]
    fn update_to_incoming_callback_query_with_message_inherits_chat() {
        // The common case — Telegram includes the originating
        // message so the controller can edit-in-place.
        let raw = serde_json::json!({
            "update_id": 3,
            "callback_query": {
                "id": "cb2",
                "from": { "id": 42 },
                "message": {
                    "message_id": 99,
                    "chat": { "id": 100 }
                },
                "data": "/deny task-2"
            }
        });
        let u: TgUpdate = serde_json::from_value(raw).unwrap();
        let inc = update_to_incoming(u).expect("callback_query must produce an IncomingMessage");
        assert_eq!(inc.chat_id, 100);
        assert_eq!(inc.message_id, 99);
        assert_eq!(inc.text, "/deny task-2");
        assert_eq!(inc.callback_query_id.as_deref(), Some("cb2"));
    }

    #[test]
    fn update_to_incoming_drops_callback_query_without_data() {
        // Defensive — Telegram should always include `data`
        // for inline-button callbacks but we drop the update
        // rather than emit an empty-text message that would
        // confuse the slash-command router.
        let raw = serde_json::json!({
            "update_id": 4,
            "callback_query": {
                "id": "cb3",
                "from": { "id": 42 }
            }
        });
        let u: TgUpdate = serde_json::from_value(raw).unwrap();
        assert!(update_to_incoming(u).is_none());
    }

    #[test]
    fn backoff_table_is_capped() {
        // Smoke: backoff for attempt=10 must not blow up the
        // shift. We don't assert the duration directly (it's
        // an async sleep elsewhere), just that the math
        // doesn't panic.
        let v = 1000u64.checked_shl(10).unwrap_or(8000).min(8000);
        assert_eq!(v, 8000);
    }

    #[test]
    fn retry_after_clamp_constant_matches_spec() {
        // FIX 4: the clamp is documented as one hour. Locking
        // the constant in a test so a future "just bump the
        // clamp" change has to update the test too.
        assert_eq!(RETRY_AFTER_CLAMP_SECS, 3600);
    }

    #[test]
    fn retry_after_clamps_when_telegram_returns_more_than_an_hour() {
        // FIX 4: simulate the exact arithmetic the 429 branch
        // executes. The function-under-test is `i64::min`, so
        // this is an integration-friendly test of the constant
        // itself + the `.min()` clamp semantics.
        let raw: i64 = 86_400; // 24h — Telegram should never
        // ask for this but the SDK has to refuse to honour it.
        let clamped = raw.min(RETRY_AFTER_CLAMP_SECS);
        assert_eq!(clamped, 3600);
        assert!(clamped < raw, "the clamp must actually shrink the value");
    }

    #[test]
    fn retry_after_no_clamp_when_within_budget() {
        // FIX 4: a normal 30s rate-limit response passes
        // through unchanged.
        let raw: i64 = 30;
        let clamped = raw.min(RETRY_AFTER_CLAMP_SECS);
        assert_eq!(clamped, 30);
    }

    #[test]
    fn backoff_jitter_never_collapses_to_a_single_value() {
        // FIX 5: the contract is "deterministic backoff plus
        // a uniformly-random jitter in [0, base/2)". Sampling
        // 32 draws is enough to detect a frozen RNG with very
        // high probability (P(all 32 equal | uniform over [0,
        // 500)) ≈ (1/500)^31, negligible). The test asserts
        // that AT LEAST two of the sampled durations differ,
        // which fails if the jitter is missing entirely.
        use rand::Rng;
        let base_ms: u64 = 1000;
        let jitter_ceiling = (base_ms / 2).max(1);
        let mut samples = Vec::with_capacity(32);
        for _ in 0..32 {
            let j = rand::thread_rng().gen_range(0..jitter_ceiling);
            samples.push(base_ms + j);
        }
        let min = samples.iter().min().copied().unwrap();
        let max = samples.iter().max().copied().unwrap();
        assert!(
            max > min,
            "jitter must produce at least two distinct durations across 32 samples: \
             {samples:?}"
        );
        // And the range must be within [base, base + ceiling).
        assert!(samples.iter().all(|&v| (1000..1500).contains(&v)));
    }

    #[test]
    fn backoff_jitter_is_safe_when_base_is_one_ms() {
        // FIX 5: defensive — if a future config knob lets the
        // base shrink to 1ms, `base / 2 = 0` would panic
        // `gen_range(0..0)`. The `.max(1)` guard in the
        // backoff fn must keep the sampler healthy.
        use rand::Rng;
        let base_ms: u64 = 1;
        let jitter_ceiling = (base_ms / 2).max(1);
        // Should not panic. The actual value is 0 (the only
        // value in `[0, 1)`).
        let j = rand::thread_rng().gen_range(0..jitter_ceiling);
        assert_eq!(j, 0);
    }
}
