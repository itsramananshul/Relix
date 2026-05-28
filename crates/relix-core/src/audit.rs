//! Per-responder audit log.
//!
//! Every cross-node RPC produces exactly one audit record on the responder
//! per RELIX-1 §1.2 invariant 5. Records are append-only and hash-chained for
//! tamper evidence. Joinable across nodes by `request_id`.

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use crate::codec::{self, CodecError};
use crate::types::{FlowId, NodeId, RequestId, Timestamp, TraceId};

/// One audit record. Persists on the responder for every inbound RPC.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditRecord {
    /// When the record was emitted.
    pub ts: Timestamp,
    /// Request ID from the RPC envelope (join key across nodes).
    pub request_id: RequestId,
    /// Trace ID (root-trace correlation).
    pub trace_id: TraceId,
    /// Caller's node id (from verified identity).
    pub caller_node_id: NodeId,
    /// Caller's human-readable name (from verified identity).
    pub caller_name: String,
    /// Caller's groups at the time of the call.
    pub caller_groups: Vec<String>,
    /// Responding node id.
    pub responder_node_id: NodeId,
    /// Method invoked.
    pub method: String,
    /// Policy decision: `allow:<rule>` or `deny:<reason>` or `error:<kind>`.
    pub policy_decision: String,
    /// Final outcome status: `ok`, `denied`, `error`.
    pub status: String,
    /// Optional flow id (when the call is part of a SOL flow).
    pub flow_id: Option<FlowId>,
    /// Optional structured error envelope tag (when status=error).
    pub error_kind: Option<u32>,
    /// Latency in ms.
    pub latency_ms: u64,
    /// Chain link to prior record.
    #[serde(with = "serde_bytes")]
    pub prev_hash: [u8; 32],
    /// Signature over the record (excluding `signature` field).
    #[serde(with = "serde_bytes")]
    pub signature: [u8; 64],
}

#[derive(Serialize)]
struct UnsignedAudit<'a> {
    ts: &'a Timestamp,
    request_id: &'a RequestId,
    trace_id: &'a TraceId,
    caller_node_id: &'a NodeId,
    caller_name: &'a String,
    caller_groups: &'a Vec<String>,
    responder_node_id: &'a NodeId,
    method: &'a String,
    policy_decision: &'a String,
    status: &'a String,
    flow_id: &'a Option<FlowId>,
    error_kind: &'a Option<u32>,
    latency_ms: u64,
    #[serde(with = "serde_bytes")]
    prev_hash: &'a [u8; 32],
}

/// Builder pattern for an `AuditRecord` — the responder fills fields as it
/// progresses through the admission pipeline.
#[derive(Clone, Debug)]
pub struct AuditDraft {
    /// Public fields (set during admission).
    pub request_id: RequestId,
    /// Trace id.
    pub trace_id: TraceId,
    /// Caller node id.
    pub caller_node_id: NodeId,
    /// Caller name.
    pub caller_name: String,
    /// Caller groups.
    pub caller_groups: Vec<String>,
    /// Method.
    pub method: String,
    /// Flow id if part of a flow.
    pub flow_id: Option<FlowId>,
    /// Started_at, used to compute latency at finish.
    pub started_at: std::time::Instant,
    /// GAP 23C: caller-supplied tenant id (X-Relix-Tenant
    /// header → RequestEnvelope.tenant_id → here). Recorded on
    /// the partition mirror so operators can slice audit
    /// queries per tenant; NOT copied into the signed
    /// [`AuditRecord`] because changing that struct would
    /// break the existing hash chain. `None` means "no tenant
    /// header supplied" — the partition mirror routes those to
    /// the literal tenant id `"default"`.
    pub tenant_id: Option<String>,
}

/// Append-only audit log writer.
pub struct AuditLog {
    path: PathBuf,
    file: File,
    signer: SigningKey,
    last_hash: [u8; 32],
    responder_node_id: NodeId,
}

impl AuditLog {
    /// Open or create the audit log. Verifies chain on open.
    pub fn open(path: impl AsRef<Path>, signer: SigningKey) -> Result<Self, AuditError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AuditError::Io(e.to_string()))?;
        }
        let responder_node_id = NodeId::from_pubkey(&signer.verifying_key().to_bytes());
        let last_hash = if path.exists() {
            verify_audit_chain(&path, &signer.verifying_key())?
        } else {
            [0u8; 32]
        };
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|e| AuditError::Io(e.to_string()))?;
        Ok(Self {
            path,
            file,
            signer,
            last_hash,
            responder_node_id,
        })
    }

    /// Finalize a draft into a signed, chained, written record.
    pub fn finalize(
        &mut self,
        draft: AuditDraft,
        policy_decision: String,
        status: AuditStatus,
        error_kind: Option<u32>,
    ) -> Result<(), AuditError> {
        let latency_ms = draft.started_at.elapsed().as_millis() as u64;
        let ts = Timestamp::now();
        let status_str = match status {
            AuditStatus::Ok => "ok",
            AuditStatus::Denied => "denied",
            AuditStatus::Error => "error",
        }
        .to_string();

        let unsigned = UnsignedAudit {
            ts: &ts,
            request_id: &draft.request_id,
            trace_id: &draft.trace_id,
            caller_node_id: &draft.caller_node_id,
            caller_name: &draft.caller_name,
            caller_groups: &draft.caller_groups,
            responder_node_id: &self.responder_node_id,
            method: &draft.method,
            policy_decision: &policy_decision,
            status: &status_str,
            flow_id: &draft.flow_id,
            error_kind: &error_kind,
            latency_ms,
            prev_hash: &self.last_hash,
        };
        let to_sign = codec::encode(&unsigned)?;
        let signature = self.signer.sign(&to_sign).to_bytes();

        let rec = AuditRecord {
            ts,
            request_id: draft.request_id,
            trace_id: draft.trace_id,
            caller_node_id: draft.caller_node_id,
            caller_name: draft.caller_name,
            caller_groups: draft.caller_groups,
            responder_node_id: self.responder_node_id,
            method: draft.method,
            policy_decision,
            status: status_str,
            flow_id: draft.flow_id,
            error_kind,
            latency_ms,
            prev_hash: self.last_hash,
            signature,
        };
        let bytes = codec::encode(&rec)?;
        if bytes.len() > u32::MAX as usize {
            return Err(AuditError::TooLarge);
        }
        let len = (bytes.len() as u32).to_be_bytes();
        self.file
            .write_all(&len)
            .map_err(|e| AuditError::Io(e.to_string()))?;
        self.file
            .write_all(&bytes)
            .map_err(|e| AuditError::Io(e.to_string()))?;
        self.file
            .sync_data()
            .map_err(|e| AuditError::Io(e.to_string()))?;
        self.last_hash = codec::content_hash(&bytes);
        Ok(())
    }

    /// Path on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Final outcome of an audited operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditStatus {
    /// Handler completed successfully.
    Ok,
    /// Policy denied the request before handler ran.
    Denied,
    /// Handler returned an error or admission failed.
    Error,
}

/// Read records from an audit log.
pub fn read_audit_records(path: impl AsRef<Path>) -> Result<Vec<AuditRecord>, AuditError> {
    let file = File::open(path.as_ref()).map_err(|e| AuditError::Io(e.to_string()))?;
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    loop {
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(AuditError::Io(e.to_string())),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| AuditError::TornWrite(format!("at record {}: {}", out.len(), e)))?;
        let rec: AuditRecord = codec::decode(&buf)
            .map_err(|e| AuditError::Decode(format!("record {}: {}", out.len(), e)))?;
        out.push(rec);
    }
    Ok(out)
}

/// Verify the hash chain on an audit log.
pub fn verify_audit_chain(
    path: impl AsRef<Path>,
    expected_signer_pubkey: &VerifyingKey,
) -> Result<[u8; 32], AuditError> {
    let records = read_audit_records(path.as_ref())?;
    let mut last_hash = [0u8; 32];
    for rec in &records {
        if rec.prev_hash != last_hash {
            return Err(AuditError::Integrity(format!(
                "chain break at request_id {}",
                rec.request_id
            )));
        }
        let unsigned = UnsignedAudit {
            ts: &rec.ts,
            request_id: &rec.request_id,
            trace_id: &rec.trace_id,
            caller_node_id: &rec.caller_node_id,
            caller_name: &rec.caller_name,
            caller_groups: &rec.caller_groups,
            responder_node_id: &rec.responder_node_id,
            method: &rec.method,
            policy_decision: &rec.policy_decision,
            status: &rec.status,
            flow_id: &rec.flow_id,
            error_kind: &rec.error_kind,
            latency_ms: rec.latency_ms,
            prev_hash: &rec.prev_hash,
        };
        let to_verify = codec::encode(&unsigned)?;
        let sig = ed25519_dalek::Signature::from_bytes(&rec.signature);
        expected_signer_pubkey
            .verify(&to_verify, &sig)
            .map_err(|_| AuditError::Integrity(format!("bad signature for {}", rec.request_id)))?;
        let bytes = codec::encode(rec)?;
        last_hash = codec::content_hash(&bytes);
    }
    Ok(last_hash)
}

/// Audit-layer errors.
#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    /// I/O failure.
    #[error("io: {0}")]
    Io(String),
    /// Codec failure.
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    /// Record larger than u32::MAX bytes.
    #[error("record too large")]
    TooLarge,
    /// Truncated record (likely crash mid-write).
    #[error("torn write: {0}")]
    TornWrite(String),
    /// Decode failure (corruption).
    #[error("decode: {0}")]
    Decode(String),
    /// Chain or signature integrity failure.
    #[error("integrity: {0}")]
    Integrity(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use tempfile::TempDir;

    fn fresh_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn fresh_draft() -> AuditDraft {
        AuditDraft {
            request_id: RequestId::new(),
            trace_id: TraceId::new(),
            caller_node_id: NodeId::from_pubkey(b"alice"),
            caller_name: "alice".into(),
            caller_groups: vec!["chat-users".into()],
            method: "ai.chat".into(),
            flow_id: Some(FlowId::new()),
            started_at: std::time::Instant::now(),
            tenant_id: None,
        }
    }

    #[test]
    fn finalize_and_read_back() {
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("audit.log");
        let key = fresh_key();
        let mut log = AuditLog::open(&path, key.clone()).expect("open");
        log.finalize(
            fresh_draft(),
            "allow:chat_users_chat".into(),
            AuditStatus::Ok,
            None,
        )
        .expect("finalize");
        log.finalize(
            fresh_draft(),
            "deny:no_match".into(),
            AuditStatus::Denied,
            Some(crate::types::error_kinds::POLICY_DENIED),
        )
        .expect("finalize");
        let recs = read_audit_records(&path).expect("read");
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].status, "ok");
        assert_eq!(recs[1].status, "denied");
        assert_eq!(
            recs[1].error_kind,
            Some(crate::types::error_kinds::POLICY_DENIED)
        );
    }

    #[test]
    fn audit_chain_verifies() {
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("audit.log");
        let key = fresh_key();
        {
            let mut log = AuditLog::open(&path, key.clone()).expect("open");
            log.finalize(fresh_draft(), "allow:x".into(), AuditStatus::Ok, None)
                .expect("a");
            log.finalize(fresh_draft(), "deny:y".into(), AuditStatus::Denied, None)
                .expect("b");
        }
        verify_audit_chain(&path, &key.verifying_key()).expect("verify");
    }
}
