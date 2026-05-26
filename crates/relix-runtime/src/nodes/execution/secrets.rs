//! Just-In-Time secret injection.
//!
//! `SecretStore` reads `RELIX_*` environment variables once
//! at controller startup and resolves `{{secret:<name>}}`
//! placeholders in tool arguments at dispatch time. Secrets
//! never appear on disk, never get committed, and never enter
//! the agent's prompt context: the placeholder is rewritten
//! to the literal value at the moment the tool call goes out.
//!
//! Naming convention: the env var `RELIX_GITHUB_TOKEN`
//! becomes the key `github_token`. The `RELIX_` prefix is
//! stripped and the rest is lowercased so the placeholder
//! grammar matches operator convention without surprise.
//!
//! ## Honest scope
//!
//! - Secrets are loaded **once** at startup. Re-reading on
//!   every dispatch is intentionally NOT supported: that
//!   would let a compromised env-injection attack take effect
//!   mid-process. Restart the controller to rotate.
//! - The store keeps the secret values in memory; there is
//!   no encryption-at-rest layer. The defensive surface here
//!   is "operator runs the binary with secrets only in env",
//!   not "secret values are hidden from a co-located
//!   process."

use std::collections::BTreeMap;

/// Errors raised by [`SecretStore::resolve`]. Carries the
/// missing secret name so callers can surface an operator-
/// facing message naming the env var to set.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    #[error("secret '{0}' not found; set RELIX_{1} env var")]
    Missing(String, String),
}

impl SecretError {
    /// Build a `Missing` error from a secret key. The second
    /// field is the env-var hint (uppercase form).
    pub fn missing(key: &str) -> Self {
        Self::Missing(key.to_string(), key.to_ascii_uppercase())
    }
}

/// In-process secret store.
#[derive(Clone, Debug)]
pub struct SecretStore {
    secrets: BTreeMap<String, String>,
}

impl SecretStore {
    /// Read every `RELIX_<NAME>` env var into the store. The
    /// resulting key is the lowercase of `<NAME>`.
    pub fn from_env() -> Self {
        Self::load_filtered(std::env::vars())
    }

    /// Build from an arbitrary key/value iterator with the
    /// same `RELIX_<NAME>` filter `from_env` uses. Exposed so
    /// tests can exercise the env-parsing logic without
    /// mutating the process env (which conflicts with the
    /// crate's `#![forbid(unsafe_code)]` posture in 2024).
    pub fn load_filtered<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (String, String)>,
    {
        let mut secrets = BTreeMap::new();
        for (k, v) in iter {
            if let Some(rest) = k.strip_prefix("RELIX_") {
                if rest.is_empty() {
                    continue;
                }
                secrets.insert(rest.to_ascii_lowercase(), v);
            }
        }
        Self { secrets }
    }

    /// Build from an explicit map. Used by tests so we don't
    /// pollute the process env.
    pub fn from_map(secrets: BTreeMap<String, String>) -> Self {
        Self { secrets }
    }

    /// Resolve `{{secret:<name>}}` placeholders in `template`
    /// against the loaded secrets. Returns `Err(Missing)` on
    /// the first unresolved placeholder so callers don't
    /// accidentally dispatch a tool call with a partial
    /// substitution.
    pub fn resolve(&self, template: &str) -> Result<String, SecretError> {
        let mut out = String::with_capacity(template.len());
        let mut rest = template;
        loop {
            let Some(start) = rest.find("{{secret:") else {
                out.push_str(rest);
                break;
            };
            out.push_str(&rest[..start]);
            let after_marker = &rest[start + "{{secret:".len()..];
            let Some(end) = after_marker.find("}}") else {
                // Unterminated placeholder — preserve it
                // verbatim so the operator sees the typo in
                // their tool args.
                out.push_str(&rest[start..]);
                break;
            };
            let key = after_marker[..end].trim();
            let value = self
                .secrets
                .get(key)
                .ok_or_else(|| SecretError::missing(key))?;
            out.push_str(value);
            rest = &after_marker[end + "}}".len()..];
        }
        Ok(out)
    }

    /// List the names of every loaded secret WITHOUT the
    /// values. The bridge's `GET /v1/secrets/available`
    /// endpoint returns this so operators can verify their
    /// env was picked up without exposing the secrets
    /// themselves.
    pub fn available_keys(&self) -> Vec<String> {
        self.secrets.keys().cloned().collect()
    }

    /// Count of loaded secrets. Useful for the dashboard
    /// summary.
    pub fn len(&self) -> usize {
        self.secrets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(pairs: &[(&str, &str)]) -> SecretStore {
        let mut m = BTreeMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), (*v).to_string());
        }
        SecretStore::from_map(m)
    }

    #[test]
    fn load_filtered_strips_relix_prefix_and_lowercases() {
        let iter = vec![
            ("RELIX_GITHUB_TOKEN".into(), "ghp_secret".into()),
            ("RELIX_OPENAI_API_KEY".into(), "sk-secret".into()),
            // Non-RELIX vars must be ignored.
            ("PATH".into(), "/usr/bin".into()),
            // Bare `RELIX_` (no name) must also be skipped.
            ("RELIX_".into(), "ignored".into()),
        ];
        let store = SecretStore::load_filtered(iter);
        assert_eq!(
            store.secrets.get("github_token").map(|s| s.as_str()),
            Some("ghp_secret")
        );
        assert_eq!(
            store.secrets.get("openai_api_key").map(|s| s.as_str()),
            Some("sk-secret")
        );
        // Non-RELIX vars dropped.
        assert!(!store.secrets.contains_key("path"));
        // Bare `RELIX_` dropped.
        assert!(!store.secrets.contains_key(""));
    }

    #[test]
    fn resolve_substitutes_placeholder() {
        let store = store_with(&[("github_token", "ghp_secret")]);
        let resolved = store
            .resolve("Authorization: Bearer {{secret:github_token}}")
            .unwrap();
        assert_eq!(resolved, "Authorization: Bearer ghp_secret");
    }

    #[test]
    fn resolve_returns_missing_error() {
        let store = store_with(&[]);
        let err = store
            .resolve("token = {{secret:github_token}}")
            .unwrap_err();
        match err {
            SecretError::Missing(name, env_hint) => {
                assert_eq!(name, "github_token");
                assert_eq!(env_hint, "GITHUB_TOKEN");
            }
        }
    }

    #[test]
    fn resolve_handles_multiple_placeholders_in_one_string() {
        let store = store_with(&[("token_a", "AAA"), ("token_b", "BBB")]);
        let resolved = store
            .resolve("a={{secret:token_a}} b={{secret:token_b}} done")
            .unwrap();
        assert_eq!(resolved, "a=AAA b=BBB done");
    }

    #[test]
    fn resolve_passes_string_without_placeholders_unchanged() {
        let store = store_with(&[("github_token", "ghp")]);
        let s = "plain text with no markers";
        assert_eq!(store.resolve(s).unwrap(), s);
    }

    #[test]
    fn resolve_preserves_unterminated_placeholder_for_operator_diagnosis() {
        let store = store_with(&[("x", "y")]);
        // Missing closing `}}` — we don't substitute, but we
        // also don't error: the operator sees their typo in
        // the dispatched tool args.
        let s = "broken {{secret:foo";
        assert_eq!(store.resolve(s).unwrap(), s);
    }

    #[test]
    fn available_keys_returns_names_not_values() {
        let store = store_with(&[
            ("github_token", "ghp_secret"),
            ("openai_api_key", "sk-secret"),
        ]);
        let keys = store.available_keys();
        assert!(keys.contains(&"github_token".to_string()));
        assert!(keys.contains(&"openai_api_key".to_string()));
        // Values do NOT leak through this surface.
        for k in &keys {
            assert!(!k.contains("ghp_"));
            assert!(!k.contains("sk-"));
        }
        assert_eq!(store.len(), 2);
        assert!(!store.is_empty());
    }

    #[test]
    fn resolve_trims_whitespace_in_placeholder_name() {
        let store = store_with(&[("github_token", "ghp")]);
        let resolved = store.resolve("token={{secret: github_token }}").unwrap();
        assert_eq!(resolved, "token=ghp");
    }

    #[test]
    fn empty_store_is_empty() {
        let store = SecretStore::from_map(BTreeMap::new());
        assert!(store.is_empty());
        assert_eq!(store.len(), 0);
        assert!(store.available_keys().is_empty());
    }
}
