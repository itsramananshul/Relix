//! `relix-cli identity ...` subcommands.
//!
//! M2 deliverable: init-org, mint, inspect — using `relix_core::identity`.

use clap::Subcommand;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::fs;
use std::path::{Path, PathBuf};

use relix_core::bundle::Bundle;
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
        /// Lifetime in hours (default 24).
        #[arg(long, default_value_t = 24)]
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
    /// Print the contents of an IdentityBundle file.
    Inspect {
        /// Path to the bundle file.
        #[arg(long)]
        bundle: PathBuf,
        /// Path to the org-root key (for signature verification).
        #[arg(long)]
        root_key: PathBuf,
    },
}

pub fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
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
        Cmd::Inspect { bundle, root_key } => inspect(&bundle, &root_key),
    }
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
    let org_id = NodeId::from_pubkey(&key.verifying_key().to_bytes());
    println!("# Relix org bootstrap");
    println!("org-label: {}", org_label);
    println!("org-id:    {}", org_id);
    println!("key-path:  {}", root_key_path.display());
    println!("# Keep the key file private. It is gitignored.");
    Ok(())
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
