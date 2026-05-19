//! OpenAI-compatible provider — works against any backend that speaks
//! `POST {base_url}/chat/completions` with the OpenAI message-list shape.
//!
//! Concrete deployments:
//!
//! | provider name | typical base_url                    | api_key_env example |
//! |---------------|-------------------------------------|---------------------|
//! | `openai`      | `https://api.openai.com/v1`         | `OPENAI_API_KEY`    |
//! | `openrouter`  | `https://openrouter.ai/api/v1`      | `OPENROUTER_API_KEY`|
//! | `xai`         | `https://api.x.ai/v1`               | `XAI_API_KEY`       |
//! | `local`       | `http://localhost:11434/v1` (Ollama) | (unset / empty)    |
//!
//! Bearer auth header is added iff a key was loaded.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use super::{
    ChatInput, ChatOutput, ChatProvider, ProviderEntry, ProviderError, TokenUsage, load_api_key,
};

/// One instance per active OpenAI-compatible provider name.
pub struct OpenAICompatibleProvider {
    name: &'static str,
    base_url: String,
    api_key: Option<String>,
    default_model: String,
    http: reqwest::Client,
}

impl OpenAICompatibleProvider {
    /// Build from a `[ai.providers.<name>]` entry. `name` is the static
    /// label the trait reports back to the handler / audit.
    pub fn from_entry(name: &'static str, entry: &ProviderEntry) -> Result<Self, ProviderError> {
        let base_url = entry
            .base_url
            .as_ref()
            .ok_or_else(|| {
                ProviderError::Permanent(format!("[ai.providers.{name}] missing base_url"))
            })?
            .trim_end_matches('/')
            .to_string();
        let api_key = load_api_key(entry)?;
        let default_model = entry
            .default_model
            .clone()
            .unwrap_or_else(|| "gpt-4o-mini".to_string());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(entry.timeout_secs))
            .build()
            .map_err(|e| ProviderError::Permanent(format!("http client: {e}")))?;
        Ok(Self {
            name,
            base_url,
            api_key,
            default_model,
            http,
        })
    }
}

#[async_trait]
impl ChatProvider for OpenAICompatibleProvider {
    async fn generate_reply(&self, input: ChatInput) -> Result<ChatOutput, ProviderError> {
        let model = if input.model.is_empty() {
            self.default_model.clone()
        } else {
            input.model.clone()
        };

        // Build the OpenAI-style messages array. Keep it simple in the
        // alpha: optional system + a single user turn that wraps the
        // history block in front of the new prompt. Typed turns land at
        // Gate 2 with the CDDL stdlib.
        let mut messages = Vec::with_capacity(2);
        if let Some(sys) = &input.system_prompt {
            messages.push(json!({ "role": "system", "content": sys }));
        }
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
        messages.push(json!({ "role": "user", "content": user_content }));

        let mut body = json!({
            "model": model,
            "messages": messages,
        });
        if let Some(t) = input.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = input.max_tokens {
            body["max_tokens"] = json!(m);
        }

        let url = format!("{}/chat/completions", self.base_url);
        let mut req = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(key) = &self.api_key {
            req = req.header("authorization", format!("Bearer {key}"));
        }

        let resp = req.send().await.map_err(|e| {
            ProviderError::Transient(format!("{provider}: http: {e}", provider = self.name))
        })?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| {
            ProviderError::Transient(format!("{provider}: read body: {e}", provider = self.name))
        })?;

        if !status.is_success() {
            let perm = !(status.as_u16() == 429 || status.is_server_error());
            let msg = format!("{}: HTTP {status}: {text}", self.name);
            return Err(if perm {
                ProviderError::Permanent(msg)
            } else {
                ProviderError::Transient(msg)
            });
        }

        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            ProviderError::Permanent(format!("{provider}: parse: {e}", provider = self.name))
        })?;
        let reply_text = parsed
            .pointer("/choices/0/message/content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                ProviderError::Permanent(format!(
                    "{provider}: no choices[0].message.content in: {text}",
                    provider = self.name
                ))
            })?;
        let usage = parsed.get("usage").map(|u| TokenUsage {
            prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            completion_tokens: u
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
        });
        Ok(ChatOutput {
            text: reply_text,
            provider: self.name,
            model,
            usage,
        })
    }

    fn provider_name(&self) -> &'static str {
        self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_base_url_errors_clearly() {
        let entry = ProviderEntry {
            base_url: None,
            api_key_env: None,
            default_model: None,
            timeout_secs: 30,
        };
        // `OpenAICompatibleProvider` does not impl Debug (it holds an HTTP
        // client), so we cannot {:?} the Ok branch — explicit match instead.
        match OpenAICompatibleProvider::from_entry("openai", &entry) {
            Ok(_) => panic!("expected permanent error, got Ok"),
            Err(ProviderError::Permanent(m)) => assert!(m.contains("missing base_url")),
            Err(other) => panic!("expected permanent, got {other}"),
        }
    }

    #[test]
    fn provider_name_passthrough() {
        let entry = ProviderEntry {
            base_url: Some("http://localhost:11434/v1".into()),
            api_key_env: None,
            default_model: Some("llama3:8b".into()),
            timeout_secs: 30,
        };
        let p = match OpenAICompatibleProvider::from_entry("local", &entry) {
            Ok(p) => p,
            Err(e) => panic!("local should build: {e}"),
        };
        assert_eq!(p.provider_name(), "local");
        assert_eq!(p.default_model, "llama3:8b");
        assert!(p.api_key.is_none());
    }
}
