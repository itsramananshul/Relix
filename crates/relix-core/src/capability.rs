//! Capability descriptors — alpha-simplified RELIX-6.

use serde::{Deserialize, Serialize};

/// What the capability does on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    /// Single request, single response.
    Unary,
    /// Server-sent stream (e.g. AI token stream).
    StreamOut,
}

/// Idempotency class per RELIX-1 §1.8.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Idempotency {
    /// Safe to retry; responder de-dupes via `idem`.
    Idempotent,
    /// MUST NOT retry on `responder_internal`.
    AtMostOnce,
    /// Caller may retry; responder caches recent results.
    AtLeastOnceSafe,
}

/// Cost class per RELIX-6 §6.11 (hint for budgeting and rate-limiting).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostClass {
    /// Sub-ms typical latency.
    Cheap,
    /// Tens to hundreds of ms.
    Expensive,
    /// Invokes a paid external service.
    ExternalPaid,
}

/// PH-CAP-RISK: operator-facing risk classification for a
/// capability. Honest worst-case-impact label, not a strict
/// formal model — the goal is to give operators a one-glance
/// sense of which capabilities deserve scrutiny in their
/// policy + dashboard surfaces.
///
/// `Unknown` is the serde default so a descriptor that hasn't
/// been audited surfaces as a clear gap rather than implicitly
/// claiming `Safe`. The validator (in `relix-cli capability
/// validate`) flags `Unknown` as a deployment warning so the
/// gap is caught before production.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Descriptor has not been audited for risk. Validator
    /// flags this; treat as "investigate before deploying".
    #[default]
    Unknown,
    /// Read-only / pure observation. No persistent state change,
    /// no external side effects, no privilege escalation.
    /// Example: `tool.read_file`, `tool.list_dir`,
    /// `tool.fs.audit_recent`, `node.health`.
    Safe,
    /// Bounded internal state change. Mutates registry-internal
    /// state (todos, session records, audit rings) but does NOT
    /// touch the host file system, network, or external
    /// processes. Cancellable. Example: `task.todo_set`,
    /// `tool.terminal.cancel`, `tool.terminal.shell.close`.
    Low,
    /// Controlled side effect outside the responder. Writes to
    /// the host file system (jailed), reaches external network
    /// endpoints (SSRF-guarded), or invokes paid services.
    /// Example: `tool.write_file`, `tool.web_fetch`, `ai.chat`,
    /// `tool.web_search`.
    Medium,
    /// Spawns external processes / drives an external execution
    /// surface. Allowlisted but potentially destructive.
    /// Example: `tool.terminal.run`, `tool.terminal.spawn`,
    /// `tool.terminal.shell.open`, `tool.mcp.invoke`,
    /// `tool.browser.navigate`.
    High,
    /// Reserved for capabilities that act on the host outside
    /// the existing allowlist / jail model (e.g. desktop
    /// control, raw network egress without SSRF guards). No
    /// shipped capability uses this tier today; included so
    /// the surface is forward-compatible.
    Critical,
}

/// Alpha capability descriptor. Reduced subset of RELIX-6 §6.4.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapabilityDescriptor {
    /// Fully-qualified method name (e.g., `memory.search`).
    pub method_name: String,
    /// Major version (callers pin this).
    pub major_version: u32,
    /// Kind.
    pub kind: CapabilityKind,
    /// Idempotency class.
    pub idempotency: Idempotency,
    /// Cost class.
    pub cost_class: CostClass,
    /// Sensitivity tags (free-form, policy-referenceable).
    #[serde(default)]
    pub sensitivity_tags: Vec<String>,
    /// Stable policy attachment point identifier (defaults to method_name).
    pub policy_attachment_point: String,
    /// Minimum-claim groups required to call (structural pre-filter; policy still applies).
    /// SIMP for alpha — full credential-claims structure at Gate 2.
    #[serde(default)]
    pub requires_groups: Vec<String>,

    // ── T4 P1: planner/operator advisory metadata ─────────────────────
    //
    // All three of these are OPTIONAL with serde defaults. Existing
    // serialised manifests decode unchanged. No runtime semantics
    // changes — these are projector/operator-facing only.
    //
    /// Short human-readable description (one sentence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Free-form categories: `fetch`, `parse`, `summarise`,
    /// `persist`, `notify`, etc. A planner can narrow by category
    /// before pattern-matching on sensitivity tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub categories: Vec<String>,
    /// What the responder needs at runtime to provide this
    /// capability (e.g. `["network:outbound", "api_key:openai"]`).
    /// Honest declaration; operators reference these in policy and
    /// during deployment planning. Not validated at runtime.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment_requirements: Vec<String>,
    /// PH-CAP-RISK: operator-facing risk classification.
    /// Defaults to `Unknown` so unaudited descriptors are
    /// visible. The validator flags `Unknown` as a deployment
    /// warning. See [`RiskLevel`] for the full taxonomy.
    #[serde(default)]
    pub risk_level: RiskLevel,
}

impl CapabilityDescriptor {
    /// Convenience constructor for the alpha capabilities.
    pub fn unary(method: impl Into<String>) -> Self {
        let m = method.into();
        Self {
            policy_attachment_point: m.clone(),
            method_name: m,
            major_version: 1,
            kind: CapabilityKind::Unary,
            idempotency: Idempotency::Idempotent,
            cost_class: CostClass::Cheap,
            sensitivity_tags: vec![],
            requires_groups: vec![],
            description: None,
            categories: vec![],
            environment_requirements: vec![],
            risk_level: RiskLevel::Unknown,
        }
    }

    /// Convenience constructor for a streaming-out capability (e.g. AI chat).
    pub fn stream_out(method: impl Into<String>) -> Self {
        let m = method.into();
        Self {
            policy_attachment_point: m.clone(),
            method_name: m,
            major_version: 1,
            kind: CapabilityKind::StreamOut,
            idempotency: Idempotency::AtMostOnce,
            cost_class: CostClass::ExternalPaid,
            sensitivity_tags: vec!["external:network".into()],
            requires_groups: vec![],
            description: None,
            categories: vec![],
            environment_requirements: vec![],
            risk_level: RiskLevel::Unknown,
        }
    }

    /// Annotate with sensitivity tags.
    pub fn with_sensitivity(mut self, tags: impl IntoIterator<Item = String>) -> Self {
        self.sensitivity_tags.extend(tags);
        self
    }

    /// Annotate with required groups (structural pre-filter).
    pub fn with_groups(mut self, groups: impl IntoIterator<Item = String>) -> Self {
        self.requires_groups.extend(groups);
        self
    }

    /// Attach a one-sentence human description (T4 P1).
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Attach planner-facing categories (T4 P1). Examples:
    /// `["fetch"]`, `["parse"]`, `["persist"]`, `["notify"]`,
    /// `["mutate", "fs"]`.
    pub fn with_categories(mut self, cats: impl IntoIterator<Item = String>) -> Self {
        self.categories.extend(cats);
        self
    }

    /// Declare runtime environment requirements (T4 P1). Honest
    /// metadata; not enforced by the runtime. Operators reference
    /// these when deciding deployment.
    pub fn with_environment_requirements(mut self, reqs: impl IntoIterator<Item = String>) -> Self {
        self.environment_requirements.extend(reqs);
        self
    }

    /// PH-CAP-RISK: set the operator-facing risk classification.
    /// Defaults to `Unknown` from the constructor; every shipped
    /// descriptor should call this to set an explicit tier.
    pub fn with_risk(mut self, risk: RiskLevel) -> Self {
        self.risk_level = risk;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unary_descriptor_roundtrips() {
        let d = CapabilityDescriptor::unary("memory.search")
            .with_sensitivity(["reads:internal".into()]);
        let bytes = crate::codec::encode(&d).expect("encode");
        let back: CapabilityDescriptor = crate::codec::decode(&bytes).expect("decode");
        assert_eq!(d.method_name, back.method_name);
        assert_eq!(d.kind, back.kind);
        assert_eq!(back.sensitivity_tags, vec!["reads:internal".to_string()]);
    }

    // ── T4 P1: planner/operator advisory metadata ─────────────────────

    #[test]
    fn new_optional_fields_default_to_empty() {
        let d = CapabilityDescriptor::unary("x.y");
        assert!(d.description.is_none());
        assert!(d.categories.is_empty());
        assert!(d.environment_requirements.is_empty());
    }

    #[test]
    fn description_categories_envreqs_roundtrip() {
        let d = CapabilityDescriptor::unary("tool.web_fetch")
            .with_description("Fetch a URL safely with SSRF + DNS pin guards.")
            .with_categories(["fetch".into(), "io".into()])
            .with_environment_requirements(["network:outbound".into()]);
        let bytes = crate::codec::encode(&d).expect("encode");
        let back: CapabilityDescriptor = crate::codec::decode(&bytes).expect("decode");
        assert_eq!(
            back.description.as_deref(),
            Some("Fetch a URL safely with SSRF + DNS pin guards.")
        );
        assert_eq!(back.categories, vec!["fetch".to_string(), "io".into()]);
        assert_eq!(
            back.environment_requirements,
            vec!["network:outbound".to_string()]
        );
    }

    #[test]
    fn descriptor_from_pre_p1_serialised_bytes_still_decodes() {
        // Simulate an older manifest that doesn't include the new
        // fields. The descriptor decodes; the new fields default to
        // empty. This is the load-bearing serde-default contract.
        use ciborium::Value;
        let mut map = vec![
            (
                Value::Text("method_name".into()),
                Value::Text("memory.search".into()),
            ),
            (
                Value::Text("major_version".into()),
                Value::Integer(1u8.into()),
            ),
            (Value::Text("kind".into()), Value::Text("unary".into())),
            (
                Value::Text("idempotency".into()),
                Value::Text("idempotent".into()),
            ),
            (
                Value::Text("cost_class".into()),
                Value::Text("cheap".into()),
            ),
            (
                Value::Text("policy_attachment_point".into()),
                Value::Text("memory.search".into()),
            ),
        ];
        // Include some optional fields from the C1 era too.
        map.push((Value::Text("sensitivity_tags".into()), Value::Array(vec![])));
        map.push((Value::Text("requires_groups".into()), Value::Array(vec![])));
        let mut bytes = Vec::new();
        ciborium::into_writer(&Value::Map(map), &mut bytes).expect("encode");
        let back: CapabilityDescriptor =
            ciborium::from_reader(&bytes[..]).expect("decode pre-P1 manifest");
        assert_eq!(back.method_name, "memory.search");
        // New fields defaulted.
        assert!(back.description.is_none());
        assert!(back.categories.is_empty());
        assert!(back.environment_requirements.is_empty());
    }

    #[test]
    fn description_none_does_not_appear_in_serialised_form() {
        // serde(skip_serializing_if = "Option::is_none") + empty
        // Vec skipping means a descriptor with no new fields encodes
        // identically to one from before P1. (Important: keeps the
        // wire payload from bloating across the network.)
        let d = CapabilityDescriptor::unary("x.y");
        let bytes = crate::codec::encode(&d).expect("encode");
        // Round-trip through ciborium::Value so we can inspect keys.
        let v: ciborium::Value = ciborium::from_reader(&bytes[..]).expect("decode");
        let map = match v {
            ciborium::Value::Map(m) => m,
            other => panic!("expected map, got {other:?}"),
        };
        let keys: Vec<String> = map
            .iter()
            .filter_map(|(k, _)| match k {
                ciborium::Value::Text(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert!(
            !keys.iter().any(|k| k == "description"),
            "description was emitted despite None: keys = {keys:?}"
        );
        assert!(
            !keys.iter().any(|k| k == "categories"),
            "empty categories were emitted: keys = {keys:?}"
        );
        assert!(
            !keys.iter().any(|k| k == "environment_requirements"),
            "empty environment_requirements were emitted: keys = {keys:?}"
        );
    }

    // ── PH-CAP-RISK: risk_level field ──────────────────────────────

    #[test]
    fn risk_level_default_is_unknown() {
        let d = CapabilityDescriptor::unary("x.y");
        assert_eq!(d.risk_level, RiskLevel::Unknown);
    }

    #[test]
    fn risk_level_default_via_derive() {
        assert_eq!(RiskLevel::default(), RiskLevel::Unknown);
    }

    #[test]
    fn with_risk_builder_sets_field() {
        let d = CapabilityDescriptor::unary("tool.terminal.run").with_risk(RiskLevel::High);
        assert_eq!(d.risk_level, RiskLevel::High);
    }

    #[test]
    fn risk_level_round_trips_through_codec() {
        for r in [
            RiskLevel::Unknown,
            RiskLevel::Safe,
            RiskLevel::Low,
            RiskLevel::Medium,
            RiskLevel::High,
            RiskLevel::Critical,
        ] {
            let d = CapabilityDescriptor::unary("x.y").with_risk(r);
            let bytes = crate::codec::encode(&d).expect("encode");
            let back: CapabilityDescriptor = crate::codec::decode(&bytes).expect("decode");
            assert_eq!(back.risk_level, r, "round-trip mismatch for {r:?}");
        }
    }

    #[test]
    fn risk_level_serializes_snake_case_in_wire_form() {
        // The wire form should use snake_case spellings of the
        // enum variants, matching the rest of the descriptor
        // surface (CapabilityKind, Idempotency, CostClass).
        let d = CapabilityDescriptor::unary("x.y").with_risk(RiskLevel::Medium);
        let bytes = crate::codec::encode(&d).expect("encode");
        let v: ciborium::Value = ciborium::from_reader(&bytes[..]).expect("decode");
        let map = match v {
            ciborium::Value::Map(m) => m,
            other => panic!("expected map, got {other:?}"),
        };
        // Find the risk_level entry and confirm it serialises as
        // `"medium"`, not `"Medium"`.
        let risk = map
            .iter()
            .find_map(|(k, vv)| match k {
                ciborium::Value::Text(s) if s == "risk_level" => Some(vv.clone()),
                _ => None,
            })
            .expect("risk_level present in encoded form");
        match risk {
            ciborium::Value::Text(s) => assert_eq!(s, "medium"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn descriptor_from_pre_risk_serialised_bytes_still_decodes() {
        // Simulate an older manifest (pre-PH-CAP-RISK) that
        // doesn't include risk_level. Should decode with
        // risk_level defaulting to Unknown.
        use ciborium::Value;
        let map = vec![
            (
                Value::Text("method_name".into()),
                Value::Text("memory.search".into()),
            ),
            (
                Value::Text("major_version".into()),
                Value::Integer(1u8.into()),
            ),
            (Value::Text("kind".into()), Value::Text("unary".into())),
            (
                Value::Text("idempotency".into()),
                Value::Text("idempotent".into()),
            ),
            (
                Value::Text("cost_class".into()),
                Value::Text("cheap".into()),
            ),
            (
                Value::Text("policy_attachment_point".into()),
                Value::Text("memory.search".into()),
            ),
            (Value::Text("sensitivity_tags".into()), Value::Array(vec![])),
            (Value::Text("requires_groups".into()), Value::Array(vec![])),
        ];
        let mut bytes = Vec::new();
        ciborium::into_writer(&Value::Map(map), &mut bytes).expect("encode");
        let back: CapabilityDescriptor =
            ciborium::from_reader(&bytes[..]).expect("decode pre-RISK manifest");
        assert_eq!(back.risk_level, RiskLevel::Unknown);
    }
}
