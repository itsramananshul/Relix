//! `[email]` runtime config consumed when `node_type = "email"`.
//!
//! Mirrors the shape of the telegram / slack / discord node
//! configs but covers two protocols (outbound SMTP + inbound
//! IMAP) plus optional DKIM signing and OAuth2 auth.
//!
//! Wire shape (controller TOML):
//!
//! ```toml
//! [email]
//! enabled = true
//!
//! # SMTP outbound
//! smtp_host = "smtp.gmail.com"
//! smtp_port = 587
//! smtp_username = "relix@example.com"
//! smtp_password_env = "EMAIL_SMTP_PASSWORD"   # or smtp_oauth2_token_env
//! smtp_from = "Relix <relix@example.com>"
//! smtp_tls = "starttls"                       # "starttls" | "tls" | "none"
//!
//! # Optional DKIM
//! dkim_private_key_path = "/etc/relix/dkim.pem"
//! dkim_selector = "relix"
//! dkim_domain = "example.com"
//!
//! # IMAP inbound
//! imap_host = "imap.gmail.com"
//! imap_port = 993
//! imap_username = "relix@example.com"
//! imap_password_env = "EMAIL_IMAP_PASSWORD"
//! imap_folder = "INBOX"
//! imap_processed_folder = ""
//! imap_poll_interval_secs = 60
//! imap_max_message_bytes = 10485760
//!
//! # OAuth2 (replaces username/password when set on either side)
//! oauth2_client_id_env = ""
//! oauth2_client_secret_env = ""
//! oauth2_refresh_token_env = ""
//! oauth2_token_endpoint = ""
//!
//! messages_ring_capacity = 200
//! allowed_senders = []                         # empty == allow everyone
//! operator_address = ""                        # reserved for notifications
//!
//! [email.memory_peer]
//! addr = "/ip4/127.0.0.1/tcp/19711"
//!
//! [email.ai_peer]
//! addr = "/ip4/127.0.0.1/tcp/19712"
//!
//! [email.coord_peer]
//! addr = "/ip4/127.0.0.1/tcp/19714"
//! ```
//!
//! Secrets are loaded from environment variables — *the cleartext
//! password / token never appears in this struct*, only the env
//! variable name. Matches the pattern Telegram's `token_env`
//! uses.

use std::path::PathBuf;

use serde::Deserialize;

/// Per-node email controller configuration.
#[derive(Clone, Debug, Deserialize)]
pub struct EmailNodeConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    // ── SMTP outbound ────────────────────────────────────
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    #[serde(default)]
    pub smtp_username: String,
    /// Name of the env var holding the SMTP password. Empty
    /// disables password auth (use OAuth2 or no auth).
    #[serde(default)]
    pub smtp_password_env: String,
    /// Name of the env var holding an OAuth2 bearer token to
    /// present via SMTP AUTH XOAUTH2. Mutually exclusive with
    /// `smtp_password_env` — when both are set the OAuth2 path
    /// wins.
    #[serde(default)]
    pub smtp_oauth2_token_env: String,
    /// `From:` header. Typically `Name <addr@host>` or plain
    /// `addr@host`. Lettre parses this with the standard mailbox
    /// grammar.
    pub smtp_from: String,
    /// One of `starttls` | `tls` | `none`. Default `starttls`.
    #[serde(default = "default_smtp_tls")]
    pub smtp_tls: String,
    /// Retry attempt count for transient SMTP failures.
    /// Permanent (5xx) failures never retry. Default 3.
    #[serde(default = "default_smtp_retries")]
    pub smtp_max_retries: u32,
    /// Pool max idle connections kept warm by lettre between
    /// sends. Default 8.
    #[serde(default = "default_smtp_pool_max")]
    pub smtp_pool_max: u32,

    // ── DKIM (optional) ──────────────────────────────────
    /// Path to the DKIM private key file (PEM-encoded). Empty
    /// disables signing entirely.
    #[serde(default)]
    pub dkim_private_key_path: PathBuf,
    /// DKIM selector (`s=` tag in DNS).
    #[serde(default)]
    pub dkim_selector: String,
    /// DKIM signing domain (`d=` tag).
    #[serde(default)]
    pub dkim_domain: String,

    // ── IMAP inbound ─────────────────────────────────────
    pub imap_host: String,
    #[serde(default = "default_imap_port")]
    pub imap_port: u16,
    #[serde(default)]
    pub imap_username: String,
    #[serde(default)]
    pub imap_password_env: String,
    /// Optional OAuth2 bearer token env var for IMAP XOAUTH2
    /// auth. Mutually exclusive with `imap_password_env`.
    #[serde(default)]
    pub imap_oauth2_token_env: String,
    #[serde(default = "default_imap_folder")]
    pub imap_folder: String,
    /// Folder to move messages to after dispatch. Empty leaves
    /// them in `imap_folder`, marked `\Seen`.
    #[serde(default)]
    pub imap_processed_folder: String,
    /// Fallback polling interval in seconds for servers that
    /// don't advertise IDLE. IDLE-capable servers get push-style
    /// notification + a 28-minute refresh tick.
    #[serde(default = "default_imap_poll_interval")]
    pub imap_poll_interval_secs: u64,
    /// Maximum allowed inbound message size. Larger messages are
    /// rejected with a bounce reply (5MB by default per RFC; the
    /// 10MB default here matches the spec).
    #[serde(default = "default_imap_max_message_bytes")]
    pub imap_max_message_bytes: u64,

    // ── OAuth2 (refresh-token grant for Google + Microsoft) ──
    /// All four fields must be set together; missing any one is
    /// a validation error.
    #[serde(default)]
    pub oauth2_client_id_env: String,
    #[serde(default)]
    pub oauth2_client_secret_env: String,
    #[serde(default)]
    pub oauth2_refresh_token_env: String,
    #[serde(default)]
    pub oauth2_token_endpoint: String,

    // ── Channel surface ──────────────────────────────────
    #[serde(default = "default_ring_capacity")]
    pub messages_ring_capacity: usize,
    /// Permit-list of sender addresses. Empty == allow everyone.
    /// Match is case-insensitive on the bare addr-spec (the part
    /// inside `<>` if present).
    #[serde(default)]
    pub allowed_senders: Vec<String>,
    /// Operator address — reserved for approval notifications.
    #[serde(default)]
    pub operator_address: String,
    /// SOL flow template path. Reserved (see telegram config).
    #[serde(default)]
    #[allow(dead_code)]
    pub flow_template: PathBuf,

    // ── Peers ────────────────────────────────────────────
    pub memory_peer: MemoryPeerConfig,
    pub ai_peer: AiPeerConfig,
    pub coord_peer: CoordPeerConfig,
}

fn default_enabled() -> bool {
    true
}
fn default_smtp_port() -> u16 {
    587
}
fn default_smtp_tls() -> String {
    "starttls".to_string()
}
fn default_smtp_retries() -> u32 {
    3
}
fn default_smtp_pool_max() -> u32 {
    8
}
fn default_imap_port() -> u16 {
    993
}
fn default_imap_folder() -> String {
    "INBOX".to_string()
}
fn default_imap_poll_interval() -> u64 {
    60
}
fn default_imap_max_message_bytes() -> u64 {
    10 * 1024 * 1024
}
fn default_ring_capacity() -> usize {
    200
}

/// SMTP TLS mode — parsed from `smtp_tls`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmtpTls {
    Starttls,
    Tls,
    None,
}

impl SmtpTls {
    pub fn parse(s: &str) -> Result<Self, EmailNodeError> {
        match s.trim().to_ascii_lowercase().as_str() {
            "starttls" | "" => Ok(Self::Starttls),
            "tls" | "implicit" | "smtps" => Ok(Self::Tls),
            "none" | "plain" | "insecure" => Ok(Self::None),
            other => Err(EmailNodeError::Config(format!(
                "smtp_tls must be one of starttls | tls | none (got {other:?})"
            ))),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct MemoryPeerConfig {
    pub addr: String,
    #[serde(default = "default_memory_alias")]
    pub alias: String,
    #[serde(default = "default_memory_deadline")]
    pub deadline_secs: i64,
}
fn default_memory_alias() -> String {
    "memory".to_string()
}
fn default_memory_deadline() -> i64 {
    10
}

#[derive(Clone, Debug, Deserialize)]
pub struct AiPeerConfig {
    pub addr: String,
    #[serde(default = "default_ai_alias")]
    pub alias: String,
    #[serde(default = "default_ai_deadline")]
    pub deadline_secs: i64,
}
fn default_ai_alias() -> String {
    "ai".to_string()
}
fn default_ai_deadline() -> i64 {
    60
}

#[derive(Clone, Debug, Deserialize)]
pub struct CoordPeerConfig {
    pub addr: String,
    #[serde(default = "default_coord_alias")]
    pub alias: String,
    #[serde(default = "default_coord_deadline")]
    pub deadline_secs: i64,
}
fn default_coord_alias() -> String {
    "coordinator".to_string()
}
fn default_coord_deadline() -> i64 {
    10
}

#[derive(Debug, thiserror::Error)]
pub enum EmailNodeError {
    #[error("email node config: {0}")]
    Config(String),
    #[error("email node: env '{0}' is not set")]
    MissingEnv(String),
}

impl EmailNodeConfig {
    /// Parse the `smtp_tls` string into the typed enum. Used by
    /// the SMTP client builder.
    pub fn smtp_tls_mode(&self) -> Result<SmtpTls, EmailNodeError> {
        SmtpTls::parse(&self.smtp_tls)
    }

    /// True when DKIM signing is enabled (key path + selector +
    /// domain all set). When any of the three is missing
    /// signing is disabled silently — the node logs a warning
    /// at boot but never fails to send.
    pub fn dkim_enabled(&self) -> bool {
        !self.dkim_private_key_path.as_os_str().is_empty()
            && !self.dkim_selector.trim().is_empty()
            && !self.dkim_domain.trim().is_empty()
    }

    /// True when OAuth2 refresh-token grant is configured.
    pub fn oauth2_enabled(&self) -> bool {
        !self.oauth2_client_id_env.trim().is_empty()
            && !self.oauth2_client_secret_env.trim().is_empty()
            && !self.oauth2_refresh_token_env.trim().is_empty()
            && !self.oauth2_token_endpoint.trim().is_empty()
    }

    /// `true` when no permit-list is configured — every sender
    /// is allowed.
    pub fn allow_everyone(&self) -> bool {
        self.allowed_senders.is_empty()
    }

    /// `true` when `sender_addr` (the addr-spec, case-folded) is
    /// in the permit list. Empty list ⇒ always true.
    pub fn sender_is_allowed(&self, sender_addr: &str) -> bool {
        if self.allow_everyone() {
            return true;
        }
        let want = sender_addr.trim().to_ascii_lowercase();
        self.allowed_senders
            .iter()
            .any(|a| a.trim().to_ascii_lowercase() == want)
    }

    /// Read an env var and return its value, returning a
    /// `MissingEnv` error when the name is non-empty but the
    /// variable isn't set / is empty.
    pub fn resolve_env(&self, name: &str) -> Result<String, EmailNodeError> {
        let n = name.trim();
        if n.is_empty() {
            return Err(EmailNodeError::Config(
                "env var name must be non-empty".into(),
            ));
        }
        match std::env::var(n) {
            Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
            _ => Err(EmailNodeError::MissingEnv(n.to_string())),
        }
    }

    /// Resolve the SMTP password from the configured env var.
    pub fn resolve_smtp_password(&self) -> Result<String, EmailNodeError> {
        self.resolve_env(&self.smtp_password_env)
    }

    /// Resolve the IMAP password from the configured env var.
    pub fn resolve_imap_password(&self) -> Result<String, EmailNodeError> {
        self.resolve_env(&self.imap_password_env)
    }

    /// Validate the config without touching env vars or the
    /// network. Catches the common operator mistakes.
    pub fn validate(&self) -> Result<(), EmailNodeError> {
        if self.smtp_host.trim().is_empty() {
            return Err(EmailNodeError::Config("smtp_host is required".into()));
        }
        if self.smtp_from.trim().is_empty() {
            return Err(EmailNodeError::Config("smtp_from is required".into()));
        }
        // Verify smtp_from parses as a mailbox.
        if !looks_like_mailbox(&self.smtp_from) {
            return Err(EmailNodeError::Config(format!(
                "smtp_from is not a valid mailbox: {:?}",
                self.smtp_from
            )));
        }
        if self.imap_host.trim().is_empty() {
            return Err(EmailNodeError::Config("imap_host is required".into()));
        }
        if self.imap_folder.trim().is_empty() {
            return Err(EmailNodeError::Config(
                "imap_folder must be non-empty".into(),
            ));
        }
        let _ = self.smtp_tls_mode()?;
        if self.messages_ring_capacity == 0 {
            return Err(EmailNodeError::Config(
                "messages_ring_capacity must be > 0".into(),
            ));
        }
        if self.imap_max_message_bytes == 0 {
            return Err(EmailNodeError::Config(
                "imap_max_message_bytes must be > 0".into(),
            ));
        }
        if self.memory_peer.addr.trim().is_empty() {
            return Err(EmailNodeError::Config(
                "[email.memory_peer].addr is required".into(),
            ));
        }
        if self.ai_peer.addr.trim().is_empty() {
            return Err(EmailNodeError::Config(
                "[email.ai_peer].addr is required".into(),
            ));
        }
        if self.coord_peer.addr.trim().is_empty() {
            return Err(EmailNodeError::Config(
                "[email.coord_peer].addr is required".into(),
            ));
        }
        // OAuth2: all-or-nothing. Refuse a half-configured block
        // because that's almost always an operator mistake.
        let oauth_fields = [
            ("oauth2_client_id_env", &self.oauth2_client_id_env),
            ("oauth2_client_secret_env", &self.oauth2_client_secret_env),
            ("oauth2_refresh_token_env", &self.oauth2_refresh_token_env),
            ("oauth2_token_endpoint", &self.oauth2_token_endpoint),
        ];
        let set_count = oauth_fields
            .iter()
            .filter(|(_, v)| !v.trim().is_empty())
            .count();
        if set_count > 0 && set_count < oauth_fields.len() {
            let missing: Vec<&str> = oauth_fields
                .iter()
                .filter(|(_, v)| v.trim().is_empty())
                .map(|(n, _)| *n)
                .collect();
            return Err(EmailNodeError::Config(format!(
                "OAuth2 config is partially set; missing: {missing:?}"
            )));
        }
        Ok(())
    }
}

/// Minimal mailbox heuristic — just enough to reject obviously
/// broken `smtp_from` strings at config time. Production parsing
/// happens in lettre when it builds the actual message.
pub fn looks_like_mailbox(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    // Strip a `Name <addr>` envelope, if present.
    let addr_part = if let Some(open) = s.rfind('<') {
        if let Some(close) = s[open + 1..].find('>') {
            &s[open + 1..open + 1 + close]
        } else {
            return false;
        }
    } else {
        s
    };
    let addr_part = addr_part.trim();
    let (local, domain) = match addr_part.split_once('@') {
        Some(pair) => pair,
        None => return false,
    };
    !local.is_empty() && !domain.is_empty() && domain.contains('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_text: &str) -> EmailNodeConfig {
        let v: toml::Value = toml::from_str(toml_text).expect("toml");
        v.try_into().expect("parse")
    }

    fn minimal_toml() -> &'static str {
        r#"
            smtp_host = "smtp.example.com"
            smtp_from = "Relix <bot@example.com>"
            imap_host = "imap.example.com"
            [memory_peer]
            addr = "/ip4/127.0.0.1/tcp/1"
            [ai_peer]
            addr = "/ip4/127.0.0.1/tcp/2"
            [coord_peer]
            addr = "/ip4/127.0.0.1/tcp/3"
        "#
    }

    #[test]
    fn parses_minimal_section_with_defaults() {
        let cfg = parse(minimal_toml());
        assert!(cfg.enabled);
        assert_eq!(cfg.smtp_port, 587);
        assert_eq!(cfg.smtp_tls, "starttls");
        assert_eq!(cfg.imap_port, 993);
        assert_eq!(cfg.imap_folder, "INBOX");
        assert_eq!(cfg.imap_poll_interval_secs, 60);
        assert_eq!(cfg.imap_max_message_bytes, 10 * 1024 * 1024);
        assert_eq!(cfg.messages_ring_capacity, 200);
        assert_eq!(cfg.smtp_max_retries, 3);
        cfg.validate().unwrap();
    }

    #[test]
    fn parses_full_section() {
        let toml_text = r#"
            enabled = true
            smtp_host = "smtp.gmail.com"
            smtp_port = 587
            smtp_username = "relix@example.com"
            smtp_password_env = "EMAIL_SMTP_PASSWORD"
            smtp_from = "Relix <relix@example.com>"
            smtp_tls = "starttls"
            dkim_private_key_path = "/etc/relix/dkim.pem"
            dkim_selector = "relix"
            dkim_domain = "example.com"
            imap_host = "imap.gmail.com"
            imap_port = 993
            imap_username = "relix@example.com"
            imap_password_env = "EMAIL_IMAP_PASSWORD"
            imap_folder = "INBOX"
            imap_processed_folder = "Processed"
            imap_poll_interval_secs = 30
            imap_max_message_bytes = 5242880
            allowed_senders = ["alice@example.com"]
            messages_ring_capacity = 100
            [memory_peer]
            addr = "/ip4/127.0.0.1/tcp/19711"
            [ai_peer]
            addr = "/ip4/127.0.0.1/tcp/19712"
            [coord_peer]
            addr = "/ip4/127.0.0.1/tcp/19714"
        "#;
        let cfg = parse(toml_text);
        cfg.validate().unwrap();
        assert!(cfg.dkim_enabled());
        assert_eq!(cfg.imap_processed_folder, "Processed");
        assert_eq!(cfg.imap_poll_interval_secs, 30);
        assert_eq!(cfg.allowed_senders, vec!["alice@example.com".to_string()]);
    }

    #[test]
    fn smtp_tls_mode_parses_each_variant() {
        assert_eq!(SmtpTls::parse("starttls").unwrap(), SmtpTls::Starttls);
        assert_eq!(SmtpTls::parse("STARTTLS").unwrap(), SmtpTls::Starttls);
        assert_eq!(SmtpTls::parse("tls").unwrap(), SmtpTls::Tls);
        assert_eq!(SmtpTls::parse("none").unwrap(), SmtpTls::None);
        assert!(SmtpTls::parse("garbage").is_err());
    }

    #[test]
    fn validate_rejects_missing_smtp_host() {
        let cfg = parse(
            r#"
                smtp_host = ""
                smtp_from = "bot@example.com"
                imap_host = "imap.example.com"
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        match cfg.validate() {
            Err(EmailNodeError::Config(m)) => assert!(m.contains("smtp_host")),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_missing_smtp_from() {
        let cfg = parse(
            r#"
                smtp_host = "smtp.example.com"
                smtp_from = ""
                imap_host = "imap.example.com"
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_unparseable_smtp_from() {
        let cfg = parse(
            r#"
                smtp_host = "smtp.example.com"
                smtp_from = "not-an-email"
                imap_host = "imap.example.com"
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_partial_oauth2_config() {
        let cfg = parse(
            r#"
                smtp_host = "smtp.example.com"
                smtp_from = "bot@example.com"
                imap_host = "imap.example.com"
                oauth2_client_id_env = "ID"
                # client_secret + refresh_token + token_endpoint missing
                [memory_peer]
                addr = "a"
                [ai_peer]
                addr = "b"
                [coord_peer]
                addr = "c"
            "#,
        );
        match cfg.validate() {
            Err(EmailNodeError::Config(m)) => assert!(m.contains("OAuth2")),
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn sender_permit_list_is_case_insensitive() {
        let mut cfg = parse(minimal_toml());
        cfg.allowed_senders = vec!["Alice@Example.com".into()];
        assert!(cfg.sender_is_allowed("alice@example.com"));
        assert!(cfg.sender_is_allowed("ALICE@EXAMPLE.COM"));
        assert!(!cfg.sender_is_allowed("bob@example.com"));
    }

    #[test]
    fn allow_everyone_when_list_empty() {
        let cfg = parse(minimal_toml());
        assert!(cfg.allow_everyone());
        assert!(cfg.sender_is_allowed("anyone@anywhere.test"));
    }

    #[test]
    fn dkim_disabled_when_any_field_missing() {
        let mut cfg = parse(minimal_toml());
        assert!(!cfg.dkim_enabled());
        cfg.dkim_private_key_path = "/tmp/k.pem".into();
        assert!(!cfg.dkim_enabled()); // selector + domain still missing
        cfg.dkim_selector = "s".into();
        assert!(!cfg.dkim_enabled());
        cfg.dkim_domain = "d.example".into();
        assert!(cfg.dkim_enabled());
    }

    #[test]
    fn resolve_env_surfaces_missing() {
        let cfg = parse(minimal_toml());
        match cfg.resolve_env("RELIX_EMAIL_TEST_DEFINITELY_MISSING_XXXX") {
            Err(EmailNodeError::MissingEnv(n)) => {
                assert_eq!(n, "RELIX_EMAIL_TEST_DEFINITELY_MISSING_XXXX")
            }
            other => panic!("expected MissingEnv, got {other:?}"),
        }
    }

    #[test]
    fn looks_like_mailbox_accepts_plain_and_envelope_forms() {
        assert!(looks_like_mailbox("user@host.example"));
        assert!(looks_like_mailbox("Name <user@host.example>"));
        assert!(!looks_like_mailbox(""));
        assert!(!looks_like_mailbox("no-at-sign"));
        assert!(!looks_like_mailbox("user@"));
        assert!(!looks_like_mailbox("@host.example"));
        assert!(!looks_like_mailbox("user@host"));
    }
}
