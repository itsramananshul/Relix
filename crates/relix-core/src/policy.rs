//! Allowlist policy engine — alpha simplification of RELIX-1 §1.13 step 9 (target: Cedar).
//!
//! Policy is a TOML file with this shape:
//!
//! ```toml
//! [admit]
//! groups = ["chat-users", "tool-users", "memory-admin"]
//!
//! # Per-method rules. Allow if caller satisfies *any* matching rule. Default deny.
//! [[rules]]
//! name = "chat_users_chat"
//! method = "ai.chat"
//! allow_groups = ["chat-users"]
//!
//! [[rules]]
//! name = "tool_users_fetch"
//! method = "tool.web_fetch"
//! allow_groups = ["tool-users"]
//! ```
//!
//! The engine's `evaluate` signature mirrors Cedar's `(principal, action, resource, context)`
//! so the Gate-2 swap is straightforward.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::identity::VerifiedIdentity;

/// Decision outcomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Allowed. `matched_rule` names the rule that admitted the call.
    Allow {
        /// Name of the matched rule (audit-visible).
        matched_rule: String,
    },
    /// Denied with a reason. `matched_rule` is `None` for default-deny.
    Deny {
        /// Human-readable reason.
        reason: String,
        /// Name of the rule that explicitly denied (if any).
        matched_rule: Option<String>,
    },
    // RequireApproval deferred to Gate 2 (SIMP-004).
}

/// Top-level policy file shape (loaded from disk via [`PolicyEngine::from_toml`]).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PolicyFile {
    /// Coarse-grained node-level admission. Empty = admit any verified identity.
    #[serde(default)]
    pub admit: AdmitSection,
    /// Per-method allow rules.
    #[serde(default, rename = "rules")]
    pub rules: Vec<Rule>,
}

/// Node-level admission: who may speak to this node at all (RELIX-5 §H.3 coarse layer).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AdmitSection {
    /// Allow callers in any of these groups. Empty = any identity admitted (alpha default).
    #[serde(default)]
    pub groups: Vec<String>,
}

/// One per-method rule.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rule {
    /// Operator-readable rule name; appears in audit when matched.
    pub name: String,
    /// Method this rule covers (exact match for alpha; wildcards future).
    pub method: String,
    /// Caller must hold at least one of these groups.
    #[serde(default)]
    pub allow_groups: Vec<String>,
}

/// The engine. Holds the parsed policy and an empty/default fallback.
#[derive(Clone, Debug)]
pub struct PolicyEngine {
    file: PolicyFile,
}

impl PolicyEngine {
    /// Construct from a parsed [`PolicyFile`].
    pub fn new(file: PolicyFile) -> Self {
        Self { file }
    }

    /// Load policy from TOML text.
    pub fn from_toml(text: &str) -> Result<Self, PolicyError> {
        let file: PolicyFile =
            toml::from_str(text).map_err(|e| PolicyError::Parse(e.to_string()))?;
        Ok(Self::new(file))
    }

    /// Load policy from a TOML file on disk.
    pub fn from_path(path: &std::path::Path) -> Result<Self, PolicyError> {
        let text = std::fs::read_to_string(path).map_err(|e| PolicyError::Io(e.to_string()))?;
        Self::from_toml(&text)
    }

    /// A permissive default: admit any verified identity for any method (development only).
    /// Used by node binaries when no policy file is configured. Logs a warning at startup.
    pub fn permissive() -> Self {
        Self {
            file: PolicyFile::default(),
        }
    }

    /// Evaluate a call.
    ///
    /// Order:
    /// 1. Node-level admission (`[admit]`): if any groups configured, caller must hold one.
    /// 2. Per-method rules (`[[rules]]`): caller must match an applicable rule.
    /// 3. Default deny.
    pub fn evaluate(&self, caller: &VerifiedIdentity, method: &str) -> Decision {
        // 1. Node admission.
        if !self.file.admit.groups.is_empty() && !caller.has_any_group(&self.file.admit.groups) {
            return Decision::Deny {
                reason: format!("caller {} not admitted by [admit] groups", caller.name),
                matched_rule: None,
            };
        }

        // 2. Per-method rules. First matching rule wins.
        for rule in &self.file.rules {
            if rule.method == method {
                if rule.allow_groups.is_empty() {
                    // A rule with no group constraint is an unconditional allow for that method.
                    return Decision::Allow {
                        matched_rule: rule.name.clone(),
                    };
                }
                if caller.has_any_group(&rule.allow_groups) {
                    return Decision::Allow {
                        matched_rule: rule.name.clone(),
                    };
                }
            }
        }

        // 3. Default deny.
        Decision::Deny {
            reason: format!(
                "no allow rule for method {} matches caller {} (groups={:?})",
                method, caller.name, caller.groups
            ),
            matched_rule: None,
        }
    }

    /// Returns true if a permissive (no-rules) engine. Useful for startup warnings.
    pub fn is_permissive(&self) -> bool {
        self.file.admit.groups.is_empty() && self.file.rules.is_empty()
    }
}

/// Policy-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// TOML parse failure.
    #[error("parse: {0}")]
    Parse(String),
    /// File read failure.
    #[error("io: {0}")]
    Io(String),
}

/// GAP 23B — per-tenant policy resolution.
///
/// Wraps a base [`PolicyEngine`] (the global / shared file) and
/// resolves per-tenant overrides from `dir/{tenant_id}.policy.toml`.
/// Per-tenant engines are cached for [`Self::ttl`] so a hot-path
/// admission check doesn't stat the filesystem on every call.
///
/// Order of resolution for a call with `tenant_id = Some(t)`:
///
/// 1. If `dir` is set AND the per-tenant file exists, parse it
///    (or hit the cache) and evaluate against the per-tenant
///    engine.
/// 2. Otherwise fall back to the global engine the resolver
///    was built with.
///
/// Tenant ids are sanitised before the file lookup — the same
/// alnum-plus-underscore rule used elsewhere (Qdrant collection
/// names, audit partitions) — so an attacker can't traverse out
/// of `dir` via `../../etc/policy.toml`.
pub struct TenantPolicyResolver {
    global: PolicyEngine,
    dir: Option<PathBuf>,
    ttl: Duration,
    /// PART 4: when `true`, every `evaluate` call MUST supply
    /// a non-empty `tenant_id`. A missing tenant returns
    /// `Decision::Deny(SECURITY_DENIED)` with cause
    /// "tenant_id required in multi-tenant mode" rather than
    /// the pre-PART-4 silent fallback to the global engine.
    /// Operators enable this in multi-tenant deployments so
    /// tenant A's policy cannot accidentally apply to tenant
    /// B's traffic.
    tenant_isolation_enabled: bool,
    /// `tenant_id -> (loaded_at, engine_or_negative_cache)`.
    /// `Some(engine)` = a file was found and parsed. `None` =
    /// negative cache: no file at that path. Both expire after
    /// `ttl` so operators can drop a new file in `dir` without
    /// restarting the node.
    ///
    /// CORR PART 4: bounded LRU cache (default
    /// [`TENANT_POLICY_CACHE_CAP`] entries) replaces the
    /// pre-fix unbounded HashMap. On a many-tenant deployment
    /// the pre-fix path grew the map until process restart;
    /// the LRU evicts the least-recently-used entry once the
    /// cap is hit so process memory stays bounded.
    cache: Mutex<lru::LruCache<String, (Instant, Option<PolicyEngine>)>>,
}

/// CORR PART 4: hard cap on the per-tenant policy cache.
/// 1000 entries is well past any realistic single-node tenant
/// count and keeps the resolver's RSS footprint bounded.
pub const TENANT_POLICY_CACHE_CAP: usize = 1000;

impl TenantPolicyResolver {
    /// Construct. `dir = None` disables per-tenant resolution
    /// (every call falls straight through to `global`). A
    /// `ttl_secs` of 0 disables caching — useful in tests.
    /// `tenant_isolation_enabled` defaults to `false`; callers
    /// opt into fail-closed behaviour via
    /// [`Self::with_tenant_isolation`].
    pub fn new(global: PolicyEngine, dir: Option<PathBuf>, ttl_secs: u64) -> Self {
        // SAFETY: TENANT_POLICY_CACHE_CAP is a `const = 1000`,
        // so the NonZeroUsize construction is infallible. The
        // `unwrap_or` keeps clippy::expect_used quiet without
        // changing the constant's invariant.
        let cap = std::num::NonZeroUsize::new(TENANT_POLICY_CACHE_CAP)
            .unwrap_or(std::num::NonZeroUsize::MIN);
        Self {
            global,
            dir,
            ttl: Duration::from_secs(ttl_secs),
            tenant_isolation_enabled: false,
            cache: Mutex::new(lru::LruCache::new(cap)),
        }
    }

    /// PART 4: enable fail-closed semantics. When set, calls
    /// to [`Self::evaluate`] with `tenant_id = None` return a
    /// deny decision instead of falling back to the global
    /// engine.
    pub fn with_tenant_isolation(mut self, enabled: bool) -> Self {
        self.tenant_isolation_enabled = enabled;
        self
    }

    /// `true` when tenant isolation is enabled and missing
    /// tenant ids are rejected.
    pub fn tenant_isolation_enabled(&self) -> bool {
        self.tenant_isolation_enabled
    }

    /// Borrow the global / fallback engine.
    pub fn global(&self) -> &PolicyEngine {
        &self.global
    }

    /// Sanitise a tenant id so it can safely form part of a
    /// filename. Replaces every non-`[A-Za-z0-9_]` char with
    /// `_`; an empty / `None` tenant resolves to `"default"`.
    pub fn sanitise_tenant_id(raw: Option<&str>) -> String {
        let s = raw.unwrap_or("default");
        if s.is_empty() {
            return "default".to_string();
        }
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// Path the resolver would load for `tenant_id` (None when
    /// `dir` isn't configured).
    pub fn tenant_path(&self, tenant_id: Option<&str>) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        let sanitised = Self::sanitise_tenant_id(tenant_id);
        Some(dir.join(format!("{sanitised}.policy.toml")))
    }

    /// Resolve the appropriate engine for `tenant_id` and
    /// evaluate the call. Always returns a `Decision` — load
    /// errors are logged via tracing and fall back to the
    /// global engine so a typo in a per-tenant TOML never bricks
    /// the cap.
    ///
    /// PART 4: when [`Self::tenant_isolation_enabled`] is
    /// `true` and `tenant_id` is `None` / empty, the call is
    /// denied with `reason = "tenant_id required in
    /// multi-tenant mode"`. The dispatch bridge maps this
    /// deny reason onto `error_kinds::SECURITY_DENIED` so the
    /// admission failure is visible in audit logs.
    pub fn evaluate(
        &self,
        caller: &VerifiedIdentity,
        method: &str,
        tenant_id: Option<&str>,
    ) -> Decision {
        if self.tenant_isolation_enabled && tenant_id.map(|s| s.trim().is_empty()).unwrap_or(true) {
            return Decision::Deny {
                reason: "tenant_id required in multi-tenant mode".to_string(),
                matched_rule: None,
            };
        }
        if let Some(engine) = self.engine_for_tenant(tenant_id) {
            engine.evaluate(caller, method)
        } else {
            self.global.evaluate(caller, method)
        }
    }

    /// Returns the per-tenant engine if one exists; otherwise
    /// `None` so the caller knows to use the global. Memoised
    /// for `ttl`.
    fn engine_for_tenant(&self, tenant_id: Option<&str>) -> Option<PolicyEngine> {
        let path = self.tenant_path(tenant_id)?;
        let key = path.display().to_string();

        // Cache hit (positive or negative) wins when fresh. A
        // poisoned mutex (another thread panicked while
        // holding the lock) bypasses the cache and re-reads
        // from disk; we never panic an admission check on a
        // background lock issue.
        if !self.ttl.is_zero()
            && let Ok(mut cache) = self.cache.lock()
            && let Some((at, entry)) = cache.get(&key)
            && at.elapsed() < self.ttl
        {
            // LRU `get` bumps recency under the hood; clone
            // the cached engine handle (cheap, Arc-backed)
            // before releasing the lock.
            return entry.clone();
        }

        let loaded = match std::fs::metadata(&path) {
            Ok(_) => match PolicyEngine::from_path(&path) {
                Ok(eng) => Some(eng),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "per-tenant policy file failed to load; falling back to global"
                    );
                    None
                }
            },
            Err(_) => None,
        };

        if !self.ttl.is_zero()
            && let Ok(mut cache) = self.cache.lock()
        {
            // CORR PART 4: LRU `put` evicts the least-recently-
            // used entry once the cap is hit so the resolver's
            // RSS stays bounded even on a many-tenant deployment.
            cache.put(key, (Instant::now(), loaded.clone()));
        }

        loaded
    }

    /// Enumerate tenant ids that have a per-tenant policy file
    /// in `dir`. Sorted; deduped. Returns empty when `dir` is
    /// unset or missing. Pure read; does not warm the cache.
    pub fn list_tenants(&self) -> Vec<String> {
        let Some(dir) = self.dir.as_ref() else {
            return Vec::new();
        };
        let entries = match std::fs::read_dir(dir) {
            Ok(it) => it,
            Err(_) => return Vec::new(),
        };
        let mut out: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                // Match `{tenant_id}.policy.toml` exactly.
                name.strip_suffix(".policy.toml").map(str::to_string)
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// Read the per-tenant policy file's raw TOML text for
    /// operator inspection. Returns `None` when `dir` is unset
    /// or no file exists for `tenant_id`. Does not parse or
    /// cache.
    pub fn tenant_policy_text(&self, tenant_id: &str) -> Option<String> {
        let path = self.tenant_path(Some(tenant_id))?;
        std::fs::read_to_string(&path).ok()
    }

    /// Drop every cached tenant engine. Used by operator tooling
    /// + tests that want a clean reload without waiting for TTL.
    pub fn clear_cache(&self) {
        if let Ok(mut g) = self.cache.lock() {
            g.clear();
        }
    }

    /// Borrow the configured directory (for diagnostics).
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NodeId;

    fn mk_id(name: &str, groups: &[&str]) -> VerifiedIdentity {
        VerifiedIdentity {
            subject_id: NodeId::from_pubkey(name.as_bytes()),
            name: name.into(),
            org_id: NodeId::from_pubkey(b"org"),
            groups: groups.iter().map(|s| s.to_string()).collect(),
            role: "agent".into(),
            clearance: "internal".into(),
            bundle_id: [0u8; 32],
        }
    }

    fn engine_for(text: &str) -> PolicyEngine {
        PolicyEngine::from_toml(text).expect("parse policy")
    }

    #[test]
    fn allowed_group_passes_with_matched_rule() {
        let engine = engine_for(
            r#"
            [[rules]]
            name = "chat_users_chat"
            method = "ai.chat"
            allow_groups = ["chat-users"]
            "#,
        );
        let alice = mk_id("alice", &["chat-users"]);
        match engine.evaluate(&alice, "ai.chat") {
            Decision::Allow { matched_rule } => assert_eq!(matched_rule, "chat_users_chat"),
            d => panic!("expected Allow, got {:?}", d),
        }
    }

    #[test]
    fn missing_group_denied() {
        let engine = engine_for(
            r#"
            [[rules]]
            name = "chat_users_chat"
            method = "ai.chat"
            allow_groups = ["chat-users"]
            "#,
        );
        let bob = mk_id("bob", &["guest"]);
        match engine.evaluate(&bob, "ai.chat") {
            Decision::Deny { matched_rule, .. } => assert!(matched_rule.is_none()),
            d => panic!("expected Deny, got {:?}", d),
        }
    }

    #[test]
    fn unknown_method_default_denied() {
        let engine = engine_for(
            r#"
            [[rules]]
            name = "x"
            method = "ai.chat"
            allow_groups = ["chat-users"]
            "#,
        );
        let alice = mk_id("alice", &["chat-users"]);
        match engine.evaluate(&alice, "ai.unrelated") {
            Decision::Deny { .. } => {}
            d => panic!("expected Deny, got {:?}", d),
        }
    }

    #[test]
    fn admit_section_blocks_off_groups() {
        let engine = engine_for(
            r#"
            [admit]
            groups = ["chat-users"]

            [[rules]]
            name = "ai_for_all_admitted"
            method = "ai.chat"
            allow_groups = ["chat-users"]
            "#,
        );
        let guest = mk_id("guest", &["guest"]);
        match engine.evaluate(&guest, "ai.chat") {
            Decision::Deny {
                matched_rule,
                reason,
            } => {
                assert!(matched_rule.is_none());
                assert!(reason.contains("admit"));
            }
            d => panic!("expected admit-deny, got {:?}", d),
        }
    }

    // ── GAP 23B: TenantPolicyResolver ─────────────────────

    fn write_policy(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("write policy file");
    }

    #[test]
    fn resolver_falls_back_to_global_when_no_tenant_file() {
        // Global file ALLOWS chat-users on ai.chat; no per-tenant
        // file for "acme" exists, so the resolver must fall back
        // to the global engine.
        let tmp = tempfile::tempdir().expect("tempdir");
        let global = engine_for(
            r#"
            [[rules]]
            name = "g_chat"
            method = "ai.chat"
            allow_groups = ["chat-users"]
            "#,
        );
        let resolver = TenantPolicyResolver::new(global, Some(tmp.path().to_path_buf()), 60);
        let alice = mk_id("alice", &["chat-users"]);
        match resolver.evaluate(&alice, "ai.chat", Some("acme")) {
            Decision::Allow { matched_rule } => assert_eq!(matched_rule, "g_chat"),
            d => panic!("expected global Allow, got {:?}", d),
        }
    }

    #[test]
    fn resolver_uses_tenant_specific_file_when_present() {
        // Global engine DENIES ai.chat; tenant "acme" has a
        // per-tenant file that allows it. The tenant-scoped
        // call must Allow.
        let tmp = tempfile::tempdir().expect("tempdir");
        write_policy(
            tmp.path(),
            "acme.policy.toml",
            r#"
            [[rules]]
            name = "acme_chat"
            method = "ai.chat"
            allow_groups = ["chat-users"]
            "#,
        );
        let global = PolicyEngine::permissive();
        let resolver = TenantPolicyResolver::new(global, Some(tmp.path().to_path_buf()), 60);
        let alice = mk_id("alice", &["chat-users"]);

        // Tenant-scoped: hit the per-tenant file.
        match resolver.evaluate(&alice, "ai.chat", Some("acme")) {
            Decision::Allow { matched_rule } => assert_eq!(matched_rule, "acme_chat"),
            d => panic!("expected acme Allow, got {:?}", d),
        }
        // Global call (no tenant): default-deny on permissive.
        match resolver.evaluate(&alice, "ai.chat", None) {
            Decision::Deny { .. } => {}
            d => panic!("expected global Deny, got {:?}", d),
        }
    }

    #[test]
    fn resolver_caches_within_ttl_and_refreshes_after() {
        // Write tenant file; first call loads + caches it.
        // Delete the file; within TTL the cached decision still
        // holds. After clear_cache (equivalent to TTL expiry),
        // the resolver re-reads the disk and falls back to
        // global.
        let tmp = tempfile::tempdir().expect("tempdir");
        write_policy(
            tmp.path(),
            "acme.policy.toml",
            r#"
            [[rules]]
            name = "acme_chat"
            method = "ai.chat"
            allow_groups = ["chat-users"]
            "#,
        );
        let global = PolicyEngine::permissive();
        let resolver = TenantPolicyResolver::new(global, Some(tmp.path().to_path_buf()), 60);
        let alice = mk_id("alice", &["chat-users"]);

        // Prime the cache.
        match resolver.evaluate(&alice, "ai.chat", Some("acme")) {
            Decision::Allow { matched_rule } => assert_eq!(matched_rule, "acme_chat"),
            d => panic!("first allow expected, got {:?}", d),
        }

        // Delete the file. Cache still serves the prior decision.
        std::fs::remove_file(tmp.path().join("acme.policy.toml")).expect("remove tenant file");
        match resolver.evaluate(&alice, "ai.chat", Some("acme")) {
            Decision::Allow { matched_rule } => assert_eq!(matched_rule, "acme_chat"),
            d => panic!("cached allow expected, got {:?}", d),
        }

        // Force TTL expiry; resolver now misses the file and
        // falls back to global (permissive => default-deny per
        // method).
        resolver.clear_cache();
        match resolver.evaluate(&alice, "ai.chat", Some("acme")) {
            Decision::Deny { .. } => {}
            d => panic!("post-expiry deny expected, got {:?}", d),
        }
    }

    #[test]
    fn resolver_sanitises_tenant_id_in_file_lookup() {
        // Traversal-style tenant id must NOT escape `dir`.
        let tmp = tempfile::tempdir().expect("tempdir");
        let resolver = TenantPolicyResolver::new(
            PolicyEngine::permissive(),
            Some(tmp.path().to_path_buf()),
            60,
        );
        let p = resolver
            .tenant_path(Some("../../etc/secrets"))
            .expect("dir set");
        let fname = p.file_name().and_then(|s| s.to_str()).expect("file");
        // Sanitiser turns `/`, `.`, `..` → `_`. The resulting
        // filename must live inside `dir` (parent == tmp.path()).
        assert_eq!(p.parent().expect("parent"), tmp.path());
        assert!(
            !fname.contains("..") && !fname.contains('/') && !fname.contains('\\'),
            "filename {fname:?} should be sanitised"
        );
    }

    #[test]
    fn resolver_lists_tenants_and_reads_text() {
        let tmp = tempfile::tempdir().expect("tempdir");
        write_policy(
            tmp.path(),
            "acme.policy.toml",
            "[[rules]]\nname = \"a\"\nmethod = \"ai.chat\"\n",
        );
        write_policy(
            tmp.path(),
            "globex.policy.toml",
            "[[rules]]\nname = \"b\"\nmethod = \"ai.chat\"\n",
        );
        // Noise: an unrelated TOML must NOT be listed.
        write_policy(tmp.path(), "notes.txt", "ignore me");
        let resolver = TenantPolicyResolver::new(
            PolicyEngine::permissive(),
            Some(tmp.path().to_path_buf()),
            60,
        );
        let tenants = resolver.list_tenants();
        assert_eq!(tenants, vec!["acme".to_string(), "globex".to_string()]);
        let text = resolver
            .tenant_policy_text("acme")
            .expect("acme text present");
        assert!(text.contains("name = \"a\""));
        assert!(resolver.tenant_policy_text("nope").is_none());
    }

    #[test]
    fn permissive_engine_allows_nothing_by_default_deny() {
        // Permissive engine has no rules, so default-deny still applies per-method.
        // Only node-admission is permissive (admits any identity).
        let engine = PolicyEngine::permissive();
        let alice = mk_id("alice", &["anything"]);
        match engine.evaluate(&alice, "ai.chat") {
            Decision::Deny { .. } => {}
            d => panic!("expected default-deny, got {:?}", d),
        }
        assert!(engine.is_permissive());
    }

    /// PART 4: tenant-isolation-enabled resolver MUST deny
    /// calls that arrive without a tenant id, even when the
    /// global engine would have admitted them. The pre-PART-4
    /// silent fallback to the global engine was a critical
    /// isolation bug — a handler that forgot to propagate the
    /// tenant header would have its policy evaluated against
    /// the global file regardless of which tenant the caller
    /// actually belonged to.
    #[test]
    fn fix_part4_resolver_fails_closed_when_tenant_id_missing_in_isolation_mode() {
        // Global engine permits ai.chat for the "chat" group.
        let global = PolicyEngine::from_toml(
            r#"
            [[rules]]
            name = "chat_for_chat_group"
            method = "ai.chat"
            allow_groups = ["chat"]
            "#,
        )
        .expect("policy parses");
        let alice = mk_id("alice", &["chat"]);
        // Without tenant_isolation, missing tenant_id falls
        // through to the global engine and the rule admits.
        let permissive_resolver = TenantPolicyResolver::new(global.clone(), None, 0);
        assert!(matches!(
            permissive_resolver.evaluate(&alice, "ai.chat", None),
            Decision::Allow { .. }
        ));
        // With tenant_isolation = true, missing tenant_id is
        // a hard deny.
        let strict = TenantPolicyResolver::new(global, None, 0).with_tenant_isolation(true);
        match strict.evaluate(&alice, "ai.chat", None) {
            Decision::Deny { reason, .. } => {
                assert!(reason.contains("tenant_id required"));
            }
            d => panic!("expected Deny, got {:?}", d),
        }
        // Empty / whitespace tenant id also denied.
        match strict.evaluate(&alice, "ai.chat", Some("")) {
            Decision::Deny { .. } => {}
            d => panic!("expected Deny on empty, got {:?}", d),
        }
        match strict.evaluate(&alice, "ai.chat", Some("   ")) {
            Decision::Deny { .. } => {}
            d => panic!("expected Deny on whitespace, got {:?}", d),
        }
        // A real tenant id falls through to the global engine
        // (no per-tenant file configured) and the global
        // rule admits.
        assert!(matches!(
            strict.evaluate(&alice, "ai.chat", Some("acme")),
            Decision::Allow { .. }
        ));
    }
}
