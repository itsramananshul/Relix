//! `relix-cli identity ...` subcommands.
//!
//! M2 deliverable: init-org, mint, inspect — using `relix_core::identity`.

use clap::Subcommand;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::fs;
use std::path::{Path, PathBuf};

use relix_core::bundle::{
    Bundle, BundleError, BundleType, DEFAULT_IDENTITY_LIFETIME_SECS, DEFAULT_RENEWAL_WINDOW_SECS,
};
use relix_core::codec;
use relix_core::identity::{IdentityBundle, issue_identity, validate_identity_bundle};
use relix_core::types::NodeId;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Generate an org-root keypair.
    ///
    /// Writes 32 raw secret-key bytes to `--root-key` and prints the org-root
    /// public key hash (= `org_id`) to stdout.
    InitOrg {
        /// Output path for the org-root signing key (32 raw bytes; 0600 on POSIX).
        #[arg(long)]
        root_key: PathBuf,
        /// Human-readable org label (recorded in the printed banner only).
        #[arg(long)]
        org: String,
    },
    /// Mint an alpha IdentityBundle for a subject.
    Mint {
        /// Org-root signing-key file (from `init-org`).
        #[arg(long)]
        root_key: PathBuf,
        /// Subject name (e.g., `alice`).
        #[arg(long)]
        name: String,
        /// Comma-separated groups (e.g., `chat-users,tool-users`).
        #[arg(long, value_delimiter = ',', num_args = 0..)]
        groups: Vec<String>,
        /// Role (default `agent`).
        #[arg(long, default_value = "agent")]
        role: String,
        /// Clearance (default `internal`).
        #[arg(long, default_value = "internal")]
        clearance: String,
        /// Lifetime in hours. Defaults to 365 days (8760h) for self-hosted
        /// node/service identities — long enough that normal operation never
        /// hits expiry; expiry still applies as a revocation backstop.
        #[arg(long, default_value_t = DEFAULT_IDENTITY_LIFETIME_SECS / 3600)]
        hours: i64,
        /// Output path for the signed bundle (raw CBOR bytes).
        #[arg(long)]
        out: PathBuf,
        /// Optional output path for the subject's signing key. If omitted, a new
        /// key is generated and discarded after computing subject_id (alpha
        /// shortcut for human users whose only signed action is logging in).
        #[arg(long)]
        subject_key: Option<PathBuf>,
    },
    /// Self-healing mint: ensure a valid, non-expiring identity bundle exists
    /// at `--out`, (re)minting it when it is missing, unreadable, expired,
    /// signed by a different/!current org root, or within the renewal window
    /// of `--not-after`. Idempotent and cheap when the bundle is healthy, so
    /// it is safe to call on every boot AND on a periodic renewal timer for a
    /// long-running mesh. This is what keeps a fresh install always bootable
    /// and a months-long mesh from lapsing.
    Ensure {
        /// Org-root signing-key file (from `init-org`).
        #[arg(long)]
        root_key: PathBuf,
        /// Subject name (e.g., `web-bridge`).
        #[arg(long)]
        name: String,
        /// Comma-separated groups (e.g., `chat-users`).
        #[arg(long, value_delimiter = ',', num_args = 0..)]
        groups: Vec<String>,
        /// Role (default `agent`).
        #[arg(long, default_value = "agent")]
        role: String,
        /// Clearance (default `internal`).
        #[arg(long, default_value = "internal")]
        clearance: String,
        /// Lifetime in hours for a (re)mint. Defaults to 365 days (8760h).
        #[arg(long, default_value_t = DEFAULT_IDENTITY_LIFETIME_SECS / 3600)]
        hours: i64,
        /// Re-mint when the existing bundle is within this many days of
        /// expiry (renewal window). Defaults to 30 days.
        #[arg(long, default_value_t = DEFAULT_RENEWAL_WINDOW_SECS / 86_400)]
        renewal_window_days: i64,
        /// Output path for the signed bundle (raw CBOR bytes).
        #[arg(long)]
        out: PathBuf,
        /// Optional persisted subject signing-key path (reused across renewals).
        #[arg(long)]
        subject_key: Option<PathBuf>,
    },
    /// Print the contents of an IdentityBundle file.
    Inspect {
        /// Path to the bundle file.
        #[arg(long)]
        bundle: PathBuf,
        /// Path to the org-root key (for signature verification).
        #[arg(long)]
        root_key: PathBuf,
    },
    /// RELIX-7.30 PART 3: issue a per-session token.
    Issue {
        #[arg(long)]
        session: String,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        tenant: Option<String>,
        /// Comma-separated capability scopes.
        #[arg(long, value_delimiter = ',', num_args = 0..)]
        scopes: Vec<String>,
        #[arg(long)]
        ttl_secs: Option<u64>,
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// RELIX-7.30 PART 3: verify a wire-encoded session token.
    Verify {
        /// SEC §12: the wire token is read from this 0600 file, or
        /// from stdin when omitted — never an argv flag (which
        /// would be visible in `ps` / shell history / journald).
        #[arg(long)]
        token_file: Option<PathBuf>,
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// RELIX-7.30 PART 3: revoke every active token for a
    /// session.
    Revoke {
        #[arg(long)]
        session: String,
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// RELIX-7.30 PART 3: list active session tokens,
    /// optionally filtered by agent.
    Tokens {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// RELIX-7.18 / GAP 17 PART 2: research a subject via web
    /// search + LLM synthesis and persist the resulting
    /// IdentityProfile to layered memory.
    Research {
        /// Subject name (e.g., "Anshul Raman").
        #[arg(long)]
        subject: String,
        /// Optional disambiguating context (e.g., "engineer at
        /// Acme, located in Berlin").
        #[arg(long)]
        context: Option<String>,
        #[arg(long, default_value = crate::defaults::DEFAULT_BRIDGE_URL)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::InitOrg { root_key, org } => init_org(&root_key, &org),
        Cmd::Mint {
            root_key,
            name,
            groups,
            role,
            clearance,
            hours,
            out,
            subject_key,
        } => mint(
            &root_key,
            &name,
            &groups,
            &role,
            &clearance,
            hours,
            &out,
            subject_key.as_deref(),
        ),
        Cmd::Ensure {
            root_key,
            name,
            groups,
            role,
            clearance,
            hours,
            renewal_window_days,
            out,
            subject_key,
        } => ensure(
            &root_key,
            &name,
            &groups,
            &role,
            &clearance,
            hours,
            renewal_window_days,
            &out,
            subject_key.as_deref(),
        ),
        Cmd::Inspect { bundle, root_key } => inspect(&bundle, &root_key),
        Cmd::Issue {
            session,
            agent,
            tenant,
            scopes,
            ttl_secs,
            bridge,
            raw,
        } => {
            issue_token(
                &bridge,
                &session,
                &agent,
                tenant.as_deref(),
                scopes,
                ttl_secs,
                raw,
            )
            .await
        }
        Cmd::Verify {
            token_file,
            bridge,
            raw,
        } => {
            let token = crate::secret_input::read_secret(token_file.as_deref())
                .map_err(|e| format!("identity verify: {e}"))?;
            verify_token(&bridge, token.as_str(), raw).await
        }
        Cmd::Revoke {
            session,
            bridge,
            raw,
        } => revoke_token(&bridge, &session, raw).await,
        Cmd::Tokens { agent, bridge, raw } => list_tokens(&bridge, agent.as_deref(), raw).await,
        Cmd::Research {
            subject,
            context,
            bridge,
            raw,
        } => research(&bridge, &subject, context.as_deref(), raw).await,
    }
}

async fn issue_token(
    bridge: &str,
    session: &str,
    agent: &str,
    tenant: Option<&str>,
    scopes: Vec<String>,
    ttl_secs: Option<u64>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/identity/tokens", bridge.trim_end_matches('/'));
    let mut payload = serde_json::Map::new();
    payload.insert("session_id".into(), serde_json::Value::from(session));
    payload.insert("agent_name".into(), serde_json::Value::from(agent));
    if let Some(t) = tenant {
        payload.insert("tenant_id".into(), serde_json::Value::from(t));
    }
    payload.insert("scopes".into(), serde_json::Value::from(scopes));
    if let Some(ttl) = ttl_secs {
        payload.insert("ttl_secs".into(), serde_json::Value::from(ttl));
    }
    let body = http_post_json(&url, &serde_json::Value::Object(payload)).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("decode issue: {e} (body={body})"))?;
    let wire = v
        .get("wire")
        .and_then(|x| x.as_str())
        .unwrap_or("(missing wire)");
    let tok = v.get("token");
    println!("wire_token:    {wire}");
    if let Some(t) = tok {
        if let Some(s) = t.get("session_id").and_then(|x| x.as_str()) {
            println!("session_id:    {s}");
        }
        if let Some(s) = t.get("agent_name").and_then(|x| x.as_str()) {
            println!("agent:         {s}");
        }
        if let Some(s) = t.get("tenant_id").and_then(|x| x.as_str()) {
            println!("tenant:        {s}");
        }
        if let Some(s) = t.get("expires_at_ms").and_then(|x| x.as_i64()) {
            println!("expires_at_ms: {s}");
        }
    }
    Ok(())
}

async fn verify_token(
    bridge: &str,
    token: &str,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/identity/tokens/verify", bridge.trim_end_matches('/'));
    let body = http_post_json(&url, &serde_json::json!({ "token": token })).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("decode verify: {e} (body={body})"))?;
    let valid = v.get("valid").and_then(|x| x.as_bool()).unwrap_or(false);
    println!("valid:        {valid}");
    if valid {
        if let Some(s) = v.get("session_id").and_then(|x| x.as_str()) {
            println!("session_id:   {s}");
        }
        if let Some(s) = v.get("agent_name").and_then(|x| x.as_str()) {
            println!("agent:        {s}");
        }
        if let Some(s) = v.get("expires_at_ms").and_then(|x| x.as_i64()) {
            println!("expires_at_ms:{s}");
        }
    } else if let Some(r) = v.get("reason").and_then(|x| x.as_str()) {
        println!("reason:       {r}");
    }
    Ok(())
}

async fn revoke_token(
    bridge: &str,
    session: &str,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/identity/tokens/revoke", bridge.trim_end_matches('/'));
    let body = http_post_json(&url, &serde_json::json!({ "session_id": session })).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("decode revoke: {e} (body={body})"))?;
    let n = v.get("revoked_count").and_then(|x| x.as_u64()).unwrap_or(0);
    println!("revoked {n} token(s) for session {session}");
    Ok(())
}

async fn list_tokens(
    bridge: &str,
    agent: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut url = format!("{}/v1/identity/tokens", bridge.trim_end_matches('/'));
    if let Some(a) = agent {
        url.push_str(&format!("?agent_name={a}"));
    }
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("decode tokens: {e} (body={body})"))?;
    let arr = v.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        println!("(no active tokens)");
        return Ok(());
    }
    println!(
        "{:<24} {:<24} {:<14} {:<14} status",
        "token_id", "session", "agent", "tenant"
    );
    for r in arr {
        let tid = r.get("token_id").and_then(|x| x.as_str()).unwrap_or("?");
        let sid = r.get("session_id").and_then(|x| x.as_str()).unwrap_or("?");
        let agent = r.get("agent_name").and_then(|x| x.as_str()).unwrap_or("?");
        let tenant = r.get("tenant_id").and_then(|x| x.as_str()).unwrap_or("-");
        let revoked = r.get("revoked").and_then(|x| x.as_bool()).unwrap_or(false);
        let status = if revoked { "revoked" } else { "active" };
        println!("{tid:<24} {sid:<24} {agent:<14} {tenant:<14} {status}");
    }
    Ok(())
}

async fn research(
    bridge: &str,
    subject: &str,
    context: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/identity/research", bridge.trim_end_matches('/'));
    let mut payload = serde_json::Map::new();
    payload.insert("subject_name".into(), serde_json::Value::from(subject));
    if let Some(c) = context {
        payload.insert("context".into(), serde_json::Value::from(c));
    }
    let body = http_post_json_long(&url, &serde_json::Value::Object(payload)).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("decode research: {e} (body={body})"))?;
    let approved = v.get("approved").and_then(|x| x.as_bool()).unwrap_or(false);
    let verdict = v
        .get("approval_verdict")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown");
    let provider = v
        .get("provider_used")
        .and_then(|x| x.as_str())
        .unwrap_or("?");
    let consulted = v
        .get("results_consulted")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let queries = v
        .get("queries_generated")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    println!("subject:           {subject}");
    println!("approval_verdict:  {verdict}");
    println!("approved:          {approved}");
    println!("search_provider:   {provider}");
    println!("results_consulted: {consulted}");
    println!("queries_generated: {}", queries.len());
    for q in &queries {
        if let Some(s) = q.as_str() {
            println!("  - {s}");
        }
    }
    if let Some(profile) = v.get("profile") {
        if let Some(s) = profile.get("display_name").and_then(|x| x.as_str()) {
            println!("display_name:      {s}");
        }
        if let Some(s) = profile.get("professional_role").and_then(|x| x.as_str()) {
            println!("professional_role: {s}");
        }
        if let Some(s) = profile.get("organization").and_then(|x| x.as_str()) {
            println!("organization:      {s}");
        }
        if let Some(s) = profile.get("location").and_then(|x| x.as_str()) {
            println!("location:          {s}");
        }
        if let Some(c) = profile.get("confidence").and_then(|x| x.as_f64()) {
            println!("confidence:        {c:.2}");
        }
        if let Some(arr) = profile.get("expertise_areas").and_then(|x| x.as_array()) {
            let joined: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
            if !joined.is_empty() {
                println!("expertise_areas:   {}", joined.join(", "));
            }
        }
        if let Some(arr) = profile.get("public_profiles").and_then(|x| x.as_array()) {
            for p in arr {
                let plat = p.get("platform").and_then(|x| x.as_str()).unwrap_or("?");
                let u = p.get("url").and_then(|x| x.as_str()).unwrap_or("?");
                println!("  profile:         {plat} {u}");
            }
        }
        if let Some(arr) = profile.get("sources_used").and_then(|x| x.as_array()) {
            for s in arr {
                if let Some(u) = s.as_str() {
                    println!("  source:          {u}");
                }
            }
        }
    }
    if let Some(id) = v.get("memory_record_id").and_then(|x| x.as_str()) {
        println!("memory_record_id:  {id}");
    }
    if let Some(id) = v.get("approval_id").and_then(|x| x.as_str()) {
        println!("approval_id:       {id}");
    }
    Ok(())
}

async fn http_post_json_long(
    url: &str,
    payload: &serde_json::Value,
) -> Result<String, Box<dyn std::error::Error>> {
    // The bridge endpoint allows the pipeline up to ~10 minutes
    // to wait for an operator approval; mirror that on the
    // client so we don't drop the request early.
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(660))
        .build()?
        .post(url)
        .json(payload)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}").into());
    }
    Ok(body)
}

async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?
        .get(url)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}").into());
    }
    Ok(body)
}

async fn http_post_json(
    url: &str,
    payload: &serde_json::Value,
) -> Result<String, Box<dyn std::error::Error>> {
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?
        .post(url)
        .json(payload)
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}").into());
    }
    Ok(body)
}

fn init_org(root_key_path: &Path, org_label: &str) -> Result<(), Box<dyn std::error::Error>> {
    if root_key_path.exists() {
        return Err(format!(
            "refusing to overwrite existing key file: {}",
            root_key_path.display()
        )
        .into());
    }
    let key = SigningKey::generate(&mut OsRng);
    if let Some(parent) = root_key_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_secret_key(root_key_path, &key)?;

    // Also write the companion .pub file (32-byte Ed25519 public key) next to
    // the secret. Trust-root config (`[trust] org_root_key_path = ...`) MUST
    // point at the .pub file, never the .key file.
    let pub_path = pub_sibling(root_key_path);
    fs::write(&pub_path, key.verifying_key().to_bytes())?;

    let org_id = NodeId::from_pubkey(&key.verifying_key().to_bytes());
    println!("# Relix org bootstrap");
    println!("org-label: {}", org_label);
    println!("org-id:    {}", org_id);
    println!("key-path:  {}", root_key_path.display());
    println!("pub-path:  {}", pub_path.display());
    println!("# Keep the .key file private. It is gitignored. Trust files reference the .pub.");
    Ok(())
}

/// Derive the conventional sibling pubkey path from a secret-key path.
/// `foo.key` → `foo.pub`; anything else → `<path>.pub`.
fn pub_sibling(key_path: &Path) -> std::path::PathBuf {
    if key_path.extension().and_then(|s| s.to_str()) == Some("key") {
        key_path.with_extension("pub")
    } else {
        let mut p = key_path.as_os_str().to_owned();
        p.push(".pub");
        std::path::PathBuf::from(p)
    }
}

#[allow(clippy::too_many_arguments)]
fn mint(
    root_key_path: &Path,
    name: &str,
    groups: &[String],
    role: &str,
    clearance: &str,
    hours: i64,
    out_path: &Path,
    subject_key_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let root_key = read_secret_key(root_key_path)?;

    let subject_key = match subject_key_path {
        Some(p) if p.exists() => read_secret_key(p)?,
        Some(p) => {
            let k = SigningKey::generate(&mut OsRng);
            write_secret_key(p, &k)?;
            k
        }
        None => SigningKey::generate(&mut OsRng),
    };
    let subject_id = NodeId::from_pubkey(&subject_key.verifying_key().to_bytes());
    let org_id = NodeId::from_pubkey(&root_key.verifying_key().to_bytes());

    let payload = IdentityBundle {
        subject_id,
        name: name.to_string(),
        org_id,
        groups: groups.to_vec(),
        role: role.to_string(),
        clearance: clearance.to_string(),
        supervisors: vec![],
    };
    let bundle = issue_identity(payload, &root_key, hours * 3600)?;
    let bytes = codec::encode(&bundle)?;
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(out_path, &bytes)?;

    println!("# Minted identity");
    println!("name:       {}", name);
    println!("subject-id: {}", subject_id);
    println!("groups:     {:?}", groups);
    println!("bundle:     {} ({} bytes)", out_path.display(), bytes.len());
    println!("expires-in: {}h", hours);
    Ok(())
}

fn now_unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Self-healing / renewing mint. Decides whether the bundle at `out_path`
/// needs (re)minting and, if so, mints it with the current org root. The
/// decision uses the same [`relix_core::bundle::BundleHeader::needs_renewal`]
/// primitive the runtime renewal path relies on, so boot-time self-heal and
/// periodic renewal share one rule.
#[allow(clippy::too_many_arguments)]
fn ensure(
    root_key_path: &Path,
    name: &str,
    groups: &[String],
    role: &str,
    clearance: &str,
    hours: i64,
    renewal_window_days: i64,
    out_path: &Path,
    subject_key_path: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    let window_secs = renewal_window_days.saturating_mul(86_400);
    // Determine whether (and why) a (re)mint is required. `None` => healthy.
    let reason: Option<String> = if !out_path.exists() {
        Some("missing".to_string())
    } else {
        match fs::read(out_path) {
            Err(e) => Some(format!("unreadable: {e}")),
            Ok(bytes) => {
                let decoded: Result<Bundle, _> = codec::decode(&bytes);
                match decoded {
                    Err(e) => Some(format!("corrupt: {e}")),
                    Ok(bundle) => {
                        // Validate against the CURRENT org root — a bundle
                        // signed by a stale/foreign root (e.g. a pre-minted
                        // bundle shipped in a checkout) fails here and is
                        // self-healed rather than left to break boot.
                        let root_key = read_secret_key(root_key_path)?;
                        let now = now_unix_secs();
                        match bundle.validate(&root_key.verifying_key(), BundleType::Identity, now)
                        {
                            Ok(()) => {
                                if bundle.header.needs_renewal(now, window_secs) {
                                    let days = bundle.header.seconds_until_expiry(now) / 86_400;
                                    Some(format!("near-expiry ({days}d remaining)"))
                                } else {
                                    None
                                }
                            }
                            Err(BundleError::Expired) => Some("expired".to_string()),
                            Err(e) => Some(format!("invalid for current org root: {e}")),
                        }
                    }
                }
            }
        }
    };

    match reason {
        None => {
            // Healthy and outside the renewal window — report remaining life.
            if let Ok(bytes) = fs::read(out_path)
                && let Ok(bundle) = codec::decode::<Bundle>(&bytes)
            {
                let days = bundle.header.seconds_until_expiry(now_unix_secs()) / 86_400;
                println!("ensure: {name} bundle valid ({days}d remaining); no action");
                return Ok(());
            }
            println!("ensure: {name} bundle valid; no action");
            Ok(())
        }
        Some(why) => {
            println!("ensure: (re)minting {name} bundle ({why})");
            mint(
                root_key_path,
                name,
                groups,
                role,
                clearance,
                hours,
                out_path,
                subject_key_path,
            )
        }
    }
}

fn inspect(bundle_path: &Path, root_key_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = fs::read(bundle_path)?;
    let bundle: Bundle = codec::decode(&bytes)?;
    let root_key = read_secret_key(root_key_path)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let verified = validate_identity_bundle(&bundle, &root_key.verifying_key(), now)?;
    let bid = bundle.bundle_id()?;
    println!("# IdentityBundle inspection");
    println!("bundle-id:   {}", hex::encode(bid));
    println!("subject-id:  {}", verified.subject_id);
    println!("name:        {}", verified.name);
    println!("org-id:      {}", verified.org_id);
    println!("groups:      {:?}", verified.groups);
    println!("role:        {}", verified.role);
    println!("clearance:   {}", verified.clearance);
    println!("not_before:  {}", bundle.header.not_before);
    println!("not_after:   {}", bundle.header.not_after);
    Ok(())
}

fn read_secret_key(path: &Path) -> Result<SigningKey, Box<dyn std::error::Error>> {
    let bytes = fs::read(path)?;
    if bytes.len() != 32 {
        return Err(format!(
            "expected 32-byte secret key, got {} bytes from {}",
            bytes.len(),
            path.display()
        )
        .into());
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(SigningKey::from_bytes(&arr))
}

fn write_secret_key(path: &Path, key: &SigningKey) -> Result<(), Box<dyn std::error::Error>> {
    let bytes = key.to_bytes();
    fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = fs::metadata(path)?.permissions();
        p.set_mode(0o600);
        fs::set_permissions(path, p)?;
    }
    Ok(())
}
