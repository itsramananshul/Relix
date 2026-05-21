//! `relix-cli capability ...` — inspect peer capability manifests.
//!
//! Read-only operator surface (T4 P3). Each subcommand dials one
//! peer over libp2p, invokes the standard `node.manifest`
//! capability through the full admission pipeline (identity →
//! policy → handler → audit), and prints the manifest. No
//! orchestration; pure projection of mesh state.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Subcommand;

use relix_core::bundle::Bundle;
use relix_core::capability::{
    CapabilityDescriptor, CapabilityKind, CostClass, Idempotency, RiskLevel,
};
use relix_core::codec;
use relix_runtime::dispatch::{build_request, decode_response};
use relix_runtime::manifest::NodeManifest;
use relix_runtime::transport::envelope::ResponseResult;
use relix_runtime::transport::rpc::{self, Event, Multiaddr};

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// List every capability the peer advertises. One line per
    /// capability with the headline fields.
    Ls {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        /// Filter by category (e.g. `fetch`, `parse`).
        #[arg(long, default_value = "")]
        category: String,
        /// Filter by sensitivity tag (e.g. `external:network`).
        #[arg(long, default_value = "")]
        tag: String,
    },
    /// Show one capability descriptor in detail.
    Get {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
        #[arg(long)]
        method: String,
    },
    /// Validate a peer's manifest against the plugin-foundations
    /// rules. Reports each issue on stderr; exits non-zero when
    /// any rule fires. Use in CI / pre-deployment checks.
    ///
    /// Rules checked:
    ///  - non-empty `method_name`
    ///  - `method_name` follows `<namespace>.<action>` convention
    ///    (must contain `.`, non-empty parts on each side)
    ///  - non-empty `policy_attachment_point`
    ///  - no duplicate `method_name` across descriptors
    ///  - sensitivity_tags follow `<namespace>:<tag>` form when
    ///    present (e.g. `fs:read`, `external:network`)
    ///  - environment_requirements follow `<key>:<value>` form
    ///    when present
    ///  - `requires_groups` non-empty (every capability must be
    ///    policy-gated to at least one group)
    Validate {
        #[arg(long)]
        peer: String,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        client_key: PathBuf,
    },
}

pub async fn run(cmd: Cmd) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        Cmd::Ls {
            peer,
            identity,
            client_key,
            category,
            tag,
        } => {
            let manifest = fetch_manifest(&peer, &identity, &client_key).await?;
            println!(
                "{}  {}  ({} caps)",
                manifest.node_type,
                manifest.node_id,
                manifest.capabilities.len()
            );
            let mut shown = 0;
            for cap in &manifest.capabilities {
                if !category.is_empty() && !cap.categories.iter().any(|c| c == &category) {
                    continue;
                }
                if !tag.is_empty() && !cap.sensitivity_tags.iter().any(|t| t == &tag) {
                    continue;
                }
                let summary = render_oneline(cap);
                println!("  {summary}");
                shown += 1;
            }
            if shown == 0 {
                let filter_note = match (category.is_empty(), tag.is_empty()) {
                    (true, true) => "(none)".to_string(),
                    (false, true) => format!("category={category}"),
                    (true, false) => format!("tag={tag}"),
                    (false, false) => format!("category={category} tag={tag}"),
                };
                println!("  (no capabilities match {filter_note})");
            }
        }
        Cmd::Validate {
            peer,
            identity,
            client_key,
        } => {
            let manifest = fetch_manifest(&peer, &identity, &client_key).await?;
            let issues = validate_manifest(&manifest);
            if issues.is_empty() {
                println!(
                    "{} {} — {} capabilities — OK",
                    manifest.node_type,
                    manifest.node_id,
                    manifest.capabilities.len()
                );
            } else {
                eprintln!(
                    "{} {} — {} capabilities — {} issue(s):",
                    manifest.node_type,
                    manifest.node_id,
                    manifest.capabilities.len(),
                    issues.len()
                );
                for issue in &issues {
                    eprintln!("  - {issue}");
                }
                std::process::exit(2);
            }
        }
        Cmd::Get {
            peer,
            identity,
            client_key,
            method,
        } => {
            let manifest = fetch_manifest(&peer, &identity, &client_key).await?;
            let Some(cap) = manifest
                .capabilities
                .iter()
                .find(|c| c.method_name == method)
            else {
                eprintln!(
                    "no capability '{method}' on peer (advertised: {})",
                    manifest
                        .capabilities
                        .iter()
                        .map(|c| c.method_name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                std::process::exit(2);
            };
            print!("{}", render_detail(&manifest, cap));
        }
    }
    Ok(())
}

/// Validate a peer's manifest against the plugin-foundations
/// rules in `docs/plugin-foundations.md` (M-series constraints).
/// Returns one human-readable issue per rule fired; empty Vec
/// when the manifest is clean.
///
/// Rules are advisory at the wire level today — the Coordinator
/// does not reject malformed descriptors at registration. This
/// linter is the operator-visible enforcement seam: pre-deploy,
/// CI, or ad-hoc inspection.
fn validate_manifest(manifest: &NodeManifest) -> Vec<String> {
    let mut issues = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for cap in &manifest.capabilities {
        if cap.method_name.trim().is_empty() {
            issues.push("descriptor with empty method_name".into());
            continue;
        }
        if !seen.insert(cap.method_name.as_str()) {
            issues.push(format!(
                "duplicate method_name `{}` — multiple descriptors register the same wire name",
                cap.method_name
            ));
        }
        // Convention: method_name MUST be `<namespace>.<action>`
        // with non-empty parts on each side of the first dot.
        // Every shipped capability follows this (task.*, node.*,
        // memory.*, ai.*, tool.*); the rule catches descriptors
        // that would land in `/v1/capabilities?category=...`
        // filters with no namespace, breaking discovery.
        if let Some((ns, action)) = cap.method_name.split_once('.') {
            if ns.is_empty() || action.is_empty() {
                issues.push(format!(
                    "`{}`: method_name must be `<namespace>.<action>` with non-empty parts on each side of the dot",
                    cap.method_name
                ));
            }
        } else {
            issues.push(format!(
                "`{}`: method_name has no `.` — convention is `<namespace>.<action>` (e.g. `task.list`)",
                cap.method_name
            ));
        }
        if cap.policy_attachment_point.trim().is_empty() {
            issues.push(format!(
                "`{}`: policy_attachment_point is empty — policy authors can't reference this method",
                cap.method_name
            ));
        }
        if cap.requires_groups.is_empty() {
            issues.push(format!(
                "`{}`: requires_groups is empty — capability is ungated, every identity can call it",
                cap.method_name
            ));
        }
        for tag in &cap.sensitivity_tags {
            if !tag.contains(':') {
                issues.push(format!(
                    "`{}`: sensitivity tag `{tag}` is missing the `<namespace>:<value>` form",
                    cap.method_name
                ));
            }
        }
        for req in &cap.environment_requirements {
            if !req.contains(':') {
                issues.push(format!(
                    "`{}`: environment requirement `{req}` is missing the `<key>:<value>` form",
                    cap.method_name
                ));
            }
        }
        // PH-CAP-RISK: flag any descriptor that hasn't been
        // audited for risk classification. Operators see this
        // in CI / pre-deploy and can either set an explicit
        // tier or document the deferral.
        if cap.risk_level == RiskLevel::Unknown {
            issues.push(format!(
                "`{}`: risk_level is `unknown` — set an explicit tier via .with_risk(...) before deploying",
                cap.method_name
            ));
        }
    }
    issues
}

fn render_oneline(cap: &CapabilityDescriptor) -> String {
    let mut s = format!(
        "{:<28}  v{}  {}  {}  {}  {}",
        cap.method_name,
        cap.major_version,
        kind_label(cap.kind),
        idempotency_label(cap.idempotency),
        cost_class_label(cap.cost_class),
        risk_level_label(cap.risk_level),
    );
    if !cap.categories.is_empty() {
        s.push_str(&format!("  [{}]", cap.categories.join(",")));
    }
    s
}

fn render_detail(manifest: &NodeManifest, cap: &CapabilityDescriptor) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "method:          {}", cap.method_name);
    let _ = writeln!(s, "major_version:   {}", cap.major_version);
    let _ = writeln!(s, "kind:            {}", kind_label(cap.kind));
    let _ = writeln!(s, "idempotency:     {}", idempotency_label(cap.idempotency));
    let _ = writeln!(s, "cost_class:      {}", cost_class_label(cap.cost_class));
    let _ = writeln!(s, "risk_level:      {}", risk_level_label(cap.risk_level));
    let _ = writeln!(s, "policy_attach:   {}", cap.policy_attachment_point);
    if !cap.sensitivity_tags.is_empty() {
        let _ = writeln!(s, "sensitivity:     {}", cap.sensitivity_tags.join(", "));
    }
    if !cap.requires_groups.is_empty() {
        let _ = writeln!(s, "requires_groups: {}", cap.requires_groups.join(", "));
    }
    if let Some(d) = cap.description.as_deref() {
        let _ = writeln!(s, "description:     {d}");
    }
    if !cap.categories.is_empty() {
        let _ = writeln!(s, "categories:      {}", cap.categories.join(", "));
    }
    if !cap.environment_requirements.is_empty() {
        let _ = writeln!(
            s,
            "environment:     {}",
            cap.environment_requirements.join(", ")
        );
    }
    let _ = writeln!(s);
    let _ = writeln!(s, "served_by:");
    let _ = writeln!(s, "  node_id:   {}", manifest.node_id);
    let _ = writeln!(s, "  node_name: {}", manifest.node_name);
    let _ = writeln!(s, "  node_type: {}", manifest.node_type);
    s
}

fn kind_label(k: CapabilityKind) -> &'static str {
    match k {
        CapabilityKind::Unary => "unary",
        CapabilityKind::StreamOut => "stream",
    }
}

fn idempotency_label(i: Idempotency) -> &'static str {
    match i {
        Idempotency::Idempotent => "idempotent",
        Idempotency::AtMostOnce => "at-most-once",
        Idempotency::AtLeastOnceSafe => "at-least-once",
    }
}

fn cost_class_label(c: CostClass) -> &'static str {
    match c {
        CostClass::Cheap => "cheap",
        CostClass::Expensive => "expensive",
        CostClass::ExternalPaid => "paid",
    }
}

fn risk_level_label(r: RiskLevel) -> &'static str {
    match r {
        RiskLevel::Unknown => "unknown",
        RiskLevel::Safe => "safe",
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
        RiskLevel::Critical => "critical",
    }
}

/// Dial, present identity, invoke `node.manifest`, decode the
/// returned NodeManifest. Same dial-and-call pattern as
/// `task::call`; refactoring into a shared helper is a separate
/// follow-up.
async fn fetch_manifest(
    peer_addr: &str,
    identity_bundle_path: &Path,
    client_key_path: &Path,
) -> Result<NodeManifest, Box<dyn std::error::Error>> {
    let bundle_bytes = std::fs::read(identity_bundle_path)?;
    let bundle: Bundle = codec::decode(&bundle_bytes)?;

    let key_bytes = std::fs::read(client_key_path)?;
    if key_bytes.len() != 32 {
        return Err("client key must be 32 raw bytes".into());
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&key_bytes);

    let port = 20_000 + (rand::random::<u16>() % 10_000);
    let (client, mut events, event_loop) = rpc::new(key, port).await?;
    tokio::spawn(event_loop.run());

    let addr: Multiaddr = peer_addr
        .parse()
        .map_err(|e| format!("parse multiaddr '{peer_addr}': {e:?}"))?;
    client
        .dial(addr.clone())
        .await
        .map_err(|e| format!("dial: {e}"))?;

    let connected = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(Event::PeerConnected { peer_id, .. }) = events.recv().await {
                return Some(peer_id);
            }
        }
    })
    .await
    .ok()
    .flatten()
    .ok_or("timeout waiting for peer connection")?;

    let envelope = build_request("node.manifest", Vec::new(), bundle, 10);
    let resp_bytes = client
        .call(connected, envelope)
        .await
        .map_err(|e| format!("rpc: {e}"))?;
    let resp = decode_response(&resp_bytes)?;
    let body = match resp.res {
        ResponseResult::Ok(b) => b.to_vec(),
        ResponseResult::Err(e) => {
            eprintln!("ERR kind={} cause={}", e.kind, e.cause);
            std::process::exit(2);
        }
        ResponseResult::StreamHandle(_) => {
            return Err("unexpected stream response from node.manifest".into());
        }
    };
    let manifest: NodeManifest = codec::decode(&body)?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(method: &str) -> CapabilityDescriptor {
        let mut d = CapabilityDescriptor::unary(method);
        d.categories = vec!["parse".into()];
        d.sensitivity_tags = vec!["parse:html".into()];
        d.description = Some("Test capability".into());
        d
    }

    #[test]
    fn render_oneline_includes_categories_when_present() {
        let c = cap("tool.web_extract");
        let s = render_oneline(&c);
        assert!(s.contains("tool.web_extract"));
        assert!(s.contains("unary"));
        assert!(s.contains("idempotent"));
        assert!(s.contains("cheap"));
        assert!(s.contains("[parse]"));
    }

    #[test]
    fn render_oneline_omits_categories_when_absent() {
        let mut c = cap("ai.chat");
        c.categories = vec![];
        let s = render_oneline(&c);
        assert!(!s.contains("[]"));
    }

    fn mk_manifest(node_type: &str) -> NodeManifest {
        let id = relix_core::types::NodeId::from_pubkey(b"x");
        NodeManifest {
            node_id: id,
            node_name: "test".into(),
            node_type: node_type.into(),
            manifest_version: 1,
            org_id: id,
            endpoints: vec![],
            capabilities: vec![],
        }
    }

    #[test]
    fn render_detail_emits_all_advisory_fields_when_set() {
        let mut manifest = mk_manifest("tool");
        manifest.node_name = "tool".into();
        let mut c = cap("tool.web_fetch");
        c.environment_requirements = vec!["network:outbound".into()];
        manifest.capabilities.push(c.clone());
        let s = render_detail(&manifest, &c);
        assert!(s.contains("method:          tool.web_fetch"));
        assert!(s.contains("description:     Test capability"));
        assert!(s.contains("categories:      parse"));
        assert!(s.contains("environment:     network:outbound"));
        assert!(s.contains("served_by:"));
    }

    #[test]
    fn render_detail_omits_optional_fields_when_unset() {
        let manifest = mk_manifest("memory");
        let mut c = CapabilityDescriptor::unary("memory.search");
        c.sensitivity_tags = vec!["reads:internal".into()];
        let s = render_detail(&manifest, &c);
        assert!(s.contains("method:          memory.search"));
        assert!(s.contains("sensitivity:     reads:internal"));
        // Absent fields should NOT appear at all (not even as
        // empty values).
        assert!(!s.contains("description:"));
        assert!(!s.contains("categories:"));
        assert!(!s.contains("environment:"));
    }

    fn cap_ok(method: &str) -> CapabilityDescriptor {
        let mut d = CapabilityDescriptor::unary(method);
        d.requires_groups = vec!["chat-users".into()];
        d.sensitivity_tags = vec!["reads:internal".into()];
        d.environment_requirements = vec!["fs:jail".into()];
        // PH-CAP-RISK: explicit tier so the validator's
        // unknown-risk check doesn't fire in tests that
        // exercise OTHER rules.
        d.risk_level = RiskLevel::Safe;
        d
    }

    #[test]
    fn validate_clean_manifest_returns_no_issues() {
        let mut manifest = mk_manifest("memory");
        manifest.capabilities.push(cap_ok("memory.search"));
        manifest.capabilities.push(cap_ok("memory.write_turn"));
        assert!(validate_manifest(&manifest).is_empty());
    }

    #[test]
    fn validate_flags_duplicate_method_names() {
        let mut manifest = mk_manifest("memory");
        manifest.capabilities.push(cap_ok("memory.search"));
        manifest.capabilities.push(cap_ok("memory.search"));
        let issues = validate_manifest(&manifest);
        assert!(
            issues.iter().any(|s| s.contains("duplicate method_name")),
            "issues = {issues:?}"
        );
    }

    #[test]
    fn validate_flags_empty_method_name() {
        let mut manifest = mk_manifest("memory");
        manifest.capabilities.push(CapabilityDescriptor::unary(""));
        let issues = validate_manifest(&manifest);
        assert!(issues.iter().any(|s| s.contains("empty method_name")));
    }

    #[test]
    fn validate_flags_missing_requires_groups() {
        let mut manifest = mk_manifest("memory");
        let mut c = CapabilityDescriptor::unary("memory.search");
        c.policy_attachment_point = "memory.search".into();
        // intentionally NO requires_groups
        manifest.capabilities.push(c);
        let issues = validate_manifest(&manifest);
        assert!(
            issues
                .iter()
                .any(|s| s.contains("requires_groups is empty")),
            "issues = {issues:?}"
        );
    }

    #[test]
    fn validate_flags_malformed_sensitivity_tags() {
        let mut manifest = mk_manifest("memory");
        let mut c = cap_ok("memory.search");
        c.sensitivity_tags = vec!["reads_internal".into()]; // missing colon
        manifest.capabilities.push(c);
        let issues = validate_manifest(&manifest);
        assert!(
            issues
                .iter()
                .any(|s| s.contains("sensitivity tag") && s.contains("missing")),
            "issues = {issues:?}"
        );
    }

    #[test]
    fn validate_flags_malformed_environment_requirements() {
        let mut manifest = mk_manifest("memory");
        let mut c = cap_ok("memory.search");
        c.environment_requirements = vec!["fs_jail".into()];
        manifest.capabilities.push(c);
        let issues = validate_manifest(&manifest);
        assert!(
            issues.iter().any(|s| s.contains("environment requirement")),
            "issues = {issues:?}"
        );
    }

    #[test]
    fn validate_empty_policy_attachment_point_caught() {
        let mut manifest = mk_manifest("memory");
        let mut c = cap_ok("memory.search");
        c.policy_attachment_point = String::new();
        manifest.capabilities.push(c);
        let issues = validate_manifest(&manifest);
        assert!(
            issues
                .iter()
                .any(|s| s.contains("policy_attachment_point is empty")),
            "issues = {issues:?}"
        );
    }

    #[test]
    fn validate_flags_method_name_without_dot() {
        let mut manifest = mk_manifest("memory");
        let mut c = cap_ok("memorysearch"); // no dot
        c.policy_attachment_point = "memorysearch".into();
        manifest.capabilities.push(c);
        let issues = validate_manifest(&manifest);
        assert!(
            issues
                .iter()
                .any(|s| s.contains("no `.`") || s.contains("`<namespace>.<action>`")),
            "issues = {issues:?}"
        );
    }

    #[test]
    fn validate_flags_method_name_with_empty_namespace() {
        let mut manifest = mk_manifest("memory");
        let mut c = cap_ok(".search"); // empty namespace
        c.policy_attachment_point = ".search".into();
        manifest.capabilities.push(c);
        let issues = validate_manifest(&manifest);
        assert!(
            issues.iter().any(|s| s.contains("non-empty parts")),
            "issues = {issues:?}"
        );
    }

    #[test]
    fn validate_flags_method_name_with_empty_action() {
        let mut manifest = mk_manifest("memory");
        let mut c = cap_ok("memory."); // empty action
        c.policy_attachment_point = "memory.".into();
        manifest.capabilities.push(c);
        let issues = validate_manifest(&manifest);
        assert!(
            issues.iter().any(|s| s.contains("non-empty parts")),
            "issues = {issues:?}"
        );
    }

    #[test]
    fn validate_accepts_multi_dot_method_names() {
        // `memory.recent_for_session` is one of the actually-shipped
        // names — the rule only checks the FIRST dot has non-empty
        // parts on each side. Underscores in the action are fine;
        // future namespaces like `tool.web.fetch` would also be
        // accepted (ns=tool, action=web.fetch).
        let mut manifest = mk_manifest("memory");
        manifest
            .capabilities
            .push(cap_ok("memory.recent_for_session"));
        manifest.capabilities.push(cap_ok("tool.web.fetch"));
        assert!(validate_manifest(&manifest).is_empty());
    }

    #[test]
    fn enum_labels_are_stable() {
        assert_eq!(kind_label(CapabilityKind::Unary), "unary");
        assert_eq!(kind_label(CapabilityKind::StreamOut), "stream");
        assert_eq!(idempotency_label(Idempotency::Idempotent), "idempotent");
        assert_eq!(idempotency_label(Idempotency::AtMostOnce), "at-most-once");
        assert_eq!(
            idempotency_label(Idempotency::AtLeastOnceSafe),
            "at-least-once"
        );
        assert_eq!(cost_class_label(CostClass::Cheap), "cheap");
        assert_eq!(cost_class_label(CostClass::Expensive), "expensive");
        assert_eq!(cost_class_label(CostClass::ExternalPaid), "paid");
    }

    // ── PH-CAP-RISK: risk_level surface ──────────────────────────

    #[test]
    fn risk_level_labels_are_stable() {
        assert_eq!(risk_level_label(RiskLevel::Unknown), "unknown");
        assert_eq!(risk_level_label(RiskLevel::Safe), "safe");
        assert_eq!(risk_level_label(RiskLevel::Low), "low");
        assert_eq!(risk_level_label(RiskLevel::Medium), "medium");
        assert_eq!(risk_level_label(RiskLevel::High), "high");
        assert_eq!(risk_level_label(RiskLevel::Critical), "critical");
    }

    #[test]
    fn validate_flags_unknown_risk_level() {
        // A descriptor with the unaudited default risk_level
        // MUST be flagged so operators see the gap.
        let mut manifest = mk_manifest("tool");
        let mut c = CapabilityDescriptor::unary("tool.example");
        c.requires_groups = vec!["chat-users".into()];
        c.sensitivity_tags = vec!["fs:read".into()];
        c.policy_attachment_point = "tool.example".into();
        // Leave risk_level at default Unknown.
        manifest.capabilities.push(c);
        let issues = validate_manifest(&manifest);
        assert!(
            issues.iter().any(|s| s.contains("risk_level is `unknown`")),
            "issues = {issues:?}"
        );
    }

    #[test]
    fn validate_accepts_explicit_risk_levels() {
        // Each non-Unknown tier must pass the validator's
        // unknown-risk check.
        for tier in [
            RiskLevel::Safe,
            RiskLevel::Low,
            RiskLevel::Medium,
            RiskLevel::High,
            RiskLevel::Critical,
        ] {
            let mut manifest = mk_manifest("tool");
            let mut c = cap_ok("tool.example");
            c.risk_level = tier;
            manifest.capabilities.push(c);
            let issues = validate_manifest(&manifest);
            assert!(
                !issues.iter().any(|s| s.contains("risk_level is `unknown`")),
                "tier {tier:?} flagged unexpectedly: {issues:?}"
            );
        }
    }

    #[test]
    fn render_oneline_includes_risk_label() {
        let mut c = cap("tool.terminal.run");
        c.risk_level = RiskLevel::High;
        let s = render_oneline(&c);
        assert!(s.contains("high"), "rendered: {s}");
    }

    #[test]
    fn render_detail_includes_risk_line() {
        let mut manifest = mk_manifest("tool");
        let mut c = cap("tool.write_file");
        c.risk_level = RiskLevel::Medium;
        manifest.capabilities.push(c.clone());
        let s = render_detail(&manifest, &c);
        assert!(s.contains("risk_level:      medium"), "rendered: {s}");
    }
}
