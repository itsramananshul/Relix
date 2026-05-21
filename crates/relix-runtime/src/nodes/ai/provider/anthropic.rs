//! Native Anthropic Messages API provider.
//!
//! Distinct from the OpenAI-compatible path because Anthropic uses different
//! headers (`x-api-key` + `anthropic-version`) and a different response shape
//! (`content[].text`, not `choices[].message.content`).

use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use super::{
    ChatInput, ChatOutput, ChatProvider, ProviderEntry, ProviderError, TokenUsage, load_api_key,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    base_url: String,
    api_key: String,
    default_model: String,
    http: reqwest::Client,
}

impl AnthropicProvider {
    pub fn from_entry(entry: &ProviderEntry) -> Result<Self, ProviderError> {
        let api_key = load_api_key(entry)?.ok_or_else(|| {
            ProviderError::Permanent(
                "[ai.providers.anthropic] missing api_key_env — set api_key_env to the env var \
                 holding the key (e.g. \"ANTHROPIC_API_KEY\")"
                    .into(),
            )
        })?;
        let base_url = entry
            .base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();
        let default_model = entry
            .default_model
            .clone()
            .unwrap_or_else(|| "claude-3-5-sonnet-latest".to_string());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(entry.timeout_secs))
            .build()
            .map_err(|e| ProviderError::Permanent(format!("anthropic: http client: {e}")))?;
        Ok(Self {
            base_url,
            api_key,
            default_model,
            http,
        })
    }
}

#[async_trait]
impl ChatProvider for AnthropicProvider {
    async fn generate_reply(&self, input: ChatInput) -> Result<ChatOutput, ProviderError> {
        let model = if input.model.is_empty() {
            self.default_model.clone()
        } else {
            input.model.clone()
        };
        let user_content = if input.history.trim().is_empty() {
            input.prompt.clone()
        } else {
            format!(
                "Recent conversation (session={s}):\n{h}\n\nNew message: {p}",
                s = input.session_id,
                h = input.history,
                p = input.prompt,
            )
        };
        let mut body = json!({
            "model": model,
            "max_tokens": input.max_tokens.unwrap_or(1024),
            "messages": [{ "role": "user", "content": user_content }],
        });
        if let Some(sys) = &input.system_prompt {
            body["system"] = json!(sys);
        }
        if let Some(t) = input.temperature {
            body["temperature"] = json!(t);
        }

        let url = format!("{}/v1/messages", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| {
                let reason = crate::nodes::ai::classify_transport_failure(&e.to_string());
                tracing::warn!(
                    provider = "anthropic",
                    failover.reason = %reason.label(),
                    "ai.provider: transport failure"
                );
                ProviderError::Transient(format!(
                    "anthropic: http [{label}]: {e}",
                    label = reason.label(),
                ))
            })?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ProviderError::Transient(format!("anthropic: read body: {e}")))?;

        if !status.is_success() {
            // H1: structured classification.
            let reason = crate::nodes::ai::classify_http_failure(status.as_u16(), &text);
            let perm = matches!(
                reason.category(),
                crate::nodes::ai::FailoverCategory::Permanent
            );
            tracing::warn!(
                provider = "anthropic",
                http.status = status.as_u16(),
                failover.reason = %reason.label(),
                failover.category = ?reason.category(),
                "ai.provider: http failure"
            );
            let msg = format!(
                "anthropic: HTTP {status} [{label}]: {text}",
                label = reason.label(),
            );
            return Err(if perm {
                ProviderError::Permanent(msg)
            } else {
                ProviderError::Transient(msg)
            });
        }

        let parsed: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Permanent(format!("anthropic: parse: {e}")))?;
        let reply = parsed
            .get("content")
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.iter().find_map(|b| b.get("text")?.as_str()))
            .map(|s| s.to_string())
            .ok_or_else(|| {
                ProviderError::Permanent(format!("anthropic: no text content in: {text}"))
            })?;
        let usage = parsed.get("usage").map(|u| TokenUsage {
            prompt_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            completion_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            total_tokens: u
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                .saturating_add(u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0))
                as u32,
        });
        Ok(ChatOutput {
            text: reply,
            provider: "anthropic",
            model,
            usage,
        })
    }

    fn provider_name(&self) -> &'static str {
        "anthropic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_api_key_env_errors() {
        // Use a deliberately-not-set env-var name.
        let entry = ProviderEntry {
            base_url: None,
            api_key_env: Some("RELIX_TEST_ABSOLUTELY_MISSING_ANTH_42".into()),
            default_model: None,
            timeout_secs: 30,
        };
        // AnthropicProvider does not impl Debug (holds an HTTP client).
        match AnthropicProvider::from_entry(&entry) {
            Ok(_) => panic!("expected error"),
            Err(ProviderError::Permanent(m)) => assert!(m.contains("missing provider key")),
            Err(other) => panic!("expected permanent, got {other}"),
        }
    }

    #[test]
    fn no_api_key_env_at_all_errors_with_hint() {
        let entry = ProviderEntry {
            base_url: None,
            api_key_env: None,
            default_model: None,
            timeout_secs: 30,
        };
        match AnthropicProvider::from_entry(&entry) {
            Ok(_) => panic!("expected error"),
            Err(ProviderError::Permanent(m)) => assert!(m.contains("missing api_key_env")),
            Err(other) => panic!("expected permanent, got {other}"),
        }
    }
}
