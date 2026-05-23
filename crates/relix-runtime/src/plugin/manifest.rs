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
    /// Path to the plugin binary, resolved relative to
    /// `manifest_dir`. The loader canonicalises the path before
    /// `Command::new()`.
    pub binary: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default = "default_invoke_timeout_secs")]
    pub invoke_timeout_secs: u64,
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
}

impl PluginManifest {
    /// Load + parse + validate a manifest from disk.
    pub fn load_from_path(path: &std::path::Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ManifestError::Io(format!("{}: {e}", path.display())))?;
        let mut m: PluginManifest =
            toml::from_str(&text).map_err(|e| ManifestError::Toml(format!("{e}")))?;
        m.manifest_dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        m.validate(path)?;
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

    /// Canonicalised absolute path of the binary, resolved
    /// against the manifest directory.
    ///
    /// Bare command names (no path separator) are returned
    /// verbatim so `Command::new` does the usual PATH lookup —
    /// that's how `binary = "python"` reaches the system Python
    /// without forcing the operator to write a full path.
    pub fn resolved_binary(&self) -> PathBuf {
        let raw = &self.plugin.runtime.binary;
        let has_sep = raw
            .as_os_str()
            .to_string_lossy()
            .chars()
            .any(|c| c == '/' || c == '\\');
        if !raw.is_absolute() && !has_sep {
            // Bare name → PATH lookup at Command::new time.
            return raw.clone();
        }
        let candidate = if raw.is_absolute() {
            raw.clone()
        } else {
            self.manifest_dir.join(raw)
        };
        candidate.canonicalize().unwrap_or(candidate)
    }
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
}
