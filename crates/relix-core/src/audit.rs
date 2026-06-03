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
        // Make the resolved backing path explicit. It is derived from
        // RELIX_DATA_DIR / node name in the controller, so logging it
        // here keeps boot from *silently* depending on a file that may
        // live outside the wiped run directory.
        tracing::info!(
            audit_path = %path.display(),
            responder = %responder_node_id,
            "audit log: opening"
        );
        let last_hash = if path.exists() {
            match verify_audit_chain(&path, &signer.verifying_key()) {
                Ok(h) => h,
                Err(AuditError::Integrity(reason)) => {
                    // The existing chain does not verify under THIS
                    // responder's key. Two very different causes, handled
                    // differently:
                    //
                    //   * The faulting record carries a *different*
                    //     `responder_node_id` — a previous key after
                    //     rotation, or a log left behind by another node.
                    //     The current key legitimately cannot verify it.
                    //     Bricking boot here is wrong (the node could never
                    //     start again after a key rotation), so quarantine
                    //     the file and start a fresh chain.
                    //
                    //   * The faulting record claims THIS responder but
                    //     still does not verify — tampering or corruption
                    //     of our own chain. That MUST stay a hard failure
                    //     and is never silently discarded.
                    match first_fault_responder(&path, &signer.verifying_key())? {
                        Some(claimed) if claimed != responder_node_id => {
                            let quarantined = quarantine_audit_log(&path)?;
                            tracing::error!(
                                audit_path = %path.display(),
                                quarantined_to = %quarantined.display(),
                                claimed_responder = %claimed,
                                current_responder = %responder_node_id,
                                reason = %reason,
                                "audit log was written by a DIFFERENT responder identity \
                                 (key rotation or a foreign/stale log) and cannot verify \
                                 under the current key; quarantined the old file and \
                                 started a fresh chain. Review and remove the quarantined \
                                 file once archived."
                            );
                            [0u8; 32]
                        }
                        _ => {
                            // Current-responder tamper/corruption, or the
                            // re-scan unexpectedly found no fault: fail
                            // closed to preserve tamper detection.
                            return Err(AuditError::Integrity(reason));
                        }
                    }
                }
                Err(e) => return Err(e),
            }
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
        // SEC PART 6: `as_millis()` returns u128; cast via
        // `try_from` so a 584-million-year request saturates
        // to u64::MAX instead of silently truncating.
        let latency_ms = u64::try_from(draft.started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
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

/// Re-scan a log and return the `responder_node_id` of the first
/// record that fails the chain or signature check under
/// `expected_signer_pubkey`, or `None` if the whole log verifies.
///
/// Mirrors [`verify_audit_chain`]'s checks exactly so the two never
/// disagree about *where* a chain first breaks. Used by
/// [`AuditLog::open`] to tell a benign key rotation / foreign log
/// (faulting record belongs to a different responder identity) apart
/// from tampering of a record that claims the current responder.
fn first_fault_responder(
    path: impl AsRef<Path>,
    expected_signer_pubkey: &VerifyingKey,
) -> Result<Option<NodeId>, AuditError> {
    let records = read_audit_records(path.as_ref())?;
    let mut last_hash = [0u8; 32];
    for rec in &records {
        if rec.prev_hash != last_hash {
            return Ok(Some(rec.responder_node_id));
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
        if expected_signer_pubkey.verify(&to_verify, &sig).is_err() {
            return Ok(Some(rec.responder_node_id));
        }
        let bytes = codec::encode(rec)?;
        last_hash = codec::content_hash(&bytes);
    }
    Ok(None)
}

/// Move an unverifiable audit log aside (preserving its bytes) so a
/// fresh chain can start in its place. Returns the quarantine path.
fn quarantine_audit_log(path: impl AsRef<Path>) -> Result<PathBuf, AuditError> {
    let path = path.as_ref();
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("audit.log");
    let mut target = path.with_file_name(format!("{file_name}.quarantined-{millis}"));
    // Never clobber an existing quarantine file (e.g. two opens in the
    // same millisecond).
    let mut n: u32 = 0;
    while target.exists() {
        n += 1;
        target = path.with_file_name(format!("{file_name}.quarantined-{millis}-{n}"));
    }
    std::fs::rename(path, &target).map_err(|e| AuditError::Io(e.to_string()))?;
    Ok(target)
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

    /// Criterion 4: a freshly signed entry verifies under the same key.
    /// Proves sign and verify agree on the canonical bytes (rules out a
    /// sign/verify mismatch — "case 2").
    #[test]
    fn sign_then_verify_round_trips() {
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("audit.log");
        let key = fresh_key();
        {
            let mut log = AuditLog::open(&path, key.clone()).expect("open");
            log.finalize(fresh_draft(), "allow:x".into(), AuditStatus::Ok, None)
                .expect("finalize");
        }
        // The written record verifies cleanly under the signer's key.
        verify_audit_chain(&path, &key.verifying_key()).expect("round-trip verify");
        // And a clean re-open (which re-runs verification) succeeds.
        AuditLog::open(&path, key.clone()).expect("reopen verifies");
    }

    /// Criterion 5: a tampered entry still fails verification. We rewrite
    /// a record's `method` field without re-signing, so the signature no
    /// longer matches the canonical bytes.
    #[test]
    fn tampered_record_fails_verification() {
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("audit.log");
        let key = fresh_key();
        {
            let mut log = AuditLog::open(&path, key.clone()).expect("open");
            log.finalize(fresh_draft(), "allow:x".into(), AuditStatus::Ok, None)
                .expect("finalize");
        }
        // Read the single record, mutate a signed field, keep the old
        // signature + responder_node_id, and rewrite the file.
        let mut recs = read_audit_records(&path).expect("read");
        assert_eq!(recs.len(), 1);
        recs[0].method = "tool.terminal".into(); // was "ai.chat"
        rewrite_log(&path, &recs);
        let err = verify_audit_chain(&path, &key.verifying_key())
            .expect_err("tampered record must fail verification");
        assert!(
            matches!(err, AuditError::Integrity(_)),
            "expected Integrity error, got {err:?}"
        );
    }

    /// New behaviour: a log written by a *different* responder key (key
    /// rotation, or a stale/foreign log) does not brick boot — it is
    /// quarantined aside and a fresh chain starts in its place.
    #[test]
    fn rotated_key_log_is_quarantined_and_boot_recovers() {
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("audit.log");
        let old_key = fresh_key();
        {
            let mut log = AuditLog::open(&path, old_key.clone()).expect("open old");
            log.finalize(fresh_draft(), "allow:x".into(), AuditStatus::Ok, None)
                .expect("finalize old");
        }
        let original_bytes = std::fs::read(&path).expect("read original");

        // Open with a DIFFERENT key — simulates the regenerated node key.
        let new_key = fresh_key();
        assert_ne!(
            old_key.verifying_key().to_bytes(),
            new_key.verifying_key().to_bytes()
        );
        let mut log = AuditLog::open(&path, new_key.clone())
            .expect("open must recover from a foreign/rotated-key log");

        // The fresh chain starts empty and accepts new records.
        log.finalize(fresh_draft(), "allow:y".into(), AuditStatus::Ok, None)
            .expect("finalize on fresh chain");
        let recs = read_audit_records(&path).expect("read fresh");
        assert_eq!(recs.len(), 1, "fresh chain has exactly the new record");
        verify_audit_chain(&path, &new_key.verifying_key()).expect("fresh chain verifies");

        // The old log was preserved (renamed), not destroyed.
        let quarantined: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("audit.log.quarantined-")
            })
            .collect();
        assert_eq!(quarantined.len(), 1, "exactly one quarantine file");
        assert_eq!(
            std::fs::read(quarantined[0].path()).expect("read quarantine"),
            original_bytes,
            "quarantine preserves the original bytes verbatim"
        );
    }

    /// Security: a record claiming the CURRENT responder that fails its
    /// signature is tampering and MUST hard-fail on open — never
    /// quarantined-and-recovered. This guards the recovery path from
    /// becoming a tamper-evasion hole.
    #[test]
    fn current_responder_tamper_hard_fails_open() {
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("audit.log");
        let key = fresh_key();
        {
            let mut log = AuditLog::open(&path, key.clone()).expect("open");
            log.finalize(fresh_draft(), "allow:x".into(), AuditStatus::Ok, None)
                .expect("finalize");
        }
        // Tamper a signed field but leave responder_node_id (== current
        // key's node id) and the now-stale signature in place.
        let mut recs = read_audit_records(&path).expect("read");
        recs[0].method = "tool.terminal".into();
        rewrite_log(&path, &recs);

        match AuditLog::open(&path, key.clone()) {
            Err(AuditError::Integrity(_)) => {}
            Ok(_) => panic!("tamper of a current-responder record must hard-fail open"),
            Err(other) => panic!("expected Integrity (hard fail), got {other:?}"),
        }
        // The tampered file was NOT quarantined away.
        assert!(path.exists(), "tampered current-key log must not be moved");
        let still: Vec<_> = std::fs::read_dir(dir.path())
            .expect("readdir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("quarantined-"))
            .collect();
        assert!(still.is_empty(), "must not quarantine a current-key tamper");
    }

    /// Test helper: overwrite the log with `recs` using the same
    /// length-prefixed framing as [`AuditLog::finalize`].
    fn rewrite_log(path: &std::path::Path, recs: &[AuditRecord]) {
        use std::io::Write;
        let mut f = std::fs::File::create(path).expect("create");
        for rec in recs {
            let bytes = codec::encode(rec).expect("encode");
            f.write_all(&(bytes.len() as u32).to_be_bytes())
                .expect("len");
            f.write_all(&bytes).expect("bytes");
        }
        f.sync_data().expect("sync");
    }
}
