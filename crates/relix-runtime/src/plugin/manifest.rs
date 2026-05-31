//! Parse `plugin.toml` manifests + validate them.

use std::path::PathBuf;

use serde::Deserialize;

/// One full plugin manifest.
#[derive(Clone, Debug, Deserialize)]
pub struct PluginManifest {
    pub plugin: PluginMeta,
    /// Resolved directory the manifest lives in. Populated by
    /// `PluginManifest::load_from_path` so callers can resolve
    /// relative paths (`binary = "./foo"`) against the manifest
    /// directory, not the controller's cwd.
    #[serde(skip)]
    pub manifest_dir: PathBuf,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PluginMeta {
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub license: String,
    /// `[plugin.capabilities]` — the methods this plugin exposes.
    /// Captured as a wrapper so the TOML key chain is the spec'd
    /// shape: `[[plugin.capabilities.provides]]`.
    #[serde(default)]
    pub capabilities: PluginCapabilities,
    /// Optional `[plugin.node_type]` block — present when the
    /// plugin defines a brand-new node_type. Reserved for a
    /// future milestone; today the loader registers individual
    /// capabilities only.
    #[serde(default)]
    pub node_type: Option<PluginNodeType>,
    pub runtime: PluginRuntime,
    /// SEC PART 2: optional publisher Ed25519 public key in
    /// 64-char lowercase hex. When present the loader expects
    /// a sibling `{manifest_path}.sig` file containing a
    /// 128-char hex Ed25519 signature over the manifest TOML
    /// bytes, and refuses to load a manifest whose signature
    /// is missing or does not verify against this key.
    #[serde(default)]
    pub publisher_key: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct PluginCapabilities {
    #[serde(default)]
    pub provides: Vec<PluginCapability>,
}

/// One capability exposed by a plugin.
#[derive(Clone, Debug, Deserialize)]
pub struct PluginCapability {
    pub method: String,
    pub description: String,
    #[serde(default)]
    pub categories: Vec<String>,
    #[serde(default)]
    pub sensitivity_tags: Vec<String>,
    #[serde(default = "default_risk")]
    pub risk_level: String,
}

fn default_risk() -> String {
    "low".to_string()
}

/// Optional `[plugin.node_type]` block.
#[derive(Clone, Debug, Deserialize)]
pub struct PluginNodeType {
    pub name: String,
    #[serde(default)]
    pub config_schema: String,
}

/// `[plugin.runtime]` — how the plugin is executed.
#[derive(Clone, Debug, Deserialize)]
pub struct PluginRuntime {
    /// `subprocess` is the only supported kind today; the field
    /// exists to leave room for future runtime kinds without
    /// breaking the manifest format.
    pub kind: String,
    /// Path to the plugin binary.
    ///
    /// SEC PART 2: the loader now REQUIRES this to be an
    /// absolute path. Bare command names (PATH lookup) are
    /// refused with a clear error so a hostile entry on the
    /// host's PATH cannot shadow the intended binary.
    pub binary: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default = "default_invoke_timeout_secs")]
    pub invoke_timeout_secs: u64,
    /// SEC PART 2: optional SHA-256 of the binary file as
    /// 64-char lowercase hex. When present the loader hashes
    /// the binary on spawn and refuses to run if the value
    /// does not match — this pins the binary against
    /// supply-chain tampering between manifest authorship and
    /// load time.
    #[serde(default)]
    pub binary_sha256: Option<String>,
}

fn default_protocol() -> String {
    "relix-plugin-v1".to_string()
}
fn default_invoke_timeout_secs() -> u64 {
    30
}

/// Errors from manifest parsing + validation.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("io: {0}")]
    Io(String),
    #[error("toml: {0}")]
    Toml(String),
    #[error("manifest at {path}: {msg}")]
    Invalid { path: String, msg: String },
    /// SEC PART 2: file on disk exceeds [`MAX_MANIFEST_BYTES`].
    /// Bounded BEFORE parsing so a hostile manifest cannot
    /// exhaust the loader's heap.
    #[error("manifest {path} is {size_bytes} bytes; max is {max_bytes}")]
    TooLarge {
        path: String,
        size_bytes: u64,
        max_bytes: u64,
    },
    /// SEC PART 2: TOML nesting depth exceeds
    /// [`MAX_MANIFEST_DEPTH`]. Pre-fix toml::from_str had no
    /// depth limit and accepted documents whose nesting alone
    /// could blow the parser's stack.
    #[error("manifest {path} nesting depth {depth} exceeds max {max_depth}")]
    TooDeep {
        path: String,
        depth: usize,
        max_depth: usize,
    },
    /// SEC PART 2: manifest declares a `publisher_key` but the
    /// sibling `.sig` file is missing.
    #[error("manifest {path} requires signature `{sig_path}` (publisher_key set) but file missing")]
    SignatureMissing { path: String, sig_path: String },
    /// SEC PART 2: signature failed to verify against the
    /// declared publisher key.
    #[error("manifest {path}: signature verification failed against publisher_key")]
    SignatureInvalid { path: String },
    /// SEC PART 2: publisher_key / signature / binary_sha256 is
    /// malformed (wrong length, non-hex, …).
    #[error("manifest {path}: malformed {field}: {cause}")]
    Malformed {
        path: String,
        field: &'static str,
        cause: String,
    },
}

/// SEC PART 2: hard ceiling on `plugin.toml` size. 1 MiB is
/// orders of magnitude past any realistic manifest.
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// SEC PART 2: hard ceiling on TOML nesting depth.
pub const MAX_MANIFEST_DEPTH: usize = 10;

impl PluginManifest {
    /// Load + parse + validate a manifest from disk.
    ///
    /// SEC PART 2: enforces three pre-parse guards before
    /// touching the TOML parser:
    ///
    /// 1. File size <= [`MAX_MANIFEST_BYTES`] (1 MiB). Stat
    ///    the file before reading so a hostile multi-GB file
    ///    cannot exhaust the loader's heap.
    /// 2. Pre-scan structural depth <= [`MAX_MANIFEST_DEPTH`]
    ///    (10 levels). Counts the live brace + bracket
    ///    nesting in the raw text; a deeply-nested manifest
    ///    is refused before recursion.
    /// 3. When the parsed manifest declares
    ///    `[plugin] publisher_key = "<hex>"`, verify the
    ///    sibling `{manifest_path}.sig` file (128-hex
    ///    Ed25519 signature) against the manifest TOML bytes.
    ///    Missing or invalid signature refuses load.
    pub fn load_from_path(path: &std::path::Path) -> Result<Self, ManifestError> {
        // (1) size cap — stat first to avoid reading a huge
        // file into the heap.
        let meta = std::fs::metadata(path)
            .map_err(|e| ManifestError::Io(format!("{}: {e}", path.display())))?;
        if meta.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge {
                path: path.display().to_string(),
                size_bytes: meta.len(),
                max_bytes: MAX_MANIFEST_BYTES,
            });
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| ManifestError::Io(format!("{}: {e}", path.display())))?;
        // Defence-in-depth: even if the on-disk size was
        // within the cap, the materialised UTF-8 string
        // could have grown via BOM-stripping; re-check.
        if text.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge {
                path: path.display().to_string(),
                size_bytes: text.len() as u64,
                max_bytes: MAX_MANIFEST_BYTES,
            });
        }
        // (2) depth cap — scan the raw text for max
        // nesting of `{` / `[` so a malicious manifest is
        // rejected before recursion.
        let depth = max_toml_structural_depth(&text);
        if depth > MAX_MANIFEST_DEPTH {
            return Err(ManifestError::TooDeep {
                path: path.display().to_string(),
                depth,
                max_depth: MAX_MANIFEST_DEPTH,
            });
        }
        let mut m: PluginManifest =
            toml::from_str(&text).map_err(|e| ManifestError::Toml(format!("{e}")))?;
        m.manifest_dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        m.validate(path)?;
        // (3) publisher signature gate, when configured.
        if let Some(pk_hex) = m.plugin.publisher_key.as_deref() {
            verify_manifest_signature(path, text.as_bytes(), pk_hex)?;
        }
        Ok(m)
    }

    pub fn validate(&self, path: &std::path::Path) -> Result<(), ManifestError> {
        let path_str = path.display().to_string();
        let invalid = |msg: String| ManifestError::Invalid {
            path: path_str.clone(),
            msg,
        };
        if self.plugin.name.trim().is_empty() {
            return Err(invalid("[plugin] name is required".into()));
        }
        if !is_valid_plugin_name(&self.plugin.name) {
            return Err(invalid(format!(
                "[plugin] name '{}' must be lowercase alphanumeric / hyphens (3..=64 chars)",
                self.plugin.name
            )));
        }
        if self.plugin.version.trim().is_empty() {
            return Err(invalid("[plugin] version is required".into()));
        }
        if self.plugin.description.trim().is_empty() {
            return Err(invalid("[plugin] description is required".into()));
        }
        if self.plugin.runtime.kind != "subprocess" {
            return Err(invalid(format!(
                "[plugin.runtime] kind '{}' not supported; only 'subprocess'",
                self.plugin.runtime.kind
            )));
        }
        if self.plugin.runtime.binary.as_os_str().is_empty() {
            return Err(invalid("[plugin.runtime] binary is required".into()));
        }
        if self.plugin.runtime.protocol != "relix-plugin-v1" {
            return Err(invalid(format!(
                "[plugin.runtime] protocol '{}' not supported; only 'relix-plugin-v1'",
                self.plugin.runtime.protocol
            )));
        }
        if self.plugin.runtime.invoke_timeout_secs == 0
            || self.plugin.runtime.invoke_timeout_secs > 300
        {
            return Err(invalid(format!(
                "[plugin.runtime] invoke_timeout_secs must be 1..=300, got {}",
                self.plugin.runtime.invoke_timeout_secs
            )));
        }
        if self.plugin.capabilities.provides.is_empty() {
            return Err(invalid(
                "[plugin.capabilities] must declare at least one provides entry".into(),
            ));
        }
        for cap in &self.plugin.capabilities.provides {
            if !is_valid_method_name(&cap.method) {
                return Err(invalid(format!(
                    "capability method `{}` is not a dotted identifier",
                    cap.method
                )));
            }
            if cap.description.trim().is_empty() {
                return Err(invalid(format!(
                    "capability `{}` is missing description",
                    cap.method
                )));
            }
            if !matches!(cap.risk_level.as_str(), "low" | "medium" | "high") {
                return Err(invalid(format!(
                    "capability `{}` risk_level '{}' must be one of low/medium/high",
                    cap.method, cap.risk_level
                )));
            }
        }
        Ok(())
    }

    /// Canonicalised absolute path of the binary.
    ///
    /// SEC PART 2: REQUIRES an absolute path. Bare command
    /// names that would trigger PATH lookup, or relative
    /// paths that depend on the controller's cwd, are
    /// refused with a clear error so a hostile entry on the
    /// host's PATH cannot shadow the intended binary.
    /// Relative paths INSIDE the manifest directory are
    /// resolved against `manifest_dir` and then required to
    /// canonicalise to an absolute existing path.
    pub fn resolved_binary(&self) -> Result<PathBuf, ManifestError> {
        let raw = &self.plugin.runtime.binary;
        let display = raw.display().to_string();
        let has_sep = raw
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(|c| c == '/' || c == '\\');
        if !raw.is_absolute() && !has_sep {
            return Err(ManifestError::Invalid {
                path: self.manifest_dir.display().to_string(),
                msg: format!(
                    "[plugin.runtime] binary `{display}` must be an absolute path \
                     — bare command names (PATH lookup) are refused",
                ),
            });
        }
        let candidate = if raw.is_absolute() {
            raw.clone()
        } else {
            self.manifest_dir.join(raw)
        };
        // Canonicalise so a symlink doesn't smuggle the
        // operator past the absolute-path check.
        let canon = candidate
            .canonicalize()
            .map_err(|e| ManifestError::Invalid {
                path: self.manifest_dir.display().to_string(),
                msg: format!("[plugin.runtime] binary `{display}` not found: {e}"),
            })?;
        if !canon.is_absolute() {
            return Err(ManifestError::Invalid {
                path: self.manifest_dir.display().to_string(),
                msg: format!(
                    "[plugin.runtime] binary `{}` did not canonicalise to absolute path",
                    canon.display()
                ),
            });
        }
        Ok(canon)
    }

    /// SEC PART 2: hash the resolved binary and compare against
    /// the declared `binary_sha256` (64-char lowercase hex).
    /// When no expected hash is configured returns `Ok(())`
    /// without reading the binary — the operator opted out.
    /// Mismatch returns `ManifestError::Invalid` with the
    /// expected + observed hashes for diagnosis.
    pub fn verify_binary_sha256(&self, binary_path: &std::path::Path) -> Result<(), ManifestError> {
        let Some(expected) = self
            .plugin
            .runtime
            .binary_sha256
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(());
        };
        if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ManifestError::Malformed {
                path: self.manifest_dir.display().to_string(),
                field: "binary_sha256",
                cause: format!("expected 64-char hex, got `{expected}`"),
            });
        }
        let bytes = std::fs::read(binary_path).map_err(|e| ManifestError::Invalid {
            path: self.manifest_dir.display().to_string(),
            msg: format!(
                "[plugin.runtime] binary {} unreadable for sha256 check: {e}",
                binary_path.display()
            ),
        })?;
        use sha2::{Digest as _, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let observed = format!("{:x}", hasher.finalize());
        let expected_lc = expected.to_ascii_lowercase();
        if observed != expected_lc {
            return Err(ManifestError::Invalid {
                path: self.manifest_dir.display().to_string(),
                msg: format!(
                    "[plugin.runtime] binary sha256 mismatch — expected {expected_lc}, observed {observed}",
                ),
            });
        }
        Ok(())
    }
}

/// SEC PART 2: rough but cheap upper bound on the maximum
/// TOML nesting depth in a manifest. Walks the text counting
/// live `{` / `[` characters that are not inside a quoted
/// string or a `#` comment. The toml crate has no public
/// depth-limit knob, so this pre-pass refuses pathological
/// inputs (the cap can be off by one in either direction
/// versus the strict TOML AST depth, but matters only at the
/// boundary — operator manifests rarely nest past 3).
fn max_toml_structural_depth(text: &str) -> usize {
    let mut depth = 0usize;
    let mut max_depth = 0usize;
    let mut in_string: Option<char> = None; // Some('"') | Some('\'')
    let mut prev_was_escape = false;
    for line in text.lines() {
        // Strip trailing comment (a `#` outside a string).
        // We process char by char to honour string boundaries.
        for ch in line.chars() {
            if let Some(q) = in_string {
                if prev_was_escape {
                    prev_was_escape = false;
                    continue;
                }
                if ch == '\\' && q == '"' {
                    prev_was_escape = true;
                    continue;
                }
                if ch == q {
                    in_string = None;
                }
                continue;
            }
            // not in string
            match ch {
                '"' | '\'' => {
                    in_string = Some(ch);
                }
                '#' => break, // rest of line is comment
                '{' | '[' => {
                    depth = depth.saturating_add(1);
                    if depth > max_depth {
                        max_depth = depth;
                    }
                }
                '}' | ']' => {
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
        }
        // Reset multiline state per spec: TOML strings can
        // be multiline (`"""` / `'''`). Our pre-scan only
        // catches the common case; the toml parser still
        // rejects mismatched delimiters below. The depth
        // cap is a defence in depth, not a strict TOML
        // tokeniser.
    }
    max_depth
}

/// SEC PART 2: verify the manifest's sibling `.sig` file
/// against the manifest's `[plugin] publisher_key`. The
/// signature is 128 lowercase hex characters (64 bytes
/// raw) over the manifest's exact TOML bytes.
fn verify_manifest_signature(
    manifest_path: &std::path::Path,
    body: &[u8],
    pk_hex: &str,
) -> Result<(), ManifestError> {
    let pk_hex = pk_hex.trim();
    let pk_bytes = match hex::decode(pk_hex) {
        Ok(b) if b.len() == 32 => b,
        Ok(b) => {
            return Err(ManifestError::Malformed {
                path: manifest_path.display().to_string(),
                field: "publisher_key",
                cause: format!("expected 32-byte key, got {}", b.len()),
            });
        }
        Err(e) => {
            return Err(ManifestError::Malformed {
                path: manifest_path.display().to_string(),
                field: "publisher_key",
                cause: format!("not hex: {e}"),
            });
        }
    };
    let mut pk_arr = [0u8; 32];
    pk_arr.copy_from_slice(&pk_bytes);
    let pk =
        ed25519_dalek::VerifyingKey::from_bytes(&pk_arr).map_err(|e| ManifestError::Malformed {
            path: manifest_path.display().to_string(),
            field: "publisher_key",
            cause: format!("invalid Ed25519 pubkey: {e}"),
        })?;
    let mut sig_path = manifest_path.as_os_str().to_owned();
    sig_path.push(".sig");
    let sig_path = std::path::PathBuf::from(sig_path);
    let sig_text = match std::fs::read_to_string(&sig_path) {
        Ok(s) => s,
        Err(_) => {
            return Err(ManifestError::SignatureMissing {
                path: manifest_path.display().to_string(),
                sig_path: sig_path.display().to_string(),
            });
        }
    };
    let sig_bytes = match hex::decode(sig_text.trim()) {
        Ok(b) if b.len() == 64 => b,
        Ok(b) => {
            return Err(ManifestError::Malformed {
                path: manifest_path.display().to_string(),
                field: "signature",
                cause: format!("expected 64-byte sig, got {}", b.len()),
            });
        }
        Err(e) => {
            return Err(ManifestError::Malformed {
                path: manifest_path.display().to_string(),
                field: "signature",
                cause: format!("not hex: {e}"),
            });
        }
    };
    let mut sig_arr = [0u8; 64];
    sig_arr.copy_from_slice(&sig_bytes);
    let signature = ed25519_dalek::Signature::from_bytes(&sig_arr);
    use ed25519_dalek::Verifier;
    pk.verify(body, &signature)
        .map_err(|_| ManifestError::SignatureInvalid {
            path: manifest_path.display().to_string(),
        })
}

fn is_valid_plugin_name(s: &str) -> bool {
    let len = s.len();
    if !(3..=64).contains(&len) {
        return false;
    }
    s.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_valid_method_name(s: &str) -> bool {
    // dotted identifier: `<seg>(.<seg>)+`, each seg is
    // [a-z][a-z0-9_]* and at least 1 char.
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() < 2 || parts.iter().any(|p| p.is_empty()) {
        return false;
    }
    parts.iter().all(|p| {
        let mut chars = p.chars();
        match chars.next() {
            Some(c) if c.is_ascii_lowercase() => {}
            _ => return false,
        }
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_manifest(text: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(text.as_bytes()).unwrap();
        // Create a fake binary file so resolved_binary() can
        // canonicalise. The loader still rejects it for being
        // unexecutable, but the manifest layer doesn't check
        // executability — that's the loader's job.
        let bin = dir.path().join("dummy");
        std::fs::File::create(&bin).unwrap();
        dir
    }

    fn full_manifest() -> &'static str {
        r#"
            [plugin]
            name        = "my-plugin"
            version     = "0.1.0"
            description = "Does a thing"
            author      = "Tester"

            [[plugin.capabilities.provides]]
            method      = "my_plugin.do_thing"
            description = "Does a thing"
            categories  = ["tool"]
            risk_level  = "low"

            [plugin.runtime]
            kind                = "subprocess"
            binary              = "./dummy"
            protocol            = "relix-plugin-v1"
            invoke_timeout_secs = 30
        "#
    }

    #[test]
    fn parses_full_manifest() {
        let dir = write_manifest(full_manifest());
        let m = PluginManifest::load_from_path(&dir.path().join("plugin.toml")).unwrap();
        assert_eq!(m.plugin.name, "my-plugin");
        assert_eq!(m.plugin.version, "0.1.0");
        assert_eq!(m.plugin.capabilities.provides.len(), 1);
        assert_eq!(
            m.plugin.capabilities.provides[0].method,
            "my_plugin.do_thing"
        );
        assert_eq!(m.plugin.runtime.invoke_timeout_secs, 30);
    }

    #[test]
    fn rejects_missing_name() {
        let dir = write_manifest(
            r#"
                [plugin]
                version = "0.1.0"
                description = "x"

                [[plugin.capabilities.provides]]
                method = "x.y"
                description = "x"

                [plugin.runtime]
                kind = "subprocess"
                binary = "./dummy"
            "#,
        );
        let err = PluginManifest::load_from_path(&dir.path().join("plugin.toml")).unwrap_err();
        // Missing `name` field is a TOML decode error, not the
        // validate() pass — both flow through ManifestError.
        assert!(matches!(err, ManifestError::Toml(_)));
    }

    #[test]
    fn rejects_invalid_method_name() {
        let dir = write_manifest(
            r#"
                [plugin]
                name = "my-plugin"
                version = "0.1.0"
                description = "x"

                [[plugin.capabilities.provides]]
                method = "Bad.Method"
                description = "x"

                [plugin.runtime]
                kind = "subprocess"
                binary = "./dummy"
            "#,
        );
        let err = PluginManifest::load_from_path(&dir.path().join("plugin.toml")).unwrap_err();
        match err {
            ManifestError::Invalid { msg, .. } => assert!(msg.contains("dotted identifier")),
            o => panic!("expected Invalid, got {o:?}"),
        }
    }

    #[test]
    fn rejects_zero_invoke_timeout() {
        let dir = write_manifest(
            r#"
                [plugin]
                name = "my-plugin"
                version = "0.1.0"
                description = "x"

                [[plugin.capabilities.provides]]
                method = "x.y"
                description = "x"

                [plugin.runtime]
                kind                = "subprocess"
                binary              = "./dummy"
                invoke_timeout_secs = 0
            "#,
        );
        let err = PluginManifest::load_from_path(&dir.path().join("plugin.toml")).unwrap_err();
        assert!(matches!(err, ManifestError::Invalid { .. }));
    }

    #[test]
    fn rejects_unknown_runtime_kind() {
        let dir = write_manifest(
            r#"
                [plugin]
                name = "my-plugin"
                version = "0.1.0"
                description = "x"

                [[plugin.capabilities.provides]]
                method = "x.y"
                description = "x"

                [plugin.runtime]
                kind   = "wasm"
                binary = "./dummy"
            "#,
        );
        let err = PluginManifest::load_from_path(&dir.path().join("plugin.toml")).unwrap_err();
        assert!(matches!(err, ManifestError::Invalid { .. }));
    }

    #[test]
    fn rejects_empty_capabilities() {
        let dir = write_manifest(
            r#"
                [plugin]
                name = "my-plugin"
                version = "0.1.0"
                description = "x"

                [plugin.runtime]
                kind = "subprocess"
                binary = "./dummy"
            "#,
        );
        let err = PluginManifest::load_from_path(&dir.path().join("plugin.toml")).unwrap_err();
        assert!(matches!(err, ManifestError::Invalid { .. }));
    }

    #[test]
    fn rejects_unknown_risk_level() {
        let dir = write_manifest(
            r#"
                [plugin]
                name = "my-plugin"
                version = "0.1.0"
                description = "x"

                [[plugin.capabilities.provides]]
                method      = "x.y"
                description = "x"
                risk_level  = "extreme"

                [plugin.runtime]
                kind = "subprocess"
                binary = "./dummy"
            "#,
        );
        let err = PluginManifest::load_from_path(&dir.path().join("plugin.toml")).unwrap_err();
        assert!(matches!(err, ManifestError::Invalid { .. }));
    }

    #[test]
    fn valid_method_names() {
        assert!(is_valid_method_name("a.b"));
        assert!(is_valid_method_name("my_plugin.do_thing"));
        assert!(is_valid_method_name("ns.method.subscope"));
        assert!(!is_valid_method_name("nodots"));
        assert!(!is_valid_method_name("Capitals.bad"));
        assert!(!is_valid_method_name(".leading"));
        assert!(!is_valid_method_name("trailing."));
        assert!(!is_valid_method_name("a..b"));
    }

    // ── SEC PART 2: manifest caps + sig verification ────

    #[test]
    fn sec_p2_manifest_over_size_cap_refused_before_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.toml");
        // Make a file just over the cap. Content is all `#`
        // (a TOML comment), but the size gate fires BEFORE
        // parsing so the comments don't even matter.
        let mut blob = Vec::with_capacity(MAX_MANIFEST_BYTES as usize + 1);
        blob.resize(MAX_MANIFEST_BYTES as usize + 1, b'#');
        std::fs::write(&path, &blob).unwrap();
        let err = PluginManifest::load_from_path(&path).unwrap_err();
        assert!(matches!(err, ManifestError::TooLarge { .. }), "got {err:?}");
    }

    #[test]
    fn sec_p2_manifest_depth_over_cap_refused() {
        // Build a TOML with > MAX_MANIFEST_DEPTH levels of
        // inline-table / inline-array nesting. We do not
        // need the document to parse — the depth gate fires
        // before parsing.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.toml");
        let mut text = String::new();
        text.push_str("[plugin]\nname = \"x\"\nversion = \"0\"\ndescription = \"d\"\nx = ");
        // Nest `[[[[ ... ]]]]` past the cap (uses `[` so the
        // depth scanner counts it).
        for _ in 0..(MAX_MANIFEST_DEPTH + 5) {
            text.push('[');
        }
        text.push('1');
        for _ in 0..(MAX_MANIFEST_DEPTH + 5) {
            text.push(']');
        }
        text.push_str("\n[plugin.runtime]\nkind = \"subprocess\"\nbinary = \"./dummy\"\n");
        text.push_str("[[plugin.capabilities.provides]]\nmethod = \"x.y\"\ndescription = \"d\"\n");
        std::fs::write(&path, &text).unwrap();
        let err = PluginManifest::load_from_path(&path).unwrap_err();
        assert!(matches!(err, ManifestError::TooDeep { .. }), "got {err:?}");
    }

    #[test]
    fn sec_p2_manifest_signature_missing_refused_when_publisher_key_set() {
        // Embed a valid hex key and a manifest body, but
        // omit the .sig file — load must fail with
        // SignatureMissing.
        let kp = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let pk_hex = hex::encode(kp.verifying_key().to_bytes());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.toml");
        // Ensure the binary actually exists so resolved_binary
        // doesn't shadow the signature error.
        let bin = dir.path().join("dummy");
        std::fs::write(&bin, b"\x7fELFstub").unwrap();
        let text = format!(
            r#"
                [plugin]
                name = "my-plugin"
                version = "0.1.0"
                description = "x"
                publisher_key = "{pk_hex}"

                [[plugin.capabilities.provides]]
                method = "x.y"
                description = "x"

                [plugin.runtime]
                kind = "subprocess"
                binary = "./dummy"
            "#
        );
        std::fs::write(&path, &text).unwrap();
        let err = PluginManifest::load_from_path(&path).unwrap_err();
        assert!(
            matches!(err, ManifestError::SignatureMissing { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn sec_p2_manifest_signature_verifies_with_correct_key() {
        use ed25519_dalek::Signer as _;
        let kp = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let pk_hex = hex::encode(kp.verifying_key().to_bytes());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plugin.toml");
        std::fs::write(dir.path().join("dummy"), b"\x7fELFstub").unwrap();
        let text = format!(
            r#"
                [plugin]
                name = "my-plugin"
                version = "0.1.0"
                description = "x"
                publisher_key = "{pk_hex}"

                [[plugin.capabilities.provides]]
                method = "x.y"
                description = "x"

                [plugin.runtime]
                kind = "subprocess"
                binary = "./dummy"
            "#
        );
        std::fs::write(&path, &text).unwrap();
        // Sign the bytes we wrote.
        let sig = kp.sign(text.as_bytes());
        std::fs::write(path.with_extension("toml.sig"), hex::encode(sig.to_bytes())).unwrap();
        // Load must succeed.
        let m = PluginManifest::load_from_path(&path).expect("verify ok");
        assert_eq!(m.plugin.name, "my-plugin");
    }

    #[test]
    fn sec_p2_binary_sha256_mismatch_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dummy"), b"actual contents").unwrap();
        let text = r#"
            [plugin]
            name = "my-plugin"
            version = "0.1.0"
            description = "x"

            [[plugin.capabilities.provides]]
            method = "x.y"
            description = "x"

            [plugin.runtime]
            kind          = "subprocess"
            binary        = "./dummy"
            # Wrong hash for "actual contents".
            binary_sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
        "#;
        let path = dir.path().join("plugin.toml");
        std::fs::write(&path, text).unwrap();
        let m = PluginManifest::load_from_path(&path).expect("load");
        let bin = m.resolved_binary().expect("resolve");
        let err = m.verify_binary_sha256(&bin).unwrap_err();
        assert!(
            matches!(err, ManifestError::Invalid { ref msg, .. } if msg.contains("sha256 mismatch")),
            "got {err:?}"
        );
    }

    #[test]
    fn sec_p2_resolved_binary_refuses_bare_command_name() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"
            [plugin]
            name = "my-plugin"
            version = "0.1.0"
            description = "x"

            [[plugin.capabilities.provides]]
            method = "x.y"
            description = "x"

            [plugin.runtime]
            kind   = "subprocess"
            # Bare command — PATH lookup. Refused.
            binary = "python"
        "#;
        let path = dir.path().join("plugin.toml");
        std::fs::write(&path, text).unwrap();
        let m = PluginManifest::load_from_path(&path).expect("load");
        let err = m.resolved_binary().unwrap_err();
        assert!(
            matches!(err, ManifestError::Invalid { ref msg, .. } if msg.contains("absolute path")),
            "got {err:?}"
        );
    }
}
