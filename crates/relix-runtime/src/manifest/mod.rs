//! Node manifest construction + on-connect exchange (RELIX-5).
//!
//! M1 stub. M5/M6 fills.

use serde::{Deserialize, Serialize};

use relix_core::capability::CapabilityDescriptor;
use relix_core::types::NodeId;

/// Alpha node manifest payload (signed via `relix_core::bundle::Bundle` with
/// `BundleType::NodeManifest`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeManifest {
    /// Node id (peer id).
    pub node_id: NodeId,
    /// Human-readable name.
    pub node_name: String,
    /// Node type discriminator (`memory`, `ai`, `tool`, `web_bridge`, `dev_cli`).
    pub node_type: String,
    /// Manifest version (increment on change).
    pub manifest_version: u64,
    /// Org id (org-root key hash).
    pub org_id: NodeId,
    /// Listen endpoints (e.g., `/ip4/127.0.0.1/tcp/9001`).
    pub endpoints: Vec<String>,
    /// Capabilities served by this node.
    pub capabilities: Vec<CapabilityDescriptor>,
}
