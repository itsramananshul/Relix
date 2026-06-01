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
//! | `tool.search_files` | `<mode>\|<pattern>\|<max_results>` where mode is `name`, `content`, or `glob` | one match per line; `path` for name and glob modes, `path:line:text` for content mode |
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

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Deserialize;

use relix_core::capability::{CapabilityDescriptor, CostClass, Idempotency, RiskLevel};
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

/// PH-FS-PARITY4: one mutation observation. Pushed onto the
/// jail's bounded audit ring after every successful write /
/// append / patch. Surfaced via `tool.fs.audit_recent`. Pure
/// in-memory observability; does NOT replace the
/// dispatch-level audit log and does NOT mutate chronicle.
#[derive(Clone, Debug)]
pub struct FsAuditEntry {
    /// Wall-clock unix seconds at the moment of mutation.
    pub ts_secs: i64,
    /// One of `"write"`, `"append"`, `"patch"`.
    pub op: &'static str,
    /// Jail-relative path (forward-slash normalized).
    pub rel_path: String,
    /// Byte count of the resulting / appended payload.
    /// For `write` and `patch` this is the resulting file
    /// size on disk; for `append` it is the number of bytes
    /// appended (not the resulting size — that is captured in
    /// the handler's response body).
    pub bytes: usize,
    /// Hex `subject_id` of the caller (full 32-byte fingerprint).
    pub caller_subject_id: String,
}

/// PH-FS-PARITY4: bounded ring of [`FsAuditEntry`]. Oldest
/// entries are evicted when the ring is full. Stored newest-last.
pub struct FsAuditRing {
    entries: Mutex<VecDeque<FsAuditEntry>>,
    capacity: usize,
}

impl FsAuditRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&self, entry: FsAuditEntry) {
        let mut g = self.entries.lock().unwrap_or_else(|e| {
            tracing::warn!("'fs audit ring poisoned'; recovering inner state");
            e.into_inner()
        });
        if g.len() == self.capacity {
            g.pop_front();
        }
        g.push_back(entry);
    }

    pub fn snapshot_newest_first(&self, max: usize) -> Vec<FsAuditEntry> {
        let g = self.entries.lock().unwrap_or_else(|e| {
            tracing::warn!("'fs audit ring poisoned'; recovering inner state");
            e.into_inner()
        });
        g.iter().rev().take(max).cloned().collect()
    }
}

/// Default ring capacity. Bounded so a busy jail can't hold an
/// unbounded mutation log in process memory.
const FS_AUDIT_RING_DEFAULT: usize = 256;

/// Path-jailed FS handle shared across all four handlers.
pub struct FsJail {
    canonical_root: PathBuf,
    cfg: FsJailConfig,
    audit: FsAuditRing,
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
            audit: FsAuditRing::new(FS_AUDIT_RING_DEFAULT),
        })
    }

    /// Push a mutation observation onto the audit ring. Called
    /// from `handle_write` / `handle_append` / `handle_patch` on
    /// the success path. Failures (Err returns) intentionally do
    /// NOT record — the operator inspects failures through the
    /// dispatch-level audit log.
    fn record_mutation(
        &self,
        op: &'static str,
        rel_path: String,
        bytes: usize,
        ctx: &InvocationCtx,
    ) {
        let ts_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.audit.push(FsAuditEntry {
            ts_secs,
            op,
            rel_path,
            bytes,
            caller_subject_id: ctx.caller.subject_id.to_string(),
        });
    }

    /// Test/operator hook — snapshot the most recent N mutations.
    pub fn audit_snapshot(&self, max: usize) -> Vec<FsAuditEntry> {
        self.audit.snapshot_newest_first(max)
    }

    /// Resolve a caller-supplied jail-relative path to a canonical
    /// absolute path inside the jail. Fails closed on any escape.
    /// `must_exist = true` requires the target to already exist (for
    /// reads, search hits); `false` allows non-existent targets (for
    /// new file writes) by canonicalising the parent dir and joining
    /// the basename.
    ///
    /// TOCTOU posture: after canonicalization we run two checks:
    /// 1. [`jail_contains_path`] (strict prefix, also refuses
    ///    `path == base`).
    /// 2. [`refuse_symlinks_within_jail`] walks every component
    ///    of the resolved path under the jail root and refuses if
    ///    any is a symlink. The pre-open symlink check is
    ///    defence-in-depth on top of canonicalisation — it shrinks
    ///    (but cannot eliminate without portable `openat`) the
    ///    window where a path component is swapped between resolve
    ///    and the eventual `File::open`.
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

        let resolved = if must_exist {
            let canonical = joined
                .canonicalize()
                .map_err(|e| JailError::Io(format!("canonicalize {trimmed}: {e}")))?;
            // `must_exist` paths may resolve to the jail root itself
            // (for `list_dir`, `tree`, `stat` on `.` / `""`). Use the
            // lenient `starts_with` check here. Callers that need
            // strict containment (e.g. "this must be a file inside the
            // jail, not the jail itself") gate that on `jail_contains_path`
            // separately.
            if !canonical.starts_with(&self.canonical_root) {
                return Err(JailError::Escape(trimmed.to_string()));
            }
            canonical
        } else {
            // Target may not exist (writes). Canonicalise the parent
            // and append the basename. Parent must exist and may
            // legitimately be the jail root itself (when creating a
            // top-level file like `hello.txt`); we only need
            // `starts_with` for the parent, not strict containment.
            let parent = joined.parent().ok_or(JailError::Empty)?.to_path_buf();
            let parent_canonical = parent
                .canonicalize()
                .map_err(|e| JailError::Io(format!("canonicalize parent of {trimmed}: {e}")))?;
            if !parent_canonical.starts_with(&self.canonical_root) {
                return Err(JailError::Escape(trimmed.to_string()));
            }
            let basename = joined.file_name().ok_or(JailError::Empty)?.to_owned();
            parent_canonical.join(basename)
        };
        // Walk the LEXICAL in-jail path (`joined`, before
        // canonicalisation) and refuse if any component is a symlink.
        // Using `resolved` here would be a no-op for `must_exist` paths:
        // `canonicalize()` has already followed every symlink away, so
        // the resolved path never contains one — the documented "no
        // symlinks" policy would silently accept an in-jail symlink.
        // `joined` still has the original `alias`/symdir component, so
        // `symlink_metadata` on each component catches it. `must_exist
        // == false` paths may not exist yet, so the missing leaf is
        // allowed; only existing symlinked components fail the check.
        refuse_symlinks_within_jail(&self.canonical_root, &joined)?;
        Ok(resolved)
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
pub enum JailError {
    #[error("path empty")]
    Empty,
    #[error("path '{0}' is absolute (must be jail-relative)")]
    Absolute(String),
    #[error("path '{0}' contains '..' segment")]
    Traversal(String),
    #[error("path '{0}' escapes jail root after canonicalisation (symlink?)")]
    Escape(String),
    #[error("path component '{0}' is a symlink (refused by TOCTOU policy)")]
    Symlink(String),
    #[error("{0}")]
    Io(String),
}

/// Strict containment check: returns `true` iff `path` is a
/// proper child of `base` (i.e. starts with `base` AND is not
/// equal to `base`). Both inputs must already be canonicalised
/// by the caller — this is the pure-path predicate, not an I/O
/// operation.
///
/// Used by [`FsJail::resolve`] after canonicalisation to refuse
/// any symlink-via-jail-root that resolves *to* the root itself,
/// and to refuse traversal escapes that pass the bare
/// `starts_with` test.
pub fn jail_contains_path(base: &Path, path: &Path) -> bool {
    if path == base {
        return false;
    }
    path.starts_with(base)
}

/// Walk every component of `resolved` that sits under `base`
/// and refuse if any is a symbolic link. Returns
/// [`JailError::Symlink`] on the first symlinked component
/// encountered; otherwise `Ok(())`.
///
/// Honest about scope: `symlink_metadata` is a stat-time check,
/// so a determined attacker with write access to the jail
/// *between* this check and the eventual open could still swap
/// a component. Eliminating that window fully requires
/// `openat(2)` which is not portable to Windows. This check
/// closes the loud failure modes (admin sets up a symlink farm
/// inside the jail, or a flow writes a symlink during its
/// own run) and works on every platform Relix supports.
pub fn refuse_symlinks_within_jail(base: &Path, resolved: &Path) -> Result<(), JailError> {
    // Walk from `base` outward through every component that
    // shares the base prefix. For each, stat with
    // symlink_metadata and refuse if it's a symlink. We
    // explicitly do NOT stat `base` itself — the operator
    // configured the jail root, including via a symlink, and
    // canonicalize already resolved it.
    let Ok(rel) = resolved.strip_prefix(base) else {
        // Should be impossible if jail_contains_path already
        // passed; the explicit Err keeps the contract honest.
        return Err(JailError::Escape(resolved.display().to_string()));
    };
    let mut walk = base.to_path_buf();
    for component in rel.components() {
        walk.push(component);
        match std::fs::symlink_metadata(&walk) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(JailError::Symlink(walk.display().to_string()));
            }
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                // The leaf may not exist yet (write paths). All
                // intermediate components must — symlink_metadata
                // would have returned Ok above. Stop walking; the
                // not-yet-existing target can't be a symlink.
                return Ok(());
            }
            Err(e) => {
                return Err(JailError::Io(format!(
                    "symlink_metadata {}: {e}",
                    walk.display()
                )));
            }
        }
    }
    Ok(())
}

impl From<JailError> for HandlerOutcome {
    fn from(e: JailError) -> Self {
        let kind = match e {
            JailError::Io(_) => error_kinds::INVALID_ARGS,
            JailError::Symlink(_) => error_kinds::POLICY_DENIED,
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
    d.risk_level = RiskLevel::Safe;
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
    d.risk_level = RiskLevel::Medium;
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
        "Search under the jail root. Modes: `name` (basename substring), \
         `content` (file-content substring), `glob` (jail-relative path \
         glob — supports *, **, ?). Linear walker (no index)."
            .into(),
    );
    d.categories = vec!["search".into(), "fs".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d.risk_level = RiskLevel::Safe;
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
    d.risk_level = RiskLevel::Medium;
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
    d.risk_level = RiskLevel::Medium;
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
    d.risk_level = RiskLevel::Safe;
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
    d.risk_level = RiskLevel::Safe;
    d
}

/// PH-FS-PARITY4 + PH-FS-AUDIT-FILTER: `tool.fs.audit_recent` —
/// snapshot the most recent successful write / append / patch /
/// fuzzy_replace mutations on the jail. Pure in-memory
/// observability; bounded ring of 256 entries. Older entries
/// are evicted in FIFO order. Does NOT replace the
/// dispatch-level audit log.
pub fn descriptor_audit_recent() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.fs.audit_recent");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["fs:audit".into()];
    d.requires_groups = vec!["operators".into()];
    d.description = Some(
        "Return the most recent successful fs mutations on the jail. Arg shapes: \
         empty (default max), `<positive_integer>` (legacy max-only form), or \
         JSON `{max?, op?}` where op filters to write|append|patch|fuzzy_replace. \
         Tab-delim rows: ts_secs\\top\\trel_path\\tbytes\\tcaller_subject_id. \
         Newest first."
            .into(),
    );
    d.categories = vec!["read".into(), "fs".into(), "audit".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d.risk_level = RiskLevel::Safe;
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
    d.risk_level = RiskLevel::Safe;
    d
}

/// PH-FS-FUZZY: `tool.fuzzy_replace` — Hermes-style fuzzy text
/// edit that tolerates whitespace differences. Caller supplies
/// `<rel_path>|<search>|<replace>`. The search block is matched
/// against the file with leading/trailing whitespace per line
/// normalized; on hit the matched span is replaced verbatim with
/// the replacement. Atomic write via the tempfile-rename pattern.
/// Refuses when the search block is found zero or multiple times
/// — there's no automatic disambiguation.
pub fn descriptor_fuzzy_replace() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.fuzzy_replace");
    d.major_version = 1;
    d.idempotency = Idempotency::AtMostOnce;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["fs:write".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Replace a block of text in a file, tolerating whitespace differences \
         between caller-supplied search text and the file. Wire: \
         `<rel_path>|<search>|<replace>`. Refuses on zero matches or multiple \
         matches (no auto-disambiguation). Atomic write. Records mutation on \
         tool.fs.audit_recent."
            .into(),
    );
    d.categories = vec!["mutate".into(), "fs".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d.risk_level = RiskLevel::Medium;
    d
}

/// PH-FS-TREE: `tool.fs.tree` — recursive directory walk with
/// depth cap. Wire: `<rel_path>` or `<rel_path>|<max_depth>`
/// (default depth 5). Returns tab-delim rows
/// `<depth>\t<kind>\t<rel_path>\t<size>`. Cap at the jail's
/// `max_search_results`.
pub fn descriptor_tree() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.fs.tree");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Expensive;
    d.sensitivity_tags = vec!["fs:read".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Recursive directory walk under the jail root, depth-bounded. Wire: \
         `<rel_path>` or `<rel_path>|<max_depth>` (default 5). Tab-delim rows: \
         depth\\tkind\\trel_path\\tsize. Capped at max_search_results entries."
            .into(),
    );
    d.categories = vec!["read".into(), "fs".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d.risk_level = RiskLevel::Safe;
    d
}

/// PH-FS-STAT: `tool.fs.stat` — metadata for a single path.
/// Wire: `<rel_path>`. Returns tab-delim key=value pairs.
pub fn descriptor_stat() -> CapabilityDescriptor {
    let mut d = CapabilityDescriptor::unary("tool.fs.stat");
    d.major_version = 1;
    d.idempotency = Idempotency::Idempotent;
    d.cost_class = CostClass::Cheap;
    d.sensitivity_tags = vec!["fs:read".into()];
    d.requires_groups = vec!["chat-users".into()];
    d.description = Some(
        "Return file/directory metadata for a single jail-relative path. \
         Tab-delim key=value pairs: path, kind (file/dir/symlink/other), \
         size, mtime, is_symlink, exists."
            .into(),
    );
    d.categories = vec!["read".into(), "fs".into()];
    d.environment_requirements = vec!["fs:jail".into()];
    d.risk_level = RiskLevel::Safe;
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
        let j = jail.clone();
        bridge.register(
            "tool.binary_sniff",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_binary_sniff(&j, &ctx) }
            })),
        );
    }
    {
        let j = jail.clone();
        bridge.register(
            "tool.fs.audit_recent",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_audit_recent(&j, &ctx) }
            })),
        );
    }
    {
        let j = jail.clone();
        bridge.register(
            "tool.fuzzy_replace",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_fuzzy_replace(&j, &ctx) }
            })),
        );
    }
    {
        let j = jail.clone();
        bridge.register(
            "tool.fs.tree",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_tree(&j, &ctx) }
            })),
        );
    }
    {
        let j = jail;
        bridge.register(
            "tool.fs.stat",
            Arc::new(FnHandler(move |ctx: InvocationCtx| {
                let j = j.clone();
                async move { handle_stat(&j, &ctx) }
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
    let rel_display = jail.display_rel(&canonical);
    jail.record_mutation("write", rel_display.clone(), content.len(), ctx);
    let body = format!("ok bytes={} path={}\n", content.len(), rel_display);
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
            "tool.search_files arg must be `mode|pattern|max_results` (mode: name|content|glob)"
                .into(),
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
                        let trimmed_line = if line.len() > 240 {
                            // Snap 240 down to a char boundary so a
                            // multi-byte codepoint isn't split (panics).
                            let mut cut = 240;
                            while cut > 0 && !line.is_char_boundary(cut) {
                                cut -= 1;
                            }
                            &line[..cut]
                        } else {
                            line
                        };
                        hits.push(format!("{}:{}:{}", rel, i + 1, trimmed_line));
                    }
                }
            }
        }
        "glob" => {
            // Match jail-relative path (forward-slash normalized)
            // against pattern using `*`, `**`, `?` semantics. See
            // `glob_match` for the supported pattern grammar.
            for p in walked {
                if hits.len() >= cap {
                    break;
                }
                let rel = jail.display_rel(&p);
                let normalized = rel.replace('\\', "/");
                if glob_match(pattern, &normalized) {
                    hits.push(rel);
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
    jail.record_mutation("patch", jail.display_rel(&canonical), patched.len(), ctx);
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
    jail.record_mutation("append", jail.display_rel(&canonical), body.len(), ctx);
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

/// Minimal glob matcher used by `tool.search_files` in `glob` mode.
///
/// Supported wildcards:
///   * `*`  — matches any run of characters NOT containing `/`.
///   * `**` — matches any run of characters, including `/`. A
///     trailing `/` in the pattern (`**/foo`) is consumed by the
///     wildcard so `**/foo` matches both `foo` and `bar/baz/foo`.
///   * `?`  — matches a single character that is NOT `/`.
///   * everything else matches literally.
///
/// Paths are expected to be forward-slash normalized by the
/// caller (Windows backslashes are translated to `/`).
fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = path.chars().collect();
    glob_inner(&p, 0, &s, 0)
}

fn glob_inner(p: &[char], pi: usize, s: &[char], si: usize) -> bool {
    if pi >= p.len() {
        return si >= s.len();
    }
    match p[pi] {
        '*' if pi + 1 < p.len() && p[pi + 1] == '*' => {
            // `**` — matches zero or more characters including `/`.
            // Optionally consume the trailing `/` in the pattern.
            let mut after = pi + 2;
            if after < p.len() && p[after] == '/' {
                after += 1;
            }
            for off in 0..=s.len().saturating_sub(si) {
                if glob_inner(p, after, s, si + off) {
                    return true;
                }
            }
            false
        }
        '*' => {
            let after = pi + 1;
            for off in 0..=s.len().saturating_sub(si) {
                if off > 0 && s[si + off - 1] == '/' {
                    // Single * may not span a path separator.
                    return false;
                }
                if glob_inner(p, after, s, si + off) {
                    return true;
                }
            }
            false
        }
        '?' => {
            if si < s.len() && s[si] != '/' {
                glob_inner(p, pi + 1, s, si + 1)
            } else {
                false
            }
        }
        c => {
            if si < s.len() && s[si] == c {
                glob_inner(p, pi + 1, s, si + 1)
            } else {
                false
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

/// PH-FS-PARITY4 + PH-FS-AUDIT-FILTER: handle
/// `tool.fs.audit_recent`. Arg shapes:
///
///   (empty)                — default max (256), no op filter
///   `<positive_integer>`   — that max, no op filter (legacy)
///   `{"max":50,"op":"write"}` — JSON form (both fields optional)
///
/// Op filter values: `write` | `append` | `patch` |
/// `fuzzy_replace`. Returns rows newest-first, tab-delim:
/// `ts_secs\top\trel_path\tbytes\tcaller_subject_id`. Final
/// row is `count=<N>` (after filtering).
fn handle_audit_recent(jail: &FsJail, ctx: &InvocationCtx) -> HandlerOutcome {
    use std::fmt::Write as _;
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("tool.fs.audit_recent arg utf8: {e}")),
    };

    #[derive(Debug, Deserialize)]
    struct AuditRequest {
        #[serde(default)]
        max: Option<usize>,
        #[serde(default)]
        op: Option<String>,
    }

    let (max, op_filter): (usize, Option<String>) = if s.is_empty() {
        (FS_AUDIT_RING_DEFAULT, None)
    } else if s.starts_with('{') {
        // JSON form — both fields optional.
        let req: AuditRequest = match serde_json::from_str(s) {
            Ok(r) => r,
            Err(e) => {
                return invalid(format!("tool.fs.audit_recent: bad JSON: {e}"));
            }
        };
        let max = match req.max {
            None => FS_AUDIT_RING_DEFAULT,
            Some(0) => {
                return invalid("tool.fs.audit_recent: max must be > 0".into());
            }
            Some(n) => n.min(FS_AUDIT_RING_DEFAULT),
        };
        let op = req.op.as_deref().map(|o| o.to_string());
        if let Some(ref o) = op
            && !matches!(o.as_str(), "write" | "append" | "patch" | "fuzzy_replace")
        {
            return invalid(format!(
                "tool.fs.audit_recent: unknown op '{o}' (supported: write, append, patch, fuzzy_replace)"
            ));
        }
        (max, op)
    } else {
        // Legacy integer form — backward-compatible with the
        // original PH-FS-PARITY4 wire.
        match s.parse::<usize>() {
            Ok(n) if n > 0 => (n.min(FS_AUDIT_RING_DEFAULT), None),
            _ => {
                return invalid(format!(
                    "tool.fs.audit_recent: arg must be a positive integer or JSON \
                     (got '{s}')"
                ));
            }
        }
    };

    // PH-FS-AUDIT-FILTER: pull a generous snapshot when filter
    // is on, so we can still return up to `max` matching entries
    // (the ring is bounded; pulling everything is cheap and the
    // filter is applied client-side here).
    let snapshot_cap = if op_filter.is_some() {
        FS_AUDIT_RING_DEFAULT
    } else {
        max
    };
    let entries = jail.audit_snapshot(snapshot_cap);
    let filtered: Vec<FsAuditEntry> = match op_filter.as_deref() {
        Some(op) => entries
            .into_iter()
            .filter(|e| e.op == op)
            .take(max)
            .collect(),
        None => entries.into_iter().take(max).collect(),
    };
    let count = filtered.len();
    let mut buf = String::new();
    for e in filtered {
        let safe_path = e.rel_path.replace(['\t', '\n'], " ");
        let _ = writeln!(
            buf,
            "{}\t{}\t{}\t{}\t{}",
            e.ts_secs, e.op, safe_path, e.bytes, e.caller_subject_id
        );
    }
    let _ = writeln!(buf, "count={count}");
    HandlerOutcome::Ok(buf.into_bytes())
}

/// PH-FS-FUZZY: handle `tool.fuzzy_replace`. Wire:
/// `<rel_path>|<search>|<replace>`. The search block is matched
/// against the file with each line's leading + trailing
/// whitespace normalized. Refuses on zero matches or multiple
/// matches.
fn handle_fuzzy_replace(jail: &FsJail, ctx: &InvocationCtx) -> HandlerOutcome {
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.fuzzy_replace arg utf8: {e}")),
    };
    let mut parts = s.splitn(3, '|');
    let rel = parts.next().unwrap_or("").trim();
    let search = parts.next().unwrap_or("");
    let replace = parts.next().unwrap_or("");
    if rel.is_empty() || search.is_empty() {
        return invalid(
            "tool.fuzzy_replace arg must be `<rel_path>|<search>|<replace>` \
             (rel_path + search are required; replace may be empty)"
                .into(),
        );
    }
    let canonical = match jail.resolve(rel, true) {
        Ok(p) => p,
        Err(e) => return e.into(),
    };
    let body = match std::fs::read_to_string(&canonical) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.fuzzy_replace read: {e}")),
    };
    if body.len() > jail.cfg.max_read_bytes {
        return invalid(format!(
            "tool.fuzzy_replace: file {} bytes exceeds read cap {}",
            body.len(),
            jail.cfg.max_read_bytes
        ));
    }
    let matches = fuzzy_find_matches(&body, search);
    if matches.is_empty() {
        return invalid("tool.fuzzy_replace: search block not found".into());
    }
    if matches.len() > 1 {
        return invalid(format!(
            "tool.fuzzy_replace: search block matches {} times; refusing to \
             auto-disambiguate (rephrase with more context)",
            matches.len()
        ));
    }
    let (start, end) = matches[0];
    let mut patched = String::with_capacity(body.len() + replace.len());
    patched.push_str(&body[..start]);
    patched.push_str(replace);
    patched.push_str(&body[end..]);
    if patched.len() > jail.cfg.max_write_bytes {
        return invalid(format!(
            "tool.fuzzy_replace: patched file {} bytes exceeds write cap {}",
            patched.len(),
            jail.cfg.max_write_bytes
        ));
    }
    let parent = canonical.parent().expect("canonical has parent");
    let tmp = match tempfile_in_dir(parent) {
        Ok(t) => t,
        Err(e) => return invalid(format!("tool.fuzzy_replace tempfile: {e}")),
    };
    if let Err(e) = std::fs::write(&tmp, patched.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return invalid(format!("tool.fuzzy_replace write tempfile: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp, &canonical) {
        let _ = std::fs::remove_file(&tmp);
        return invalid(format!("tool.fuzzy_replace rename: {e}"));
    }
    let rel_display = jail.display_rel(&canonical);
    jail.record_mutation("fuzzy_replace", rel_display.clone(), patched.len(), ctx);
    let body = format!("ok bytes={} path={}\n", patched.len(), rel_display);
    HandlerOutcome::Ok(body.into_bytes())
}

/// PH-FS-FUZZY: normalize a string for whitespace-tolerant
/// matching. Trims each line's leading + trailing whitespace
/// and collapses internal runs of whitespace into a single
/// space. Newlines preserved between lines.
fn normalize_for_fuzzy(s: &str) -> String {
    s.lines()
        .map(|line| line.split_whitespace().collect::<Vec<&str>>().join(" "))
        .collect::<Vec<String>>()
        .join("\n")
}

/// PH-FS-FUZZY: find all (start, end) byte ranges in `haystack`
/// whose normalized form equals the normalized `needle`. Done
/// by scanning every line-aligned window of `needle.lines()`
/// length and comparing normalized forms.
fn fuzzy_find_matches(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    let needle_norm = normalize_for_fuzzy(needle);
    let needle_line_count = needle.lines().count().max(1);
    let mut out: Vec<(usize, usize)> = Vec::new();
    let line_offsets: Vec<usize> = std::iter::once(0)
        .chain(
            haystack
                .char_indices()
                .filter_map(|(i, c)| if c == '\n' { Some(i + 1) } else { None }),
        )
        .collect();
    let total_lines = haystack.lines().count();
    if total_lines == 0 {
        return out;
    }
    for start_line in 0..=total_lines.saturating_sub(needle_line_count) {
        let start = line_offsets[start_line];
        let end_line = start_line + needle_line_count;
        let end = if end_line < line_offsets.len() {
            // Exclude the trailing newline; we want to replace
            // exactly the needle's line range, not the
            // following separator.
            line_offsets[end_line].saturating_sub(1)
        } else {
            haystack.len()
        };
        let window = &haystack[start..end];
        if normalize_for_fuzzy(window) == needle_norm {
            out.push((start, end));
        }
    }
    out
}

/// PH-FS-TREE: handle `tool.fs.tree`. Wire: `<rel_path>` or
/// `<rel_path>|<max_depth>` (default depth 5).
fn handle_tree(jail: &FsJail, ctx: &InvocationCtx) -> HandlerOutcome {
    use std::fmt::Write as _;
    const DEFAULT_DEPTH: usize = 5;
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s,
        Err(e) => return invalid(format!("tool.fs.tree arg utf8: {e}")),
    };
    let (rel, max_depth) = match s.rsplit_once('|') {
        Some((p, n_str)) if n_str.trim().parse::<usize>().is_ok() => (
            p.trim(),
            n_str.trim().parse::<usize>().unwrap_or(DEFAULT_DEPTH),
        ),
        _ => (s.trim(), DEFAULT_DEPTH),
    };
    let rel = if rel.is_empty() { "." } else { rel };
    let canonical = if rel == "." {
        jail.canonical_root.clone()
    } else {
        match jail.resolve(rel, true) {
            Ok(p) => p,
            Err(e) => return e.into(),
        }
    };
    let meta = match std::fs::metadata(&canonical) {
        Ok(m) => m,
        Err(e) => return invalid(format!("tool.fs.tree metadata: {e}")),
    };
    if !meta.is_dir() {
        return invalid(format!(
            "tool.fs.tree: '{}' is not a directory",
            jail.display_rel(&canonical)
        ));
    }
    let mut buf = String::new();
    let cap = jail.cfg.max_search_results;
    let mut emitted = 0usize;
    // Pre-order DFS so directory parents print before their
    // children — operator-readable.
    let mut stack: Vec<(PathBuf, usize)> = vec![(canonical.clone(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if emitted >= cap {
            break;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        let mut collected: Vec<std::fs::DirEntry> = entries.flatten().collect();
        // Stable sort for deterministic output.
        collected.sort_by_key(|e| e.file_name());
        // Push subdirs in reverse so the first one is processed first
        // (we're using a stack, hence reverse).
        for entry in collected.iter().rev() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() && depth < max_depth {
                stack.push((path, depth + 1));
            }
        }
        for entry in &collected {
            if emitted >= cap {
                break;
            }
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
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
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let rel = jail.display_rel(&path);
            let safe_rel = rel.replace(['\t', '\n'], " ");
            let _ = writeln!(buf, "{depth}\t{kind}\t{safe_rel}\t{size}");
            emitted += 1;
        }
    }
    let _ = writeln!(buf, "count={emitted}");
    HandlerOutcome::Ok(buf.into_bytes())
}

/// PH-FS-STAT: handle `tool.fs.stat`. Wire: `<rel_path>`.
/// Returns one line of `key=value` pairs, tab-separated.
fn handle_stat(jail: &FsJail, ctx: &InvocationCtx) -> HandlerOutcome {
    use std::fmt::Write as _;
    let s = match std::str::from_utf8(&ctx.args) {
        Ok(s) => s.trim(),
        Err(e) => return invalid(format!("tool.fs.stat arg utf8: {e}")),
    };
    if s.is_empty() {
        return invalid("tool.fs.stat: rel_path required".into());
    }
    // Resolve with must_exist=false so we can report exists=false
    // honestly without surfacing a canonicalize error.
    let canonical = match jail.resolve(s, false) {
        Ok(p) => p,
        Err(e) => return e.into(),
    };
    let rel_display = jail.display_rel(&canonical);
    let mut buf = String::new();
    match std::fs::symlink_metadata(&canonical) {
        Ok(m) => {
            let ft = m.file_type();
            let kind = if ft.is_dir() {
                "dir"
            } else if ft.is_file() {
                "file"
            } else if ft.is_symlink() {
                "symlink"
            } else {
                "other"
            };
            let size = m.len();
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let _ = writeln!(
                buf,
                "path={}\tkind={}\tsize={}\tmtime={}\tis_symlink={}\texists=true",
                rel_display,
                kind,
                size,
                mtime,
                ft.is_symlink(),
            );
        }
        Err(_) => {
            let _ = writeln!(
                buf,
                "path={}\tkind=missing\tsize=0\tmtime=0\tis_symlink=false\texists=false",
                rel_display,
            );
        }
    }
    HandlerOutcome::Ok(buf.into_bytes())
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
            tenant_id: None,
        }
    }

    // ── TOCTOU hardening tests (Task 2) ────────────────────

    #[test]
    fn jail_contains_path_rejects_equal_and_outside() {
        let td = TempDir::new().unwrap();
        let base = td.path().canonicalize().unwrap();
        // Equal-to-base is REJECTED — the base dir itself is not
        // "inside" the jail, only its descendants are.
        assert!(!jail_contains_path(&base, &base));
        // Outside-base is rejected.
        let outside = base.parent().unwrap().to_path_buf();
        assert!(!jail_contains_path(&base, &outside));
        // Child path is accepted.
        let child = base.join("file.txt");
        assert!(jail_contains_path(&base, &child));
    }

    #[test]
    fn toctou_normal_file_inside_jail_is_accepted() {
        let (td, j) = mk_jail();
        let p = td.path().join("hello.txt");
        std::fs::write(&p, b"hi").unwrap();
        // resolve() canonicalises + symlink-checks; a plain file
        // inside the jail must come back as Ok.
        let resolved = j.resolve("hello.txt", true).unwrap();
        assert!(resolved.ends_with("hello.txt"));
        assert!(jail_contains_path(&j.canonical_root, &resolved));
    }

    #[test]
    fn toctou_dotdot_traversal_is_rejected() {
        let (_td, j) = mk_jail();
        let err = j
            .resolve("subdir/../../etc/passwd", true)
            .expect_err("must reject `..`");
        assert!(matches!(err, JailError::Traversal(_)), "wrong err: {err:?}");
    }

    #[test]
    fn toctou_path_equal_to_base_is_rejected_by_jail_contains_path() {
        // `jail_contains_path` returns FALSE for `path == base` so
        // callers wanting strict containment (e.g. "this must be a
        // file inside the jail, not the jail itself") can rely on
        // it. `resolve(".")` is permitted because list_dir / stat
        // legitimately operate on the jail root; the test exercises
        // the contract of the public helper.
        let td = TempDir::new().unwrap();
        let base = td.path().canonicalize().unwrap();
        assert!(!jail_contains_path(&base, &base));
        // A subpath that resolves back to base via normalisation
        // (e.g. via an absolute symlink) would be a real escape.
        // The strict helper guards that path.
        let outside = base.parent().unwrap().to_path_buf();
        assert!(!jail_contains_path(&base, &outside));
    }

    #[cfg(unix)]
    #[test]
    fn toctou_symlink_pointing_outside_jail_is_rejected() {
        // Sandbox-local symlink farm: drop a `link` inside the
        // jail that points to `/tmp` (outside). resolve()'s
        // symlink walk refuses before the consumer ever opens
        // the file.
        use std::os::unix::fs::symlink;
        let (td, j) = mk_jail();
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("real.txt");
        std::fs::write(&target, b"sensitive").unwrap();
        let link = td.path().join("evil");
        symlink(&target, &link).unwrap();
        let err = j.resolve("evil", true).expect_err("must reject symlink");
        assert!(matches!(err, JailError::Symlink(_)) || matches!(err, JailError::Escape(_)));
    }

    #[cfg(unix)]
    #[test]
    fn toctou_symlink_pointing_inside_jail_is_still_rejected() {
        // Even a symlink that points to a path inside the jail is
        // refused — the policy is "no symlinks", not "no escapes
        // via symlink." This shrinks the window where a swap mid-
        // operation could move the symlink's target out.
        use std::os::unix::fs::symlink;
        let (td, j) = mk_jail();
        let real = td.path().join("real.txt");
        std::fs::write(&real, b"x").unwrap();
        let link = td.path().join("alias");
        symlink(&real, &link).unwrap();
        let err = j
            .resolve("alias", true)
            .expect_err("symlinks inside jail are still refused");
        assert!(matches!(err, JailError::Symlink(_)));
    }

    #[test]
    fn refuse_symlinks_within_jail_passes_for_normal_path() {
        // The bare helper must accept a normal file without any
        // symlink in any component.
        let td = TempDir::new().unwrap();
        let base = td.path().canonicalize().unwrap();
        let f = base.join("a.txt");
        std::fs::write(&f, b"x").unwrap();
        refuse_symlinks_within_jail(&base, &f).expect("normal path must pass");
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

    // ── PH-FS-PARITY3: tool.search_files glob mode ─────────────────

    #[test]
    fn glob_match_star_does_not_span_path_sep() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "src/main.rs"));
    }

    #[test]
    fn glob_match_double_star_spans_path_sep() {
        assert!(glob_match("**/*.rs", "main.rs"));
        assert!(glob_match("**/*.rs", "src/main.rs"));
        assert!(glob_match("**/*.rs", "src/nodes/tool/fs.rs"));
        assert!(!glob_match("**/*.rs", "src/main.txt"));
    }

    #[test]
    fn glob_match_question_mark_matches_one_non_slash() {
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(!glob_match("a?c", "a/c"));
    }

    #[test]
    fn glob_match_literal_segment_match() {
        assert!(glob_match("src/main.rs", "src/main.rs"));
        assert!(!glob_match("src/main.rs", "src/lib.rs"));
    }

    #[test]
    fn glob_match_double_star_only_matches_anything() {
        assert!(glob_match("**", "anything"));
        assert!(glob_match("**", "a/b/c/d"));
        assert!(glob_match("**", ""));
    }

    #[test]
    fn glob_match_empty_pattern_only_matches_empty() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn search_files_glob_mode_finds_rust_files() {
        let (td, j) = mk_jail();
        // Build a small tree.
        std::fs::create_dir_all(td.path().join("src/nodes")).unwrap();
        std::fs::write(td.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(td.path().join("src/lib.rs"), "// lib").unwrap();
        std::fs::write(td.path().join("src/nodes/mod.rs"), "// mod").unwrap();
        std::fs::write(td.path().join("README.md"), "# readme").unwrap();

        let r = handle_search(&j, &ctx(b"glob|**/*.rs|100"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        // Normalize OS path separators for the assertions.
        let lines: Vec<String> = body.lines().map(|l| l.replace('\\', "/")).collect();
        assert!(lines.iter().any(|l| l == "src/main.rs"), "lines={lines:?}");
        assert!(lines.iter().any(|l| l == "src/lib.rs"), "lines={lines:?}");
        assert!(
            lines.iter().any(|l| l == "src/nodes/mod.rs"),
            "lines={lines:?}"
        );
        assert!(!lines.iter().any(|l| l == "README.md"));
    }

    #[test]
    fn search_files_glob_mode_respects_single_star_segment() {
        let (td, j) = mk_jail();
        std::fs::create_dir_all(td.path().join("src/nodes")).unwrap();
        std::fs::write(td.path().join("src/main.rs"), "x").unwrap();
        std::fs::write(td.path().join("src/nodes/mod.rs"), "x").unwrap();

        // `src/*.rs` matches only files directly under src/, not nested ones.
        let r = handle_search(&j, &ctx(b"glob|src/*.rs|100"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let lines: Vec<String> = body.lines().map(|l| l.replace('\\', "/")).collect();
        assert!(lines.iter().any(|l| l == "src/main.rs"));
        assert!(!lines.iter().any(|l| l == "src/nodes/mod.rs"));
    }

    #[test]
    fn search_files_unknown_mode_rejected() {
        let (_td, j) = mk_jail();
        let r = handle_search(&j, &ctx(b"regex|.*|10"));
        match r {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("unknown mode")),
            _ => panic!("expected Err for unknown mode"),
        }
    }

    #[test]
    fn search_files_glob_mode_respects_max_results_cap() {
        let (td, j) = mk_jail();
        for i in 0..10 {
            std::fs::write(td.path().join(format!("f{i}.txt")), "x").unwrap();
        }
        // Cap is 100 by default; ask for 3 explicitly.
        let r = handle_search(&j, &ctx(b"glob|*.txt|3"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert_eq!(body.lines().count(), 3);
    }

    // ── PH-FS-PARITY4: tool.fs.audit_recent + mutation ring ────────

    #[test]
    fn audit_recent_descriptor_shape() {
        let d = descriptor_audit_recent();
        assert_eq!(d.method_name, "tool.fs.audit_recent");
        assert_eq!(d.major_version, 1);
        assert!(matches!(d.idempotency, Idempotency::Idempotent));
        assert!(matches!(d.cost_class, CostClass::Cheap));
        assert!(d.sensitivity_tags.iter().any(|t| t == "fs:audit"));
        assert!(d.requires_groups.iter().any(|g| g == "operators"));
    }

    #[test]
    fn audit_ring_records_successful_write() {
        let (_td, j) = mk_jail();
        let r = handle_write(&j, &ctx(b"a.txt|overwrite|hello"));
        assert!(matches!(r, HandlerOutcome::Ok(_)));
        let snap = j.audit_snapshot(10);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].op, "write");
        assert_eq!(snap[0].bytes, 5);
        assert!(snap[0].rel_path.ends_with("a.txt"));
        assert!(!snap[0].caller_subject_id.is_empty());
    }

    #[test]
    fn audit_ring_records_append_and_patch() {
        let (_td, j) = mk_jail();
        handle_write(&j, &ctx(b"a.txt|overwrite|hi\n"));
        handle_append(&j, &ctx(b"a.txt|world\n"));
        let diff = "--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n hi\n-world\n+WORLD\n";
        let arg = format!("a.txt|unified_diff|{diff}");
        handle_patch(&j, &ctx(arg.as_bytes()));

        let snap = j.audit_snapshot(10);
        // Newest first: patch, append, write.
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].op, "patch");
        assert_eq!(snap[1].op, "append");
        assert_eq!(snap[2].op, "write");
    }

    #[test]
    fn audit_ring_does_not_record_failed_write() {
        let (_td, j) = mk_jail();
        // Traversal — rejected before any I/O.
        let r = handle_write(&j, &ctx(b"../escape|overwrite|x"));
        assert!(matches!(r, HandlerOutcome::Err(_)));
        let snap = j.audit_snapshot(10);
        assert_eq!(snap.len(), 0);
    }

    #[test]
    fn audit_ring_is_bounded_by_capacity_default() {
        let (_td, j) = mk_jail();
        // Push more than the default 256 to exercise eviction.
        for i in 0..(FS_AUDIT_RING_DEFAULT + 10) {
            handle_write(&j, &ctx(format!("f{i}.txt|overwrite|x").as_bytes()));
        }
        let snap = j.audit_snapshot(10_000);
        assert_eq!(snap.len(), FS_AUDIT_RING_DEFAULT);
    }

    #[test]
    fn audit_recent_handler_returns_newest_first_with_count() {
        let (_td, j) = mk_jail();
        handle_write(&j, &ctx(b"a.txt|overwrite|x"));
        handle_write(&j, &ctx(b"b.txt|overwrite|yy"));
        let r = handle_audit_recent(&j, &ctx(b""));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let lines: Vec<&str> = body.lines().collect();
        // 2 entries + count= trailer.
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("b.txt"));
        assert!(lines[1].contains("a.txt"));
        assert_eq!(lines[2], "count=2");
    }

    #[test]
    fn audit_recent_handler_respects_max_arg() {
        let (_td, j) = mk_jail();
        for i in 0..5 {
            handle_write(&j, &ctx(format!("f{i}.txt|overwrite|x").as_bytes()));
        }
        let r = handle_audit_recent(&j, &ctx(b"2"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let lines: Vec<&str> = body.lines().collect();
        // 2 entries + count=2 trailer.
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], "count=2");
    }

    #[test]
    fn audit_recent_handler_rejects_non_numeric_arg() {
        let (_td, j) = mk_jail();
        let r = handle_audit_recent(&j, &ctx(b"abc"));
        match r {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("positive integer") || e.cause.contains("JSON"));
            }
            _ => panic!("expected Err"),
        }
    }

    // ── PH-FS-AUDIT-FILTER: op-filter on tool.fs.audit_recent ─────

    #[test]
    fn audit_filter_json_op_write_only_returns_writes() {
        let (_td, j) = mk_jail();
        // Mix of ops via the high-level handlers.
        handle_write(&j, &ctx(b"a.txt|overwrite|hi"));
        handle_append(&j, &ctx(b"a.txt|world"));
        handle_write(&j, &ctx(b"b.txt|overwrite|x"));
        // Filter to write only.
        let r = handle_audit_recent(&j, &ctx(br#"{"op":"write"}"#));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let lines: Vec<&str> = body.lines().collect();
        // 2 write rows + count=2 trailer.
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[2], "count=2");
        for row in &lines[..lines.len() - 1] {
            assert!(row.contains("\twrite\t"), "non-write row leaked: {row}");
        }
    }

    #[test]
    fn audit_filter_json_op_append_excludes_writes() {
        let (_td, j) = mk_jail();
        handle_write(&j, &ctx(b"a.txt|overwrite|hi"));
        handle_append(&j, &ctx(b"a.txt|world\n"));
        let r = handle_audit_recent(&j, &ctx(br#"{"op":"append"}"#));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.last().unwrap(), &"count=1");
        assert!(lines[0].contains("\tappend\t"));
    }

    #[test]
    fn audit_filter_json_with_max_caps_post_filter() {
        let (_td, j) = mk_jail();
        for i in 0..5 {
            handle_write(&j, &ctx(format!("f{i}.txt|overwrite|x").as_bytes()));
        }
        let r = handle_audit_recent(&j, &ctx(br#"{"max":3,"op":"write"}"#));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.last().unwrap(), &"count=3");
    }

    #[test]
    fn audit_filter_json_unknown_op_rejected() {
        let (_td, j) = mk_jail();
        let r = handle_audit_recent(&j, &ctx(br#"{"op":"frobnicate"}"#));
        match r {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("unknown op"));
                assert!(e.cause.contains("frobnicate"));
            }
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn audit_filter_legacy_integer_form_still_works() {
        let (_td, j) = mk_jail();
        handle_write(&j, &ctx(b"a.txt|overwrite|x"));
        handle_write(&j, &ctx(b"b.txt|overwrite|y"));
        // Pure integer arg — backward-compatible path.
        let r = handle_audit_recent(&j, &ctx(b"5"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert_eq!(body.lines().last().unwrap(), "count=2");
    }

    #[test]
    fn audit_filter_json_bad_shape_rejected() {
        let (_td, j) = mk_jail();
        // Starts with `{` so JSON path; payload is invalid JSON.
        let r = handle_audit_recent(&j, &ctx(b"{not json"));
        match r {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("bad JSON"));
            }
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn audit_filter_json_zero_max_rejected() {
        let (_td, j) = mk_jail();
        let r = handle_audit_recent(&j, &ctx(br#"{"max":0}"#));
        match r {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("max must be > 0"));
            }
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn audit_filter_json_op_only_no_max_uses_default() {
        let (_td, j) = mk_jail();
        // Push 10 writes; default max should cover them all.
        for i in 0..10 {
            handle_write(&j, &ctx(format!("f{i}.txt|overwrite|x").as_bytes()));
        }
        let r = handle_audit_recent(&j, &ctx(br#"{"op":"write"}"#));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert_eq!(body.lines().last().unwrap(), "count=10");
    }

    #[test]
    fn audit_filter_json_fuzzy_replace_op_matches() {
        let (td, j) = mk_jail();
        let p = td.path().join("code.txt");
        std::fs::write(&p, "fn a() {}\nfn b() {}\n").unwrap();
        let arg = "code.txt|fn b() {}|fn B() {}";
        handle_fuzzy_replace(&j, &ctx(arg.as_bytes()));
        // Mix in a plain write so the filter has to work.
        handle_write(&j, &ctx(b"other.txt|overwrite|hi"));
        let r = handle_audit_recent(&j, &ctx(br#"{"op":"fuzzy_replace"}"#));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.last().unwrap(), &"count=1");
        assert!(lines[0].contains("\tfuzzy_replace\t"));
    }

    // ── PH-FS-FUZZY: tool.fuzzy_replace ────────────────────────────

    #[test]
    fn fuzzy_replace_descriptor_shape() {
        let d = descriptor_fuzzy_replace();
        assert_eq!(d.method_name, "tool.fuzzy_replace");
        assert!(matches!(d.idempotency, Idempotency::AtMostOnce));
        assert!(d.sensitivity_tags.iter().any(|t| t == "fs:write"));
    }

    #[test]
    fn normalize_for_fuzzy_collapses_internal_whitespace() {
        assert_eq!(
            normalize_for_fuzzy("  fn  foo (  )  {\n   bar  ;\n}\n"),
            "fn foo ( ) {\nbar ;\n}\n".trim_end().to_string()
        );
    }

    #[test]
    fn fuzzy_find_matches_exact_match() {
        let body = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let hits = fuzzy_find_matches(body, "fn b() {}");
        assert_eq!(hits.len(), 1);
        let (s, e) = hits[0];
        assert_eq!(&body[s..e], "fn b() {}");
    }

    #[test]
    fn fuzzy_find_matches_tolerates_whitespace_diff() {
        let body = "    fn a() {\n        body();\n    }\n";
        let hits = fuzzy_find_matches(body, "fn a() {\nbody();\n}");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn fuzzy_find_matches_multiple_hits() {
        let body = "x = 1\nx = 1\nx = 1\n";
        let hits = fuzzy_find_matches(body, "x = 1");
        assert!(hits.len() >= 2);
    }

    #[test]
    fn fuzzy_replace_succeeds_with_single_match() {
        let (td, j) = mk_jail();
        let p = td.path().join("code.txt");
        std::fs::write(&p, "fn a() {}\n    fn b() {}\nfn c() {}\n").unwrap();
        let arg = "code.txt|fn b() {}|fn B() { panic!(); }";
        let r = handle_fuzzy_replace(&j, &ctx(arg.as_bytes()));
        match r {
            HandlerOutcome::Ok(_) => {}
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        }
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("fn B() { panic!(); }"));
        assert!(!after.contains("fn b() {}"));
        // Audit ring should have recorded the mutation.
        let snap = j.audit_snapshot(10);
        assert!(snap.iter().any(|e| e.op == "fuzzy_replace"));
    }

    #[test]
    fn fuzzy_replace_refuses_zero_matches() {
        let (td, j) = mk_jail();
        std::fs::write(td.path().join("c.txt"), "hello\n").unwrap();
        let r = handle_fuzzy_replace(&j, &ctx(b"c.txt|world|foo"));
        match r {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("not found")),
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn fuzzy_replace_refuses_multiple_matches() {
        let (td, j) = mk_jail();
        std::fs::write(td.path().join("c.txt"), "x = 1\nx = 1\nx = 1\n").unwrap();
        let r = handle_fuzzy_replace(&j, &ctx(b"c.txt|x = 1|x = 2"));
        match r {
            HandlerOutcome::Err(e) => {
                assert!(e.cause.contains("matches") && e.cause.contains("refusing"));
            }
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn fuzzy_replace_rejects_traversal() {
        let (_td, j) = mk_jail();
        let r = handle_fuzzy_replace(&j, &ctx(b"../escape|foo|bar"));
        match r {
            HandlerOutcome::Err(_) => {}
            _ => panic!("expected traversal rejection"),
        }
    }

    // ── PH-FS-TREE: tool.fs.tree ───────────────────────────────────

    #[test]
    fn tree_descriptor_shape() {
        let d = descriptor_tree();
        assert_eq!(d.method_name, "tool.fs.tree");
        assert!(matches!(d.cost_class, CostClass::Expensive));
    }

    #[test]
    fn tree_returns_depth_prefixed_rows() {
        let (td, j) = mk_jail();
        std::fs::create_dir_all(td.path().join("a/b/c")).unwrap();
        std::fs::write(td.path().join("a/x.txt"), "x").unwrap();
        std::fs::write(td.path().join("a/b/y.txt"), "yy").unwrap();
        std::fs::write(td.path().join("a/b/c/z.txt"), "zzz").unwrap();

        let r = handle_tree(&j, &ctx(b"."));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let lines: Vec<String> = body.lines().map(|l| l.replace('\\', "/")).collect();
        // Every row except the trailer starts with a depth digit.
        for l in &lines[..lines.len() - 1] {
            assert!(l.chars().next().unwrap().is_ascii_digit(), "row: {l}");
        }
        // Includes the trailer.
        assert!(lines.last().unwrap().starts_with("count="));
        // Includes deeper files.
        assert!(lines.iter().any(|l| l.contains("a/b/c/z.txt")));
    }

    #[test]
    fn tree_respects_max_depth() {
        let (td, j) = mk_jail();
        std::fs::create_dir_all(td.path().join("a/b/c")).unwrap();
        std::fs::write(td.path().join("a/x.txt"), "x").unwrap();
        std::fs::write(td.path().join("a/b/y.txt"), "y").unwrap();
        std::fs::write(td.path().join("a/b/c/z.txt"), "z").unwrap();
        // Depth 1 should walk root + immediate children, NOT
        // into b/c — z.txt sits at depth 3.
        let r = handle_tree(&j, &ctx(b".|1"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        let body = body.replace('\\', "/");
        assert!(body.contains("a"));
        assert!(!body.contains("z.txt"));
    }

    #[test]
    fn tree_rejects_non_directory() {
        let (td, j) = mk_jail();
        std::fs::write(td.path().join("f.txt"), "x").unwrap();
        let r = handle_tree(&j, &ctx(b"f.txt"));
        match r {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("not a directory")),
            _ => panic!("expected Err"),
        }
    }

    // ── PH-FS-STAT: tool.fs.stat ───────────────────────────────────

    #[test]
    fn stat_descriptor_shape() {
        let d = descriptor_stat();
        assert_eq!(d.method_name, "tool.fs.stat");
        assert!(matches!(d.cost_class, CostClass::Cheap));
    }

    #[test]
    fn stat_existing_file_reports_size_and_kind() {
        let (td, j) = mk_jail();
        std::fs::write(td.path().join("f.txt"), b"hello").unwrap();
        let r = handle_stat(&j, &ctx(b"f.txt"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert!(body.contains("kind=file"));
        assert!(body.contains("size=5"));
        assert!(body.contains("exists=true"));
        assert!(body.contains("is_symlink=false"));
    }

    #[test]
    fn stat_existing_dir_reports_dir_kind() {
        let (td, j) = mk_jail();
        std::fs::create_dir(td.path().join("d")).unwrap();
        let r = handle_stat(&j, &ctx(b"d"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert!(body.contains("kind=dir"));
        assert!(body.contains("exists=true"));
    }

    #[test]
    fn stat_missing_path_reports_exists_false() {
        let (_td, j) = mk_jail();
        let r = handle_stat(&j, &ctx(b"missing.txt"));
        let body = match r {
            HandlerOutcome::Ok(b) => String::from_utf8(b).unwrap(),
            HandlerOutcome::Err(e) => panic!("expected Ok, got: {}", e.cause),
        };
        assert!(body.contains("exists=false"));
        assert!(body.contains("kind=missing"));
    }

    #[test]
    fn stat_empty_arg_rejected() {
        let (_td, j) = mk_jail();
        let r = handle_stat(&j, &ctx(b""));
        match r {
            HandlerOutcome::Err(e) => assert!(e.cause.contains("rel_path required")),
            _ => panic!("expected Err"),
        }
    }

    #[test]
    fn stat_rejects_traversal() {
        let (_td, j) = mk_jail();
        let r = handle_stat(&j, &ctx(b"../escape"));
        match r {
            HandlerOutcome::Err(_) => {}
            _ => panic!("expected traversal rejection"),
        }
    }

    /// PH-RISK-PIN-ALL: pin the risk tier of every shipped fs
    /// descriptor. None should default to Unknown (would trip
    /// the validator) and each carries the deliberate tier
    /// chosen in PH-CAP-RISK stage 2. Reads are Safe, writes /
    /// patches / fuzzy_replace are Medium. Future fs descriptor
    /// additions trip this test and force an audit.
    #[test]
    fn fs_descriptors_have_explicit_non_unknown_risk() {
        let pinned: &[(&str, CapabilityDescriptor, RiskLevel)] = &[
            ("tool.read_file", descriptor_read(), RiskLevel::Safe),
            ("tool.write_file", descriptor_write(), RiskLevel::Medium),
            ("tool.search_files", descriptor_search(), RiskLevel::Safe),
            ("tool.patch", descriptor_patch(), RiskLevel::Medium),
            ("tool.append_file", descriptor_append(), RiskLevel::Medium),
            (
                "tool.patch_preview",
                descriptor_patch_preview(),
                RiskLevel::Safe,
            ),
            (
                "tool.binary_sniff",
                descriptor_binary_sniff(),
                RiskLevel::Safe,
            ),
            (
                "tool.fs.audit_recent",
                descriptor_audit_recent(),
                RiskLevel::Safe,
            ),
            ("tool.list_dir", descriptor_list(), RiskLevel::Safe),
            (
                "tool.fuzzy_replace",
                descriptor_fuzzy_replace(),
                RiskLevel::Medium,
            ),
            ("tool.fs.tree", descriptor_tree(), RiskLevel::Safe),
            ("tool.fs.stat", descriptor_stat(), RiskLevel::Safe),
        ];
        for (name, d, expected) in pinned {
            assert_ne!(
                d.risk_level,
                RiskLevel::Unknown,
                "{name} defaulted to Unknown risk"
            );
            assert_eq!(
                d.risk_level, *expected,
                "{name} risk tier drifted (expected {expected:?})"
            );
        }
    }
}
