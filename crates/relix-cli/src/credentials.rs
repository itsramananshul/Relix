//! `relix credentials ...` — RELIX-7.30 PART 2 operator surface.

use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;
use serde_json::Value;

const DEFAULT_BRIDGE: &str = crate::defaults::DEFAULT_BRIDGE_URL;

#[derive(Subcommand, Debug)]
pub enum Cmd {
    Store {
        #[arg(long)]
        name: String,
        /// SEC §12: the secret value is read from this 0600 file,
        /// or from stdin when omitted — never an argv flag (which
        /// would be visible in `ps` / shell history / journald).
        #[arg(long)]
        value_file: Option<PathBuf>,
        #[arg(long, default_value = "api_key")]
        kind: String,
        #[arg(long)]
        owner: Option<String>,
        /// Expiry as a unix-millisecond timestamp. Operators
        /// pass `--expires-at-ms` rather than an ISO string so
        /// the CLI stays parser-free.
        #[arg(long)]
        expires_at_ms: Option<i64>,
        /// Rotation interval in seconds. The scheduler emits a
        /// notification every interval until the operator
        /// rotates the value.
        #[arg(long)]
        rotate_every: Option<u64>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    List {
        #[arg(long)]
        owner: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    Rotate {
        #[arg(long)]
        name: String,
        /// SEC §12: the new secret value is read from this 0600
        /// file, or from stdin when omitted — never an argv flag.
        #[arg(long)]
        new_value_file: Option<PathBuf>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    Revoke {
        #[arg(long)]
        name: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    Audit {
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value = DEFAULT_BRIDGE)]
        bridge: String,
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// SEC PART 1: rebuild a legacy v1-format vault.
    MigrateKdf {
        /// Vault DB path. Direct file access — no bridge call.
        #[arg(long)]
        db_path: PathBuf,
        /// Env var holding the legacy SHA-256 master secret.
        #[arg(long, default_value = "RELIX_CREDENTIAL_KEY")]
        legacy_key_env: String,
        /// Active key version name to stamp on rewritten rows.
        #[arg(long, default_value = "v1")]
        new_key_version: String,
        /// Env var holding the new master secret used to derive
        /// the new Argon2id-protected AES key.
        #[arg(long, default_value = "RELIX_CREDENTIAL_KEY")]
        new_key_version_env: String,
        /// Argon2id memory cost in KiB.
        #[arg(long, default_value_t = 65_536)]
        argon2_memory_cost: u32,
        /// Argon2id time cost (iterations).
        #[arg(long, default_value_t = 3)]
        argon2_time_cost: u32,
        /// Argon2id parallelism.
        #[arg(long, default_value_t = 4)]
        argon2_parallelism: u32,
    },
    /// SEC PART 7: rotate the vault key.
    RotateVaultKey {
        /// Vault DB path.
        #[arg(long)]
        db_path: PathBuf,
        /// `name=env_var` pair for each known key version.
        /// Repeat the flag once per version, e.g.
        /// `--key-version v1=RELIX_CREDENTIAL_KEY
        ///  --key-version v2=RELIX_CREDENTIAL_KEY_V2`. The
        /// highest-numbered version with a non-empty env var
        /// becomes the active write key.
        #[arg(long = "key-version", value_name = "NAME=ENV_VAR")]
        key_versions: Vec<String>,
        /// Argon2id memory cost in KiB (must match the value
        /// the vault was created with — re-derivation under
        /// different params won't decrypt existing rows).
        #[arg(long, default_value_t = 65_536)]
        argon2_memory_cost: u32,
        #[arg(long, default_value_t = 3)]
        argon2_time_cost: u32,
        #[arg(long, default_value_t = 4)]
        argon2_parallelism: u32,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Store {
            name,
            value_file,
            kind,
            owner,
            expires_at_ms,
            rotate_every,
            bridge,
            raw,
        } => {
            let value = crate::secret_input::read_secret(value_file.as_deref())
                .map_err(|e| format!("credentials store: {e}"))?;
            store(
                &bridge,
                &name,
                value.as_str(),
                &kind,
                owner.as_deref(),
                expires_at_ms,
                rotate_every,
                raw,
            )
            .await
        }
        Cmd::List { owner, bridge, raw } => list(&bridge, owner.as_deref(), raw).await,
        Cmd::Rotate {
            name,
            new_value_file,
            bridge,
            raw,
        } => {
            let new_value = crate::secret_input::read_secret(new_value_file.as_deref())
                .map_err(|e| format!("credentials rotate: {e}"))?;
            rotate(&bridge, &name, new_value.as_str(), raw).await
        }
        Cmd::Revoke {
            name,
            reason,
            bridge,
            raw,
        } => revoke(&bridge, &name, reason.as_deref(), raw).await,
        Cmd::Audit {
            name,
            limit,
            bridge,
            raw,
        } => audit(&bridge, &name, limit, raw).await,
        Cmd::MigrateKdf {
            db_path,
            legacy_key_env,
            new_key_version,
            new_key_version_env,
            argon2_memory_cost,
            argon2_time_cost,
            argon2_parallelism,
        } => migrate_kdf(
            db_path,
            &legacy_key_env,
            &new_key_version,
            &new_key_version_env,
            argon2_memory_cost,
            argon2_time_cost,
            argon2_parallelism,
        ),
        Cmd::RotateVaultKey {
            db_path,
            key_versions,
            argon2_memory_cost,
            argon2_time_cost,
            argon2_parallelism,
        } => rotate_vault_key(
            db_path,
            &key_versions,
            argon2_memory_cost,
            argon2_time_cost,
            argon2_parallelism,
        ),
    }
}

fn migrate_kdf(
    db_path: PathBuf,
    legacy_key_env: &str,
    new_key_version: &str,
    new_key_version_env: &str,
    argon2_memory_cost: u32,
    argon2_time_cost: u32,
    argon2_parallelism: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    use relix_runtime::credentials::store::{self, KdfParams, KeyVersionMap};
    let legacy_master = std::env::var(legacy_key_env).unwrap_or_default();
    if legacy_master.is_empty() {
        return Err(format!(
            "credentials migrate-kdf: legacy key env var `{legacy_key_env}` is unset or empty"
        )
        .into());
    }
    let new_master = std::env::var(new_key_version_env).unwrap_or_default();
    if new_master.is_empty() {
        return Err(format!(
            "credentials migrate-kdf: new-version key env var \
             `{new_key_version_env}` is unset or empty"
        )
        .into());
    }
    let mut keys = KeyVersionMap::default();
    keys.insert(new_key_version.to_string(), new_master);
    let params = KdfParams {
        memory_cost_kib: argon2_memory_cost,
        time_cost: argon2_time_cost,
        parallelism: argon2_parallelism,
    };
    let report = store::migrate_kdf(&db_path, &legacy_master, keys, params)?;
    println!(
        "vault migrated: {} row(s) re-encrypted under key version `{}`",
        report.rows_rotated, report.active_version
    );
    Ok(())
}

fn rotate_vault_key(
    db_path: PathBuf,
    key_versions: &[String],
    argon2_memory_cost: u32,
    argon2_time_cost: u32,
    argon2_parallelism: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    use relix_runtime::credentials::store::{CredentialStore, KdfParams, KeyVersionMap};
    if key_versions.is_empty() {
        return Err("credentials rotate-vault-key: at least one --key-version is required".into());
    }
    let mut map = KeyVersionMap::default();
    for raw in key_versions {
        let (name, env_var) = match raw.split_once('=') {
            Some(pair) => pair,
            None => {
                return Err(format!(
                    "credentials rotate-vault-key: invalid --key-version `{raw}` (use NAME=ENV_VAR)"
                )
                .into());
            }
        };
        let secret = std::env::var(env_var).unwrap_or_default();
        if secret.is_empty() {
            // Skip — never panic an operator's rotation because
            // an env var was unset; just warn and continue.
            eprintln!("warning: --key-version `{name}` env var `{env_var}` is empty; skipping");
            continue;
        }
        map.insert(name.to_string(), secret);
    }
    if map.is_empty() {
        return Err("credentials rotate-vault-key: every --key-version env var was empty".into());
    }
    let params = KdfParams {
        memory_cost_kib: argon2_memory_cost,
        time_cost: argon2_time_cost,
        parallelism: argon2_parallelism,
    };
    let store = CredentialStore::open_with_params(&db_path, map, params, false)?;
    let report = store.rotate_vault_key(Some("relix credentials rotate-vault-key"))?;
    println!(
        "vault key rotated: {} row(s) re-encrypted under active version `{}`",
        report.rows_rotated, report.active_version
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn store(
    bridge: &str,
    name: &str,
    value: &str,
    kind: &str,
    owner: Option<&str>,
    expires_at_ms: Option<i64>,
    rotate_every: Option<u64>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{}/v1/credentials", bridge.trim_end_matches('/'));
    let mut payload = serde_json::Map::new();
    payload.insert("name".into(), Value::from(name));
    payload.insert("value".into(), Value::from(value));
    payload.insert("kind".into(), Value::from(kind));
    if let Some(o) = owner {
        payload.insert("owner_agent".into(), Value::from(o));
    }
    if let Some(e) = expires_at_ms {
        payload.insert("expires_at_ms".into(), Value::from(e));
    }
    if let Some(r) = rotate_every {
        payload.insert("rotation_interval_secs".into(), Value::from(r));
    }
    let body = http_post_json(&url, &Value::Object(payload)).await?;
    if raw {
        println!("{body}");
    } else {
        print_summary(&body)?;
    }
    Ok(())
}

async fn list(
    bridge: &str,
    owner: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut url = format!("{}/v1/credentials", bridge.trim_end_matches('/'));
    if let Some(o) = owner {
        url.push_str(&format!("?owner_agent={}", urlencode(o)));
    }
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: Value =
        serde_json::from_str(&body).map_err(|e| format!("decode list: {e} (body={body})"))?;
    let arr = v.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        println!("(no credentials)");
        return Ok(());
    }
    println!(
        "{:<24} {:<10} {:<14} {:<3} status",
        "name", "kind", "owner", "ver"
    );
    for r in arr {
        let name = r.get("name").and_then(|x| x.as_str()).unwrap_or("?");
        let kind = r.get("kind").and_then(|x| x.as_str()).unwrap_or("?");
        let owner = r.get("owner_agent").and_then(|x| x.as_str()).unwrap_or("-");
        let ver = r.get("version").and_then(|x| x.as_u64()).unwrap_or(0);
        let revoked = r.get("revoked").and_then(|x| x.as_bool()).unwrap_or(false);
        let status = if revoked { "revoked" } else { "active" };
        println!("{name:<24} {kind:<10} {owner:<14} {ver:<3} {status}");
    }
    Ok(())
}

async fn rotate(
    bridge: &str,
    name: &str,
    new_value: &str,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/credentials/{}/rotate",
        bridge.trim_end_matches('/'),
        urlencode(name)
    );
    let body = http_post_json(&url, &serde_json::json!({ "new_value": new_value })).await?;
    if raw {
        println!("{body}");
    } else {
        print_summary(&body)?;
    }
    Ok(())
}

async fn revoke(
    bridge: &str,
    name: &str,
    reason: Option<&str>,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/credentials/{}/revoke",
        bridge.trim_end_matches('/'),
        urlencode(name)
    );
    let mut payload = serde_json::Map::new();
    if let Some(r) = reason {
        payload.insert("reason".into(), Value::from(r));
    }
    let body = http_post_json(&url, &Value::Object(payload)).await?;
    if raw {
        println!("{body}");
    } else {
        print_summary(&body)?;
    }
    Ok(())
}

async fn audit(
    bridge: &str,
    name: &str,
    limit: usize,
    raw: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!(
        "{}/v1/credentials/{}/audit?limit={}",
        bridge.trim_end_matches('/'),
        urlencode(name),
        limit
    );
    let body = http_get(&url).await?;
    if raw {
        println!("{body}");
        return Ok(());
    }
    let v: Value =
        serde_json::from_str(&body).map_err(|e| format!("decode audit: {e} (body={body})"))?;
    let arr = v.as_array().cloned().unwrap_or_default();
    if arr.is_empty() {
        println!("(no audit rows)");
        return Ok(());
    }
    for row in arr {
        let ts = row
            .get("timestamp_ms")
            .and_then(|x| x.as_i64())
            .unwrap_or(0);
        let ev = row.get("event").and_then(|x| x.as_str()).unwrap_or("?");
        let actor = row.get("actor").and_then(|x| x.as_str()).unwrap_or("-");
        let details = row.get("details").and_then(|x| x.as_str()).unwrap_or("");
        println!("{ts:>13}  {ev:<10}  by {actor:<14}  {details}");
    }
    Ok(())
}

fn print_summary(body: &str) -> Result<(), Box<dyn std::error::Error>> {
    let v: Value = serde_json::from_str(body)?;
    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("?");
    let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("?");
    let ver = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0);
    let revoked = v.get("revoked").and_then(|x| x.as_bool()).unwrap_or(false);
    println!(
        "{name} (kind={kind} version={ver} status={})",
        if revoked { "revoked" } else { "active" }
    );
    Ok(())
}

async fn http_get(url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
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

async fn http_post_json(url: &str, payload: &Value) -> Result<String, Box<dyn std::error::Error>> {
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
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

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe = b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~';
        if safe {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(&mut out, "%{b:02X}");
        }
    }
    out
}
