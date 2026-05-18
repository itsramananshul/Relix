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
}
