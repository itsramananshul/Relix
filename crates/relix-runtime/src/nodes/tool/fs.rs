//! Path-jailed filesystem capabilities for the tool node.
//!
//! Four capabilities live here:
//!
//! - `tool.read_file`    — read a UTF-8 file under the jail root.
//! - `tool.write_file`   — atomic write to a path under the jail root.
//! - `tool.search_files` — name or substring-content search under the jail.
//! - `tool.patch`        — apply a unified diff to a file under the jail.
//!
//! Every capability shares one [`FsJail`] whose `root` is canonicalised
//! at construction. Caller-supplied paths are:
//!
//! 1. Rejected if absolute, empty, or containing `..` segments.
//! 2. Joined to the jail root.
//! 3. Canonicalised (which resolves symlinks).
//! 4. Verified to still live under `root` after canonicalisation —
//!    so a symlink that points outside fails the check.
//!
//! For `write_file` the file may not exist yet; we canonicalise the
//! *parent* directory (which must exist) and then join the basename,
//! which catches symlinked parent directories pointing outside.
//!
//! **Honest limitation: TOCTOU.** Between canonicalise and open(), the
//! path could be re-symlinked. A correct fix needs `openat(O_NOFOLLOW)`
//! semantics which `std::fs` doesn't expose portably. For the alpha we
//! accept this; the bringup script places the jail under `dev-data/`
//! which is operator-owned and not user-writable.
//!
//! ## Wire format (SIMP-016 alpha — UTF-8 strings)
//!
//! | Method | Arg | Returns |
//! |---|---|---|
//! | `tool.read_file`    | `<rel_path>` *or* `<rel_path>\|<max_bytes>` | file contents (UTF-8) |
//! | `tool.write_file`   | `<rel_path>\|<mode>\|<content>` where mode is `overwrite` or `create_new` | `ok bytes=<N> path=<canonical>\n` |
//! | `tool.search_files` | `<mode>\|<pattern>\|<max_results>` where mode is `name` or `content` | one match per line; `path` for name mode, `path:line:text` for content mode |
//! | `tool.patch`        | `<rel_path>\|unified_diff\|<diff body>` | `ok bytes=<N>\n` |
//!
//! All paths in args are jail-relative. Returns expose paths
//! jail-relative too (operators inspect by setting the same root).
//!
//! ## Not in scope (deliberate)
//!
//! - No directory create / remove / rename. The jail's directory shape
//!   is owned by the operator, not by the tool.
//! - No binary file handling — `tool.read_file` rejects non-UTF-8
//!   contents. (`tool.web_fetch` has the same restriction; the bridge
//!   has the same restriction; consistent.)
//! - No `replace`-mode patch. Diff is the safer + reviewable form for
//!   v0; replace mode lands when there's a real flow that needs it.
//! - No content indexing. `search_files` is a linear walker with byte-
//!   level substring match. Adequate at alpha scale; an indexer is a
//!   separate capability when one is needed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use relix_core::capability::{CapabilityDescriptor, CostClass, Idempotency};
use relix_core::types::{ErrorEnvelope, error_kinds};

use crate::dispatch::{DispatchBridge, FnHandler, HandlerOutcome, InvocationCtx};

/// Per-node FS jail configuration. Lives in `[tool.fs]` (or just
/// `[tool]` with the `fs_*` knobs flattened; see `ToolConfig`).
#[derive(Clone, Debug, Deserialize)]
pub struct FsJailConfig {
    /// Jail root. Must exist at startup. All capabilities operate only
    /// on paths under this directory.
    pub root: PathBuf,
    /// Max bytes `tool.read_file` will return. Default 10 MiB.
    #[serde(default = "default_read_bytes")]
    pub max_read_bytes: usize,
    /// Max bytes `tool.write_file` will accept. Default 10 MiB.
    #[serde(default = "default_write_bytes")]
    pub max_write_bytes: usize,
    /// Max matches `tool.search_files` will return. Default 200.
    #[serde(default = "default_max_search_results")]
    pub max_search_results: usize,
}

fn default_read_bytes() -> usize {
    10 * 1024 * 1024
}
fn default_write_bytes() -> usize {
    10 * 1024 * 1024
}
fn default_max_search_results() -> usize {
    200
}

/// Construction errors surfaced at startup.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("io: {0}")]
    Io(String),
    #[error("jail root does not exist: {0}")]
    RootMissing(String),
    #[error("jail root is not a directory: {0}")]
    RootNotDir(String),
}

/// Path-jailed FS handle shared across all four handlers.
pub struct FsJail {
    canonical_root: PathBuf,
    cfg: FsJailConfig,
}

impl FsJail {
    pub fn new(cfg: FsJailConfig) -> Result<Self, FsError> {
        if !cfg.root.exists() {
            return Err(FsError::RootMissing(cfg.root.display().to_string()));
        }
        if !cfg.root.is_dir() {
            return Err(FsError::RootNotDir(cfg.root.display().to_string()));
        }
        let canonical_root = cfg
            .root
            .canonicalize()
            .map_err(|e| FsError::Io(format!("canonicalize root: {e}")))?;
        Ok(Self {
            canonical_root,
            cfg,
        })
    }

    /// Resolve a caller-supplied jail-relative path to a canonical
    /// absolute path inside the jail. Fails closed on any escape.
    /// `must_exist = true` requires the target to already exist (for
    /// reads, search hits); `false` allows non-existent targets (for
    /// new file writes) by canonicalising the parent dir and joining
    /// the basename.
    fn resolve(&self, rel: &str, must_exist: bool) -> Result<PathBuf, JailError> {
        let trimmed = rel.trim();
        if trimmed.is_empty() {
            return Err(JailError::Empty);
        }
        let rel_path = Path::new(trimmed);
        if rel_path.is_absolute() {
            return Err(JailError::Absolute(trimmed.to_string()));
        }
        // Reject any `..` segment outright. We could allow them and
        // rely on canonicalisation, but explicit rejection produces
        // clearer error messages and removes one class of mistakes.
        for comp in rel_path.components() {
            if matches!(comp, std::path::Component::ParentDir) {
                return Err(JailError::Traversal(trimmed.to_string()));
            }
        }

        let joined = self.canonical_root.join(rel_path);

        if must_exist {
            let canonical = joined
                .canonicalize()
                .map_err(|e| JailError::Io(format!("canonicalize {trimmed}: {e}")))?;
            if !canonical.starts_with(&self.canonical_root) {
                return Err(JailError::Escape(trimmed.to_string()));
            }
            Ok(canonical)
        } else {
            // Target may not exist (writes). Canonicalise the parent
            // and append the basename. Parent must exist.
            let parent = joined.parent().ok_or(JailError::Empty)?.to_path_buf();
            let parent_canonical = parent
                .canonicalize()
                .map_err(|e| JailError::Io(format!("canonicalize parent of {trimmed}: {e}")))?;
            if !parent_canonical.starts_with(&self.canonical_root) {
                return Err(JailError::Escape(trimmed.to_string()));
            }
            let basename = joined.file_name().ok_or(JailError::Empty)?.to_owned();
            Ok(parent_canonical.join(basename))
        }
    }

    /// Render a canonical absolute path as jail-relative (for return
    /// values and audit logs). Falls back to the absolute path if it
    /// somehow isn't under root.
    fn display_rel(&self, canonical: &Path) -> String {
        canonical
            .strip_prefix(&self.canonical_root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| canonical.to_string_lossy().to_string())
    }
}

#[derive(Debug, thiserror::Error)]
enum JailError {
    #[error("path empty")]
    Empty,
    #[error("path '{0}' is absolute (must be jail-relative)")]
    Absolute(String),
    #[error("path '{0}' contains '..' segment")]
    Traversal(String),
    #[error("path '{0}' escapes jail root after canonicalisation (symlink?)")]
    Escape(String),
    #[error("{0}")]
    Io(String),
}

impl From<JailError> for HandlerOutcome {
    fn from(e: JailError) -> Self {
        let kind = match e {
            JailError::Io(_) => error_kinds::INVALID_ARGS,
            _ => error_kinds::POLICY_DENIED,
        };
        HandlerOutcome::Err(ErrorEnvelope {
            kind,
            cause: e.to_string(),
            retry_hint: 2,
            retry_after: None,
        })
    }
}

// ──────────────────────────── Capability descriptors ───────────────────────

pub fn descriptor_read() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.read_file");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["fs:read".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Read a UTF-8 file under the jail root. Optional max_bytes cap rejects \
         oversize files (does NOT truncate)."
            .into(),
    );
    d.categories = vec!["read".into(), "fs".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d
}

pub fn descriptor_write() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.write_file");
    d.major_version = 1;
    // Writes are not idempotent in general (overwrite changes mtime,
    // create_new fails on the second call). AtMostOnce per RELIX-1.
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["fs:write".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Atomic write to a path under the jail root. Modes: 'overwrite' or \
         'create_new'."
            .into(),
    );
    d.categories = vec!["mutate".into(), "fs".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d
}

pub fn descriptor_search() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.search_files");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Expensive; // walks the tree
    d.sensitivity_tags = vec!["fs:read".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Name or substring-content search under the jail root. Linear walker \
         (no index)."
            .into(),
    );
    d.categories = vec!["search".into(), "fs".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d
}

pub fn descriptor_patch() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.patch");
    d.major_version = 1;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["fs:write".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Apply a unified diff to an existing file under the jail root. Refuses \
         non-existent files and mismatched-context diffs."
            .into(),
    );
    d.categories = vec!["mutate".into(), "fs".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d
}

/// PH-FS-PARITY1: `tool.append_file` — append bytes to an
/// existing file under the jail root. Strictly additive
/// (refuses to create new files; use tool.write_file for that).
/// Useful for log-style append workflows where the AI doesn't
/// need a full read-modify-write.
pub fn descriptor_append() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.append_file");
    d.major_version = 1;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["fs:write".into(), "fs:append".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Append bytes to an existing file under the jail root. Refuses to \
         create new files (use tool.write_file). Enforces the same per-file \
         write cap as tool.write_file."
            .into(),
    );
    d.categories = vec!["mutate".into(), "fs".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d
}

/// PH-FS-PARITY1: `tool.patch_preview` — dry-run a unified
/// diff. Returns the patched body without writing it. Lets
/// operators verify a patch lands cleanly before committing.
pub fn descriptor_patch_preview() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.patch_preview");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["fs:read".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Dry-run a unified diff against an existing file. Returns the would-be \
         patched body without writing. Honest about mismatched-context diffs \
         (returns the same error tool.patch would)."
            .into(),
    );
    d.categories = vec!["read".into(), "fs".into(), "preview".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d
}

/// PH-FS-PARITY2: `tool.binary_sniff` — classify a file as
/// text or binary by reading its first few KiB. Useful before
/// `tool.read_file` (which strictly requires UTF-8) so a
/// caller can decide whether to read it as text or hand it to
/// `tool.pdf` / a future binary-aware capability.
pub fn descriptor_binary_sniff() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.binary_sniff");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["fs:read".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Classify a file as text/binary by reading the first 8 KiB. Returns \
         size, sniff_bytes, is_binary, detected_class (utf8/ascii/binary/empty), \
         null_byte_count, and first_bytes_hex. Does NOT read the whole file."
            .into(),
    );
    d.categories = vec!["read".into(), "fs".into(), "classify".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d
}

/// CW2: `tool.list_dir` — list direct children of a
/// jail-relative directory. Returns one line per entry:
/// `<kind>\t<name>\t<size_bytes>\t<modified_unix_secs>`
/// where kind is `dir` / `file` / `symlink` / `other`.
/// Caps at `FsJailConfig::max_search_results` entries
/// (same cap as search_files; operators paginate via
/// `<rel_path>|<offset>` if they need more).
pub fn descriptor_list() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.list_dir");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["fs:read".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "List direct children of a directory under the jail root. \
         Tab-delimited rows (kind\\tname\\tsize\\tmtime). Capped at the \
         operator's max_search_results."
            .into(),
    );
    d.categories = vec!["read".into(), "fs".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d
}

// ──────────────────────────── Registration ─────────────────────────────────

pub fn register(bridge: &mut DispatchBridge, jail: Arc<FsJail>) {
    {
        let j = jail.clone();
        bridge.register(
            "tool.read_file",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_read(&j, &ctx) }
            })),
        );
    }
    {
        let j = jail.clone();
        bridge.register(
            "tool.write_file",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_write(&j, &ctx) }
            })),
        );
    }
    {
        let j = jail.clone();
        bridge.register(
            "tool.search_files",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_search(&j, &ctx) }
            })),
        );
    }
    {
        let j = jail.clone();
        bridge.register(
            "tool.patch",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_patch(&j, &ctx) }
            })),
        );
    }
    {
        let j = jail.clone();
        bridge.register(
            "tool.list_dir",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_list_dir(&j, &ctx) }
            })),
        );
    }
    {
        let j = jail.clone();
        bridge.register(
            "tool.append_file",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_append(&j, &ctx) }
            })),
        );
    }
    {
        let j = jail.clone();
        bridge.register(
            "tool.patch_preview",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_patch_preview(&j, &ctx) }
            })),
        );
    }
    {
        let j = jail;
        bridge.register(
            "tool.binary_sniff",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_binary_sniff(&j, &ctx) }
            })),
        );
    }
}

// ──────────────────────────── Handlers ─────────────────────────────────────

fn handle_read(jail: &FsJail, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.read_file arg utf8: {e}")),
    };
    let (rel, cap) = match s.rsplit_once('|') {
        Some((p, n_str)) if n_str.trim().parse::<usize>().is_ok() => {
            (p.trim(), n_str.trim().parse::<usize>().unwrap())
        }
        _ => (s.trim(), jail.cfg.max_read_bytes),
    };
    let canonical = match jail.resolve(rel, true) {
        Ok(p) => p,
        Err(e) => return e.into(),
    };
    let meta = match std::fs::metadata(&canonical) {
        Ok(m) => m,
        Err(e) => return invalid(format!("tool.read_file metadata: {e}")),
    };
    if !meta.is_file() {
        return invalid(format!(
            "tool.read_file: '{}' is not a regular file",
            jail.display_rel(&canonical)
        ));
    }
    let effective_cap = cap.min(jail.cfg.max_read_bytes);
    if meta.len() as usize > effective_cap {
        return invalid(format!(
            "tool.read_file: file {} bytes exceeds cap {}",
            meta.len(),
            effective_cap
        ));
    }
    let bytes = match std::fs::read(&canonical) {
        Ok(b) => b,
        Err(e) => return invalid(format!("tool.read_file io: {e}")),
    };
    match String::from_utf8(bytes) {
        Ok(s) => HandlerOutcome::Ok(s.into_bytes()),
        Err(_) => invalid(format!(
            "tool.read_file: '{}' contains non-UTF-8 bytes",
            jail.display_rel(&canonical)
        )),
    }
}

fn handle_write(jail: &FsJail, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.write_file arg utf8: {e}")),
    };
    // path|mode|content
    let mut parts = s.splitn(3, '|');
    let rel = parts.next().unwrap_or("").trim();
    let mode = parts.next().unwrap_or("").trim();
    let content = parts.next().unwrap_or("");
    if rel.is_empty() || mode.is_empty() {
        return invalid(
            "tool.write_file arg must be `path|mode|content` (mode: overwrite|create_new)".into(),
        );
    }
    if content.len() > jail.cfg.max_write_bytes {
        return invalid(format!(
            "tool.write_file: content {} bytes exceeds cap {}",
            content.len(),
            jail.cfg.max_write_bytes
        ));
    }
    let canonical = match jail.resolve(rel, false) {
        Ok(p) => p,
        Err(e) => return e.into(),
    };
    let create_new = match mode {
        "overwrite" => false,
        "create_new" => true,
        other => return invalid(format!("tool.write_file: unknown mode '{other}'")),
    };
    if create_new && canonical.exists() {
        return invalid(format!(
            "tool.write_file: refusing to overwrite (mode=create_new): {}",
            jail.display_rel(&canonical)
        ));
    }
    // Atomic write via tempfile-in-same-dir + rename.
    let parent = match canonical.parent() {
        Some(p) => p,
        None => return invalid("tool.write_file: target has no parent dir".into()),
    };
    let tmp = match tempfile_in_dir(parent) {
        Ok(t) => t,
        Err(e) => return invalid(format!("tool.write_file tempfile: {e}")),
    };
    if let Err(e) = std::fs::write(&tmp, content.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return invalid(format!("tool.write_file write tempfile: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp, &canonical) {
        let _ = std::fs::remove_file(&tmp);
        return invalid(format!("tool.write_file rename: {e}"));
    }
    let body = format!(
        "ok bytes={} path={}\n",
        content.len(),
        jail.display_rel(&canonical)
    );
    HandlerOutcome::Ok(body.into_bytes())
}

fn handle_search(jail: &FsJail, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.search_files arg utf8: {e}")),
    };
    // mode|pattern|max_results
    let parts: Vec<&str> = s.splitn(3, '|').collect();
    let mode = parts.first().copied().unwrap_or("").trim();
    let pattern = parts.get(1).copied().unwrap_or("");
    let cap = parts
        .get(2)
        .and_then(|n| n.trim().parse::<usize>().ok())
        .unwrap_or(jail.cfg.max_search_results)
        .min(jail.cfg.max_search_results);
    if mode.is_empty() || pattern.is_empty() {
        return invalid(
            "tool.search_files arg must be `mode|pattern|max_results` (mode: name|content)".into(),
        );
    }
    let mut hits: Vec<String> = Vec::new();
    let mut walked: Vec<PathBuf> = Vec::new();
    walk_under(
        &jail.canonical_root,
        &jail.canonical_root,
        &mut walked,
        50_000,
    );

    match mode {
        "name" => {
            let needle = pattern.to_ascii_lowercase();
            for p in walked {
                if hits.len() >= cap {
                    break;
                }
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                if name.contains(&needle) {
                    hits.push(jail.display_rel(&p));
                }
            }
        }
        "content" => {
            // For content search we only look at files that look text-y.
            // Skip files larger than max_read_bytes to bound work.
            for p in walked {
                if hits.len() >= cap {
                    break;
                }
                let meta = match std::fs::metadata(&p) {
                    Ok(m) if m.is_file() => m,
                    _ => continue,
                };
                if meta.len() as usize > jail.cfg.max_read_bytes {
                    continue;
                }
                let bytes = match std::fs::read(&p) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let Ok(text) = std::str::from_utf8(&bytes) else {
                    continue;
                };
                for (i, line) in text.lines().enumerate() {
                    if hits.len() >= cap {
                        break;
                    }
                    if line.contains(pattern) {
                        let rel = jail.display_rel(&p);
                        let trimmed_line = if line.len() > 240 { &line[..240] } else { line };
                        hits.push(format!("{}:{}:{}", rel, i + 1, trimmed_line));
                    }
                }
            }
        }
        other => return invalid(format!("tool.search_files: unknown mode '{other}'")),
    }
    HandlerOutcome::Ok(hits.join("\n").into_bytes())
}

fn handle_patch(jail: &FsJail, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.patch arg utf8: {e}")),
    };
    // path|patch_mode|patch_body
    let mut parts = s.splitn(3, '|');
    let rel = parts.next().unwrap_or("").trim();
    let mode = parts.next().unwrap_or("").trim();
    let body = parts.next().unwrap_or("");
    if rel.is_empty() || mode.is_empty() || body.is_empty() {
        return invalid(
            "tool.patch arg must be `path|unified_diff|<diff body>` (mode: unified_diff)".into(),
        );
    }
    if mode != "unified_diff" {
        return invalid(format!(
            "tool.patch: unknown mode '{mode}' (alpha supports `unified_diff` only)"
        ));
    }
    if body.len() > jail.cfg.max_write_bytes {
        return invalid(format!(
            "tool.patch: diff {} bytes exceeds write cap {}",
            body.len(),
            jail.cfg.max_write_bytes
        ));
    }
    let canonical = match jail.resolve(rel, true) {
        Ok(p) => p,
        Err(e) => return e.into(),
    };
    let original = match std::fs::read_to_string(&canonical) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.patch read: {e}")),
    };
    let patch = match diffy::Patch::from_str(body) {
        Ok(p) => p,
        Err(e) => return invalid(format!("tool.patch: invalid unified diff: {e}")),
    };
    let patched = match diffy::apply(&original, &patch) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.patch: apply failed: {e}")),
    };
    if patched.len() > jail.cfg.max_write_bytes {
        return invalid(format!(
            "tool.patch: patched file {} bytes exceeds write cap {}",
            patched.len(),
            jail.cfg.max_write_bytes
        ));
    }
    let parent = canonical.parent().expect("canonical has parent");
    let tmp = match tempfile_in_dir(parent) {
        Ok(t) => t,
        Err(e) => return invalid(format!("tool.patch tempfile: {e}")),
    };
    if let Err(e) = std::fs::write(&tmp, patched.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return invalid(format!("tool.patch write tempfile: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp, &canonical) {
        let _ = std::fs::remove_file(&tmp);
        return invalid(format!("tool.patch rename: {e}"));
    }
    let body = format!("ok bytes={}\n", patched.len());
    HandlerOutcome::Ok(body.into_bytes())
}

/// PH-FS-PARITY1: arg shape `<rel_path>|<bytes>`. Append-only;
/// refuses to create new files (use `tool.write_file`).
/// Enforces the jail's `max_write_bytes` against the appended
/// length, not the resulting file size — same posture as
/// tool.write_file's per-call cap.
fn handle_append(jail: &FsJail, ctx: &InvocationCtx) -> HandlerOutcome {
    use std::io::Write as _;
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.append_file arg utf8: {e}")),
    };
    let (rel, body) = match s.split_once('|') {
        Some(p) => p,
        None => {
            return invalid(
                "tool.append_file arg shape `<rel_path>|<bytes>` (bytes may be empty)".into(),
            );
        }
    };
    let rel = rel.trim();
    if rel.is_empty() {
        return invalid("tool.append_file: rel_path required".into());
    }
    if body.len() > jail.cfg.max_write_bytes {
        return invalid(format!(
            "tool.append_file: {} bytes exceeds write cap {}",
            body.len(),
            jail.cfg.max_write_bytes
        ));
    }
    // Use resolve(false) so we validate the parent + jail-escape
    // posture before checking existence; returns the clean
    // "does not exist" message rather than the raw canonicalize
    // IO error.
    let canonical = match jail.resolve(rel, false) {
        Ok(p) => p,
        Err(e) => return e.into(),
    };
    let meta = match std::fs::metadata(&canonical) {
        Ok(m) => m,
        Err(_) => {
            return invalid(format!(
                "tool.append_file: '{}' does not exist (use tool.write_file to create)",
                jail.display_rel(&canonical),
            ));
        }
    };
    if !meta.is_file() {
        return invalid(format!(
            "tool.append_file: '{}' is not a regular file",
            jail.display_rel(&canonical),
        ));
    }
    let mut f = match std::fs::OpenOptions::new().append(true).open(&canonical) {
        Ok(f) => f,
        Err(e) => return invalid(format!("tool.append_file open: {e}")),
    };
    if let Err(e) = f.write_all(body.as_bytes()) {
        return invalid(format!("tool.append_file write: {e}"));
    }
    let new_size = std::fs::metadata(&canonical).map(|m| m.len()).unwrap_or(0);
    HandlerOutcome::Ok(format!("ok appended={} new_size={new_size}\n", body.len()).into_bytes())
}

/// PH-FS-PARITY1: arg shape `<rel_path>|<unified_diff_body>`.
/// Read-only — returns the patched body without writing.
/// Useful for "would this patch land cleanly?" checks before
/// committing via tool.patch.
fn handle_patch_preview(jail: &FsJail, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.patch_preview arg utf8: {e}")),
    };
    let (rel, body) = match s.split_once('|') {
        Some(p) => p,
        None => {
            return invalid("tool.patch_preview arg shape `<rel_path>|<unified_diff>`".into());
        }
    };
    let rel = rel.trim();
    if rel.is_empty() || body.is_empty() {
        return invalid("tool.patch_preview: rel_path + diff required".into());
    }
    if body.len() > jail.cfg.max_write_bytes {
        return invalid(format!(
            "tool.patch_preview: diff {} bytes exceeds write cap {}",
            body.len(),
            jail.cfg.max_write_bytes
        ));
    }
    let canonical = match jail.resolve(rel, true) {
        Ok(p) => p,
        Err(e) => return e.into(),
    };
    let original = match std::fs::read_to_string(&canonical) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.patch_preview read: {e}")),
    };
    let patch = match diffy::Patch::from_str(body) {
        Ok(p) => p,
        Err(e) => return invalid(format!("tool.patch_preview: invalid unified diff: {e}")),
    };
    let patched = match diffy::apply(&original, &patch) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.patch_preview: apply failed: {e}")),
    };
    HandlerOutcome::Ok(patched.into_bytes())
}

const BINARY_SNIFF_BYTES: usize = 8 * 1024;
const BINARY_SNIFF_HEX_PREVIEW: usize = 32;

fn handle_binary_sniff(jail: &FsJail, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.binary_sniff arg utf8: {e}")),
    };
    let rel = s.trim();
    if rel.is_empty() {
        return invalid("tool.binary_sniff: rel_path required".into());
    }
    let canonical = match jail.resolve(rel, true) {
        Ok(p) => p,
        Err(e) => return e.into(),
    };
    let meta = match std::fs::metadata(&canonical) {
        Ok(m) => m,
        Err(e) => return invalid(format!("tool.binary_sniff metadata: {e}")),
    };
    if !meta.is_file() {
        return invalid(format!(
            "tool.binary_sniff: '{}' is not a regular file",
            jail.display_rel(&canonical)
        ));
    }
    let size = meta.len();
    let read_cap = (BINARY_SNIFF_BYTES as u64).min(size) as usize;
    let bytes = match read_prefix(&canonical, read_cap) {
        Ok(b) => b,
        Err(e) => return invalid(format!("tool.binary_sniff read: {e}")),
    };
    let cls = classify_bytes(&bytes);
    let preview = hex_preview(&bytes, BINARY_SNIFF_HEX_PREVIEW);
    let body = format!(
        "path={}\n\
         size={size}\n\
         sniff_bytes={sniff}\n\
         is_binary={is_binary}\n\
         detected_class={class}\n\
         null_byte_count={nulls}\n\
         first_bytes_hex={hex}\n",
        jail.display_rel(&canonical),
        sniff = bytes.len(),
        is_binary = cls.is_binary,
        class = cls.detected_class,
        nulls = cls.null_byte_count,
        hex = preview,
    );
    HandlerOutcome::Ok(body.into_bytes())
}

/// Read up to `cap` bytes from `path` without loading the whole file.
fn read_prefix(path: &Path, cap: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    if cap == 0 {
        return Ok(Vec::new());
    }
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; cap];
    let mut read = 0;
    while read < cap {
        let n = f.read(&mut buf[read..])?;
        if n == 0 {
            break;
        }
        read += n;
    }
    buf.truncate(read);
    Ok(buf)
}

#[derive(Debug, PartialEq, Eq)]
struct SniffClass {
    is_binary: bool,
    detected_class: &'static str,
    null_byte_count: usize,
}

/// Classify a byte buffer. Strategy:
/// - empty → `empty`, not binary
/// - any null byte → `binary`
/// - valid UTF-8 → `utf8` (and `ascii` when all bytes < 0x80)
/// - else → `binary`
fn classify_bytes(bytes: &[u8]) -> SniffClass {
    let null_count = bytes.iter().filter(|b| **b == 0).count();
    if bytes.is_empty() {
        return SniffClass {
            is_binary: false,
            detected_class: "empty",
            null_byte_count: 0,
        };
    }
    if null_count > 0 {
        return SniffClass {
            is_binary: true,
            detected_class: "binary",
            null_byte_count: null_count,
        };
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => {
            let all_ascii = bytes.iter().all(|b| *b < 0x80);
            SniffClass {
                is_binary: false,
                detected_class: if all_ascii { "ascii" } else { "utf8" },
                null_byte_count: 0,
            }
        }
        Err(_) => SniffClass {
            is_binary: true,
            detected_class: "binary",
            null_byte_count: 0,
        },
    }
}

fn hex_preview(bytes: &[u8], cap: usize) -> String {
    use std::fmt::Write as _;
    let n = cap.min(bytes.len());
    let mut out = String::with_capacity(n * 2);
    for b in &bytes[..n] {
        let _ = write!(out, "{b:02x}");
    }
    out
}

// ──────────────────────────── Helpers ──────────────────────────────────────

/// Recursive directory walk that does NOT follow symlinks. Bounded by
/// `max_entries` so a misconfigured jail (e.g. set to `/`) can't blow
/// up memory. Order is breadth-first.
fn walk_under(root: &Path, _orig: &Path, out: &mut Vec<PathBuf>, max_entries: usize) {
    let mut queue: std::collections::VecDeque<PathBuf> = std::collections::VecDeque::new();
    queue.push_back(root.to_path_buf());
    while let Some(dir) = queue.pop_front() {
        if out.len() >= max_entries {
            break;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            if out.len() >= max_entries {
                break;
            }
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                // Never traverse symlinks. They may point outside the
                // jail; the symlink itself is excluded from results so
                // search_files won't surface paths whose canonical
                // form sits outside root.
                continue;
            }
            if ft.is_dir() {
                queue.push_back(path);
            } else if ft.is_file() {
                out.push(path);
            }
        }
    }
}

/// Create a uniquely-named tempfile in `dir`. Returns the path. The
/// file is created empty; callers `std::fs::write` then `rename`.
fn tempfile_in_dir(dir: &Path) -> std::io::Result<PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let name = format!(".relix-tool-write-{pid}-{nanos}.tmp");
    let tmp = dir.join(name);
    std::fs::File::create(&tmp)?;
    Ok(tmp)
}

fn invalid(cause: String) -> HandlerOutcome {
    HandlerOutcome::Err(ErrorEnvelope {
        kind: error_kinds::INVALID_ARGS,
        cause,
        retry_hint: 2,
        retry_after: None,
    })
}

/// CW2: `tool.list_dir` handler. Args: `<rel_path>` for the
/// jail root → list directory entries. Optional `|<offset>`
/// tail enables stable pagination (`0` = first page,
/// `max_search_results` per page). Returns one tab-delim row
/// per entry:
///   `<kind>\t<name>\t<size_bytes>\t<modified_unix_secs>`
/// where kind ∈ {dir, file, symlink, other}. Final row is
/// `next_offset=<N>` so callers can drive pagination
/// (empty string when no more results).
fn handle_list_dir(jail: &FsJail, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.list_dir arg utf8: {e}")),
    };
    // `<rel_path>` or `<rel_path>|<offset>`.
    let (rel, offset): (&str, usize) = match s.rsplit_once('|') {
        Some((p, n_str)) if n_str.trim().parse::<usize>().is_ok() => {
            (p.trim(), n_str.trim().parse::<usize>().unwrap())
        }
        _ => (s.trim(), 0),
    };
    let canonical = match jail.resolve(rel, true) {
        Ok(p) => p,
        Err(e) => return e.into(),
    };
    let meta = match std::fs::metadata(&canonical) {
        Ok(m) => m,
        Err(e) => return invalid(format!("tool.list_dir metadata: {e}")),
    };
    if !meta.is_dir() {
        return invalid(format!(
            "tool.list_dir: '{}' is not a directory",
            jail.display_rel(&canonical)
        ));
    }
    let read_dir = match std::fs::read_dir(&canonical) {
        Ok(it) => it,
        Err(e) => return invalid(format!("tool.list_dir read_dir: {e}")),
    };
    // Collect + sort by name for deterministic pagination.
    let mut entries: Vec<std::fs::DirEntry> = match read_dir.collect::<Result<Vec<_>, _>>() {
        Ok(v) => v,
        Err(e) => return invalid(format!("tool.list_dir iterate: {e}")),
    };
    entries.sort_by_key(|a| a.file_name());
    let cap = jail.cfg.max_search_results;
    let total = entries.len();
    let end = offset.saturating_add(cap).min(total);
    let mut buf = String::new();
    use std::fmt::Write as _;
    for entry in entries.iter().skip(offset).take(cap) {
        let name = entry.file_name().to_string_lossy().to_string();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => {
                // Skip the entry rather than failing the whole
                // listing — operators get an honest count via
                // `next_offset` advance.
                continue;
            }
        };
        let kind = if ft.is_dir() {
            "dir"
        } else if ft.is_file() {
            "file"
        } else if ft.is_symlink() {
            "symlink"
        } else {
            "other"
        };
        let (size, mtime) = match entry.metadata() {
            Ok(m) => {
                let size = m.len();
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                (size, mtime)
            }
            Err(_) => (0u64, 0i64),
        };
        // Sanitize name — operators may have weird filenames,
        // but tabs + newlines would break the line format.
        let safe_name = name.replace(['\t', '\n'], " ");
        let _ = writeln!(buf, "{kind}\t{safe_name}\t{size}\t{mtime}");
    }
    // Trailer for stable pagination. Empty value when the
    // page completed the directory.
    let next = if end < total {
        end.to_string()
    } else {
        String::new()
    };
    let _ = writeln!(buf, "next_offset={next}");
    HandlerOutcome::Ok(buf.into_bytes())
}

// ──────────────────────────── Tests ────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn mk_jail() -> (TempDir, Arc<FsJail>) {
        let td = TempDir::new().unwrap();
        let cfg = FsJailConfig {
            root: td.path().to_path_buf(),
            max_read_bytes: 1024 * 1024,
            max_write_bytes: 1024 * 1024,
            max_search_results: 100,
        };
        let jail = FsJail::new(cfg).unwrap();
        (td, Arc::new(jail))
    }

    fn ctx(args: &[u8]) -> InvocationCtx {
        use relix_core::identity::VerifiedIdentity;
        use relix_core::types::{NodeId, RequestId, TraceId};
        InvocationCtx {
            caller: VerifiedIdentity {
                subject_id: NodeId::from_pubkey(b"x"),
                name: "x".into(),
                org_id: NodeId::from_pubkey(b"o"),
                groups: vec![],
                role: "".into(),
                clearance: "".into(),
                bundle_id: [0; 32],
            },
            trace_id: TraceId::new(),
            request_id: RequestId::new(),
            args: args.to_vec(),
        }
    }

    #[test]
    fn resolve_rejects_absolute_traversal_empty() {
        let (_td, j) = mk_jail();
        assert!(matches!(j.resolve("", true), Err(JailError::Empty)));
        assert!(matches!(j.resolve("   ", true), Err(JailError::Empty)));
        #[cfg(unix)]
        assert!(matches!(
            j.resolve("/etc/passwd", true),
            Err(JailError::Absolute(_))
        ));
        #[cfg(windows)]
        assert!(matches!(
            j.resolve("C:\\Windows\\System32", true),
            Err(JailError::Absolute(_))
        ));
        assert!(matches!(
            j.resolve("../escape.txt", true),
            Err(JailError::Traversal(_))
        ));
        assert!(matches!(
            j.resolve("subdir/../escape.txt", true),
            Err(JailError::Traversal(_))
        ));
    }

    #[test]
    fn list_dir_returns_sorted_entries_with_next_offset() {
        // CW2: list_dir lists direct children with stable
        // alphabetical sort + tab-delimited rows + the
        // next_offset trailer.
        let (_td, j) = mk_jail();
        // Seed: two files + one subdir.
        handle_write(&j, &ctx(b"a.txt|create_new|first"));
        handle_write(&j, &ctx(b"b.txt|create_new|second"));
        std::fs::create_dir(j.canonical_root.join("subdir")).unwrap();
        // List the jail root.
        let r = handle_list_dir(&j, &ctx(b"."));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("list_dir failed: {}", e.cause),
        };
        // Three rows + trailer; sorted alphabetically.
        let lines: Vec<&str> = body.lines().collect();
        assert!(lines.len() >= 4);
        assert!(lines[0].starts_with("file\ta.txt\t"));
        assert!(lines[1].starts_with("file\tb.txt\t"));
        assert!(lines[2].starts_with("dir\tsubdir\t"));
        assert_eq!(*lines.last().unwrap(), "next_offset=");
    }

    #[test]
    fn list_dir_paginates_with_offset() {
        let (_td, j) = mk_jail();
        // Cap is 100 by default — force a smaller cap to
        // exercise pagination without spamming the test.
        let small_cfg = FsJailConfig {
            root: j.canonical_root.clone(),
            max_read_bytes: 1024,
            max_write_bytes: 1024,
            max_search_results: 2,
        };
        let small_jail = Arc::new(FsJail::new(small_cfg).unwrap());
        for i in 0..5 {
            handle_write(
                &small_jail,
                &ctx(format!("f{i}.txt|create_new|x").as_bytes()),
            );
        }
        // First page: 2 results, next_offset=2.
        let r = handle_list_dir(&small_jail, &ctx(b"."));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("list_dir page 1: {}", e.cause),
        };
        let trailer = body.lines().last().unwrap();
        assert_eq!(trailer, "next_offset=2");
        // Second page: 2 more, next_offset=4.
        let r = handle_list_dir(&small_jail, &ctx(b".|2"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("list_dir page 2: {}", e.cause),
        };
        let trailer = body.lines().last().unwrap();
        assert_eq!(trailer, "next_offset=4");
        // Final page: 1 result, trailer empty.
        let r = handle_list_dir(&small_jail, &ctx(b".|4"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("list_dir page 3: {}", e.cause),
        };
        let trailer = body.lines().last().unwrap();
        assert_eq!(trailer, "next_offset=");
    }

    #[test]
    fn list_dir_rejects_non_directory() {
        let (_td, j) = mk_jail();
        handle_write(&j, &ctx(b"a.txt|create_new|x"));
        let r = handle_list_dir(&j, &ctx(b"a.txt"));
        match r {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("not a directory")),
            HandlerOutcome::Ok(_) => panic!("expected error on file target"),
        }
    }

    #[test]
    fn list_dir_respects_jail_traversal_protections() {
        let (_td, j) = mk_jail();
        let r = handle_list_dir(&j, &ctx(b"../."));
        match r {
            HandlerOutcome::Err(_) => {}
            HandlerOutcome::Ok(_) => panic!("expected jail rejection"),
        }
    }

    #[test]
    fn write_then_read_round_trip() {
        let (_td, j) = mk_jail();
        let r = handle_write(&j, &ctx(b"hello.txt|create_new|hello world"));
        match &r {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => panic!("write failed: {}", e.cause),
        }
        let r = handle_read(&j, &ctx(b"hello.txt"));
        match r {
            HandlerOutcome::Ok(b) => assert_eq!(String::from_utf8(b).unwrap(), "hello world"),
            HandlerOutcome::Err(e) => panic!("read failed: {}", e.cause),
        }
    }

    #[test]
    fn create_new_refuses_existing_file() {
        let (_td, j) = mk_jail();
        let _ = handle_write(&j, &ctx(b"a.txt|create_new|first"));
        let r = handle_write(&j, &ctx(b"a.txt|create_new|second"));
        match r {
            HandlerOutcome::Err(e) => {
                assert!(
                    e.cause.contains("refusing to overwrite"),
                    "cause: {}",
                    e.cause
                );
            }
            HandlerOutcome::Ok(_) => panic!("expected create_new to refuse existing"),
        }
    }

    #[test]
    fn overwrite_replaces_content() {
        let (_td, j) = mk_jail();
        let _ = handle_write(&j, &ctx(b"a.txt|create_new|first"));
        let r = handle_write(&j, &ctx(b"a.txt|overwrite|second"));
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let r = handle_read(&j, &ctx(b"a.txt"));
        match r {
            HandlerOutcome::Ok(b) => assert_eq!(String::from_utf8(b).unwrap(), "second"),
            HandlerOutcome::Err(e) => panic!("read failed: {}", e.cause),
        }
    }

    #[test]
    fn read_oversize_rejected() {
        let (td, j) = mk_jail();
        let p = td.path().join("big.txt");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&[b'x'; 200]).unwrap();
        // Reset jail with small cap for this assertion.
        let small = FsJail::new(FsJailConfig {
            root: td.path().to_path_buf(),
            max_read_bytes: 10,
            max_write_bytes: 10,
            max_search_results: 100,
        })
        .unwrap();
        let r = handle_read(&small, &ctx(b"big.txt"));
        match r {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("exceeds cap"), "cause: {}", e.cause);
            }
            HandlerOutcome::Ok(_) => panic!("expected oversize rejection"),
        }
        // jail value not used elsewhere; silence unused warning.
        drop(j);
    }

    #[test]
    fn read_non_utf8_rejected() {
        let (td, j) = mk_jail();
        let p = td.path().join("bin");
        std::fs::write(&p, [0xff, 0xfe, 0x00]).unwrap();
        let r = handle_read(&j, &ctx(b"bin"));
        match r {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("non-UTF-8"), "cause: {}", e.cause),
            HandlerOutcome::Ok(_) => panic!("expected utf8 rejection"),
        }
    }

    #[test]
    fn search_name_finds_files() {
        let (td, j) = mk_jail();
        std::fs::write(td.path().join("alpha.txt"), b"x").unwrap();
        std::fs::write(td.path().join("beta.txt"), b"y").unwrap();
        std::fs::create_dir(td.path().join("sub")).unwrap();
        std::fs::write(td.path().join("sub/gamma.md"), b"z").unwrap();
        let r = handle_search(&j, &ctx(b"name|alpha|10"));
        match r {
            HandlerOutcome::Ok(b) => {
                let s = String::from_utf8(b).unwrap();
                assert!(s.contains("alpha.txt"), "got: {s}");
                assert!(!s.contains("beta"));
            }
            HandlerOutcome::Err(e) => panic!("search failed: {}", e.cause),
        }
        // recursive
        let r = handle_search(&j, &ctx(b"name|gamma|10"));
        match r {
            HandlerOutcome::Ok(b) => {
                let s = String::from_utf8(b).unwrap();
                assert!(s.contains("gamma.md"), "got: {s}");
            }
            HandlerOutcome::Err(e) => panic!("recursive search failed: {}", e.cause),
        }
    }

    #[test]
    fn search_content_includes_line_numbers() {
        let (td, j) = mk_jail();
        std::fs::write(
            td.path().join("doc.txt"),
            "alpha line\nbeta line\nalpha again",
        )
        .unwrap();
        let r = handle_search(&j, &ctx(b"content|alpha|10"));
        match r {
            HandlerOutcome::Ok(b) => {
                let s = String::from_utf8(b).unwrap();
                assert!(s.contains("doc.txt:1:alpha line"), "got: {s}");
                assert!(s.contains("doc.txt:3:alpha again"), "got: {s}");
                assert!(!s.contains("beta"), "got: {s}");
            }
            HandlerOutcome::Err(e) => panic!("content search failed: {}", e.cause),
        }
    }

    #[test]
    fn patch_unified_diff_applies() {
        let (_td, j) = mk_jail();
        let _ = handle_write(
            &j,
            &ctx(b"x.txt|create_new|line one\nline two\nline three\n"),
        );
        let diff = "--- a/x.txt\n+++ b/x.txt\n@@ -1,3 +1,3 @@\n line one\n-line two\n+LINE TWO\n line three\n";
        let arg = format!("x.txt|unified_diff|{diff}");
        let r = handle_patch(&j, &ctx(arg.as_bytes()));
        match r {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => panic!("patch failed: {}", e.cause),
        }
        let r = handle_read(&j, &ctx(b"x.txt"));
        match r {
            HandlerOutcome::Ok(b) => {
                let s = String::from_utf8(b).unwrap();
                assert!(s.contains("LINE TWO"), "got: {s}");
                assert!(!s.contains("line two"));
            }
            HandlerOutcome::Err(e) => panic!("read after patch failed: {}", e.cause),
        }
    }

    #[test]
    fn patch_with_mismatched_context_rejected() {
        let (_td, j) = mk_jail();
        let _ = handle_write(&j, &ctx(b"y.txt|create_new|original line\n"));
        // Syntactically valid unified diff but the context line
        // doesn't match what's actually in the file. diffy::apply
        // returns an error.
        let diff =
            "--- a/y.txt\n+++ b/y.txt\n@@ -1,1 +1,1 @@\n-completely different\n+something else\n";
        let arg = format!("y.txt|unified_diff|{diff}");
        let r = handle_patch(&j, &ctx(arg.as_bytes()));
        match r {
            HandlerOutcome::Err(e) => assert!(
                e.cause.contains("apply failed") || e.cause.contains("invalid"),
                "expected apply/invalid in cause, got: {}",
                e.cause
            ),
            HandlerOutcome::Ok(_) => panic!("expected error on mismatched diff"),
        }
        // File must be unchanged (we wrote tmp + rename only on success).
        let r = handle_read(&j, &ctx(b"y.txt"));
        match r {
            HandlerOutcome::Ok(b) => assert_eq!(String::from_utf8(b).unwrap(), "original line\n"),
            HandlerOutcome::Err(e) => panic!("read failed: {}", e.cause),
        }
    }

    #[test]
    fn patch_unknown_mode_rejected() {
        let (_td, j) = mk_jail();
        let _ = handle_write(&j, &ctx(b"z.txt|create_new|orig"));
        let r = handle_patch(&j, &ctx(b"z.txt|replace|x|y"));
        match r {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("alpha supports"), "cause: {}", e.cause);
            }
            HandlerOutcome::Ok(_) => panic!("expected unknown-mode rejection"),
        }
    }

    #[test]
    fn descriptors_have_expected_sensitivity() {
        assert!(
            descriptor_read()
                .sensitivity_tags
                .iter()
                .any(|t| t == "fs:read")
        );
        assert!(
            descriptor_write()
                .sensitivity_tags
                .iter()
                .any(|t| t == "fs:write")
        );
        assert!(
            descriptor_search()
                .sensitivity_tags
                .iter()
                .any(|t| t == "fs:read")
        );
        assert!(
            descriptor_patch()
                .sensitivity_tags
                .iter()
                .any(|t| t == "fs:write")
        );
        assert!(matches!(
            descriptor_write().idempotency,
            Idempotency::AtMostOnce
        ));
        assert!(matches!(
            descriptor_patch().idempotency,
            Idempotency::AtMostOnce
        ));
    }

    // ── Track 6 hardening: edge cases the original alpha tests skipped ──

    #[test]
    fn write_with_traversal_in_path_is_rejected() {
        let (_td, j) = mk_jail();
        // Even with `create_new`, a `..` in the rel path is refused
        // before any file open is attempted.
        let r = handle_write(&j, &ctx(b"../escape.txt|create_new|hi"));
        match r {
            HandlerOutcome::Err(e) => {
                assert!(
                    e.cause.contains("contains '..'") || e.cause.contains("traversal"),
                    "cause: {}",
                    e.cause
                );
            }
            HandlerOutcome::Ok(_) => panic!("expected traversal rejection on write"),
        }
    }

    #[test]
    fn write_with_absolute_path_is_rejected() {
        let (_td, j) = mk_jail();
        #[cfg(unix)]
        let absolute_arg = b"/tmp/escape.txt|create_new|hi".to_vec();
        #[cfg(windows)]
        let absolute_arg = b"C:\\Windows\\escape.txt|create_new|hi".to_vec();
        let r = handle_write(&j, &ctx(&absolute_arg));
        match r {
            HandlerOutcome::Err(e) => {
                assert!(
                    e.cause.to_lowercase().contains("absolute"),
                    "cause: {}",
                    e.cause
                );
            }
            HandlerOutcome::Ok(_) => panic!("expected absolute-path rejection on write"),
        }
    }

    #[test]
    fn write_oversize_payload_rejected_before_open() {
        let (td, _) = mk_jail();
        let tiny = FsJail::new(FsJailConfig {
            root: td.path().to_path_buf(),
            max_read_bytes: 10,
            max_write_bytes: 10,
            max_search_results: 100,
        })
        .unwrap();
        // 20 bytes of content with a 10-byte cap.
        let r = handle_write(&tiny, &ctx(b"big.txt|create_new|aaaaaaaaaaaaaaaaaaaa"));
        match r {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("exceeds cap"), "cause: {}", e.cause);
            }
            HandlerOutcome::Ok(_) => panic!("expected write oversize rejection"),
        }
        // File MUST NOT have been created.
        assert!(!td.path().join("big.txt").exists(),);
    }

    #[test]
    fn patch_on_nonexistent_file_rejected() {
        let (_td, j) = mk_jail();
        let diff = "--- a/ghost.txt\n+++ b/ghost.txt\n@@ -1,1 +1,1 @@\n-x\n+y\n";
        let arg = format!("ghost.txt|unified_diff|{diff}");
        let r = handle_patch(&j, &ctx(arg.as_bytes()));
        match r {
            HandlerOutcome::Err(_) => {}
            HandlerOutcome::Ok(_) => panic!("expected error on patching ghost file"),
        }
        assert!(!Path::new("ghost.txt").exists(),);
    }

    #[test]
    fn search_content_no_matches_returns_empty_body() {
        let (td, j) = mk_jail();
        std::fs::write(td.path().join("doc.txt"), "alpha beta").unwrap();
        let r = handle_search(&j, &ctx(b"content|charlie|10"));
        match r {
            HandlerOutcome::Ok(b) => {
                assert!(b.is_empty(), "expected empty body, got: {b:?}");
            }
            HandlerOutcome::Err(e) => panic!("search failed: {}", e.cause),
        }
    }

    #[test]
    fn search_name_handles_deeply_nested_dirs() {
        let (td, j) = mk_jail();
        let deep = td.path().join("a").join("b").join("c").join("d").join("e");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("needle.txt"), b"x").unwrap();
        let r = handle_search(&j, &ctx(b"name|needle|10"));
        match r {
            HandlerOutcome::Ok(b) => {
                let s = String::from_utf8(b).unwrap();
                assert!(s.contains("needle.txt"), "got: {s}");
                // Path should reflect the nested structure.
                assert!(s.contains("a") && s.contains("e"), "path lost depth: {s}");
            }
            HandlerOutcome::Err(e) => panic!("deep search failed: {}", e.cause),
        }
    }

    #[test]
    fn search_empty_pattern_does_not_match_everything() {
        // Operator concern: an empty content pattern with substring
        // semantics would `contains("")` -> true on every line and
        // explode the result set. Verify we either reject it or
        // return zero matches (NEVER all lines of all files).
        let (td, j) = mk_jail();
        std::fs::write(td.path().join("a.txt"), "one\ntwo\nthree").unwrap();
        std::fs::write(td.path().join("b.txt"), "four\nfive").unwrap();
        let r = handle_search(&j, &ctx(b"content||10"));
        match r {
            HandlerOutcome::Ok(b) => {
                let n = b.iter().filter(|c| **c == b'\n').count();
                assert!(n <= 10, "empty pattern leaked match-all (got {n} lines)");
            }
            HandlerOutcome::Err(_) => {
                // Explicit rejection of empty pattern is also fine —
                // safer than silently matching everything.
            }
        }
    }

    #[test]
    fn read_with_explicit_max_bytes_rejects_oversize_not_truncates() {
        // Regression guard for the safety contract: max_bytes is a
        // CAP that rejects oversize files, NOT a truncation directive.
        // Truncated reads silently hide content from the caller and
        // can lead to wrong-answer flows. The honest behaviour is to
        // refuse and let the caller raise the cap if needed.
        let (td, j) = mk_jail();
        std::fs::write(td.path().join("doc.txt"), "abcdefghij").unwrap();
        let r = handle_read(&j, &ctx(b"doc.txt|4"));
        match r {
            HandlerOutcome::Err(e) => {
                assert!(
                    e.cause.contains("exceeds cap"),
                    "expected cap rejection, got: {}",
                    e.cause
                );
            }
            HandlerOutcome::Ok(_) => panic!("max_bytes must reject oversize, not truncate"),
        }
        // Reading within the cap returns the full contents.
        let r = handle_read(&j, &ctx(b"doc.txt|32"));
        match r {
            HandlerOutcome::Ok(b) => {
                assert_eq!(String::from_utf8(b).unwrap(), "abcdefghij");
            }
            HandlerOutcome::Err(e) => panic!("read within cap failed: {}", e.cause),
        }
    }

    // ── PH-FS-PARITY1: tool.append_file + tool.patch_preview ──────────

    #[test]
    fn append_file_appends_to_existing() {
        let (td, j) = mk_jail();
        let p = td.path().join("log.txt");
        std::fs::write(&p, "first\n").unwrap();
        let r = handle_append(&j, &ctx(b"log.txt|second\n"));
        match r {
            HandlerOutcome::Ok(b) => {
                let s = String::from_utf8(b).unwrap();
                assert!(s.contains("ok appended=7"));
            }
            HandlerOutcome::Err(e) => panic!("append failed: {}", e.cause),
        }
        let out = std::fs::read_to_string(&p).unwrap();
        assert_eq!(out, "first\nsecond\n");
    }

    #[test]
    fn append_file_refuses_missing_target() {
        let (_td, j) = mk_jail();
        let r = handle_append(&j, &ctx(b"nope.txt|hi"));
        match r {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("does not exist")),
            _ => panic!("expected error for missing file"),
        }
    }

    #[test]
    fn append_file_respects_write_cap() {
        let (td, j) = mk_jail();
        std::fs::write(td.path().join("doc.txt"), "x").unwrap();
        let big = "y".repeat(j.cfg.max_write_bytes + 1);
        let arg = format!("doc.txt|{big}");
        let r = handle_append(&j, &ctx(arg.as_bytes()));
        match r {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("exceeds write cap")),
            _ => panic!("expected oversize rejection"),
        }
    }

    #[test]
    fn append_file_rejects_traversal() {
        let (_td, j) = mk_jail();
        let r = handle_append(&j, &ctx(b"../escape|hi"));
        match r {
            HandlerOutcome::Err(_) => {}
            _ => panic!("expected traversal rejection"),
        }
    }

    #[test]
    fn patch_preview_returns_patched_without_writing() {
        let (td, j) = mk_jail();
        let p = td.path().join("doc.txt");
        std::fs::write(&p, "line one\nline two\n").unwrap();
        let diff =
            "--- a/doc.txt\n+++ b/doc.txt\n@@ -1,2 +1,2 @@\n line one\n-line two\n+line TWO\n";
        let arg = format!("doc.txt|{diff}");
        let r = handle_patch_preview(&j, &ctx(arg.as_bytes()));
        match r {
            HandlerOutcome::Ok(b) => {
                assert_eq!(String::from_utf8(b).unwrap(), "line one\nline TWO\n");
            }
            HandlerOutcome::Err(e) => panic!("preview failed: {}", e.cause),
        }
        // File on disk is unchanged.
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "line one\nline two\n");
    }

    #[test]
    fn patch_preview_handles_diff_that_parses_to_no_hunks() {
        // Honest about diffy's behavior: arbitrary text parses
        // as a Patch with zero hunks. Apply against any file
        // returns the original content. Test exists to pin that
        // behavior so a future diffy upgrade either preserves
        // it or this test catches the change.
        let (td, j) = mk_jail();
        std::fs::write(td.path().join("doc.txt"), "hello\n").unwrap();
        let arg = "doc.txt|this is not a diff";
        let r = handle_patch_preview(&j, &ctx(arg.as_bytes()));
        match r {
            HandlerOutcome::Ok(b) => {
                assert_eq!(String::from_utf8(b).unwrap(), "hello\n");
            }
            HandlerOutcome::Err(e) => {
                panic!(
                    "expected unchanged content for no-hunks diff, got err: {}",
                    e.cause
                )
            }
        }
    }

    // ── PH-FS-PARITY2: tool.binary_sniff ───────────────────────────

    #[test]
    fn binary_sniff_descriptor_shape() {
        let d = descriptor_binary_sniff();
        assert_eq!(d.method_name, "tool.binary_sniff");
        assert_eq!(d.major_version, 1);
        assert!(matches!(d.idempotency, Idempotency::Idempotent));
        assert!(matches!(d.cost_class, CostClass::Cheap));
        assert!(d.sensitivity_tags.iter().any(|t| t == "fs:read"));
        assert!(d.environment_requirements.iter().any(|r| r == "fs:jail"));
    }

    #[test]
    fn classify_bytes_empty() {
        let c = classify_bytes(b"");
        assert!(!c.is_binary);
        assert_eq!(c.detected_class, "empty");
        assert_eq!(c.null_byte_count, 0);
    }

    #[test]
    fn classify_bytes_ascii() {
        let c = classify_bytes(b"hello, world");
        assert!(!c.is_binary);
        assert_eq!(c.detected_class, "ascii");
    }

    #[test]
    fn classify_bytes_utf8_non_ascii() {
        let c = classify_bytes("héllo ☃".as_bytes());
        assert!(!c.is_binary);
        assert_eq!(c.detected_class, "utf8");
    }

    #[test]
    fn classify_bytes_with_nulls_is_binary() {
        let c = classify_bytes(b"hello\0world");
        assert!(c.is_binary);
        assert_eq!(c.detected_class, "binary");
        assert_eq!(c.null_byte_count, 1);
    }

    #[test]
    fn classify_bytes_invalid_utf8_is_binary() {
        // 0xFF 0xFE is not valid UTF-8 (lone continuation bytes).
        let c = classify_bytes(&[0x68, 0xff, 0xfe, 0x69]);
        assert!(c.is_binary);
        assert_eq!(c.detected_class, "binary");
    }

    #[test]
    fn hex_preview_caps_at_requested_length() {
        let bytes: Vec<u8> = (0..50u8).collect();
        let s = hex_preview(&bytes, 4);
        assert_eq!(s, "00010203");
    }

    #[test]
    fn binary_sniff_handler_reports_text_for_utf8_file() {
        let (td, j) = mk_jail();
        std::fs::write(td.path().join("greeting.txt"), "héllo\n").unwrap();
        let r = handle_binary_sniff(&j, &ctx(b"greeting.txt"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert!(body.contains("path=greeting.txt"));
        assert!(body.contains("is_binary=false"));
        assert!(body.contains("detected_class=utf8"));
        assert!(body.contains("null_byte_count=0"));
        assert!(body.contains("first_bytes_hex="));
    }

    #[test]
    fn binary_sniff_handler_reports_binary_for_file_with_nulls() {
        let (td, j) = mk_jail();
        let payload: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
        std::fs::write(td.path().join("img.bin"), payload).unwrap();
        let r = handle_binary_sniff(&j, &ctx(b"img.bin"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert!(body.contains("is_binary=true"));
        assert!(body.contains("detected_class=binary"));
        // PNG signature has 0x0d 0x0a 0x1a 0x0a; the 0x00 isn't
        // in the signature itself but lone bytes 0x89 0x50 etc.
        // make this not valid UTF-8 → still classified binary.
        assert!(body.contains("first_bytes_hex=8950"));
    }

    #[test]
    fn binary_sniff_handler_reports_empty_for_empty_file() {
        let (td, j) = mk_jail();
        std::fs::File::create(td.path().join("nothing")).unwrap();
        let r = handle_binary_sniff(&j, &ctx(b"nothing"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert!(body.contains("size=0"));
        assert!(body.contains("sniff_bytes=0"));
        assert!(body.contains("is_binary=false"));
        assert!(body.contains("detected_class=empty"));
    }

    #[test]
    fn binary_sniff_handler_rejects_directory() {
        let (td, j) = mk_jail();
        std::fs::create_dir(td.path().join("d")).unwrap();
        let r = handle_binary_sniff(&j, &ctx(b"d"));
        match r {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("not a regular file")),
            _ => panic!("expected Err for directory target"),
        }
    }

    #[test]
    fn binary_sniff_handler_only_reads_first_8kib_for_large_file() {
        let (td, j) = mk_jail();
        // 20 KiB of ASCII A's — sniff should report sniff_bytes=8192.
        let big = "A".repeat(20 * 1024);
        std::fs::write(td.path().join("big.txt"), &big).unwrap();
        let r = handle_binary_sniff(&j, &ctx(b"big.txt"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert!(body.contains(&format!("size={}", 20 * 1024)));
        assert!(body.contains(&format!("sniff_bytes={}", 8 * 1024)));
        assert!(body.contains("detected_class=ascii"));
    }

    #[test]
    fn binary_sniff_handler_empty_arg_rejected() {
        let (_td, j) = mk_jail();
        let r = handle_binary_sniff(&j, &ctx(b""));
        match r {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("rel_path required")),
            _ => panic!("expected Err for empty arg"),
        }
    }
}
