//! Per-flow append-only signed hash-chained event log per RELIX-3.
//!
//! ## DETERMINISM
//!
//! Event records are encoded via [`codec::encode`]. The hash chain links events
//! deterministically: each record's `prev_hash` = BLAKE3-256 of the prior
//! record's full encoded bytes (including signature). Tampering any byte breaks
//! the chain on next read.
//!
//! ## Log-Before-Act
//!
//! The append operation fsyncs before returning; callers MUST `append` an
//! `RemoteCallIssued` event before issuing the corresponding RPC, and call
//! `append` for `RemoteCallCompleted` after observing the response. This
//! invariant is the caller's responsibility — `relix-runtime::coordinator`
//! enforces it.

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};

use crate::codec::{self, CodecError};
use crate::types::{FlowId, NodeId, Timestamp};

/// Event types per RELIX-3 §3.4. Alpha subset.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    /// Flow was created; payload describes the trigger.
    FlowStarted,
    /// An outbound RPC was issued.
    RemoteCallIssued,
    /// An outbound RPC completed (success or error).
    RemoteCallCompleted,
    /// A stream chunk was received by the flow.
    StreamChunkReceived,
    /// Flow reached `Completed` terminal state.
    FlowCompleted,
    /// Flow reached `Failed` terminal state.
    FlowFailed,
}

/// Event record per RELIX-3 §3.3. Signed and hash-chained.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventRecord {
    /// Flow this event belongs to.
    pub flow_id: FlowId,
    /// Monotonic sequence number within the flow.
    pub event_seq: u64,
    /// Wall-clock at write time. For ordering/ops only — NOT consumed by replay.
    pub ts: Timestamp,
    /// Event discriminator.
    pub kind: EventType,
    /// Type-specific payload (CBOR-encoded).
    pub payload: ByteBuf,
    /// BLAKE3-256 of prior record's encoded bytes (zeros for seq 0).
    #[serde(with = "serde_bytes")]
    pub prev_hash: [u8; 32],
    /// Owning controller's Ed25519 signature over encoded(record_without_sig).
    #[serde(with = "serde_bytes")]
    pub signature: [u8; 64],
}

/// Same shape as [`EventRecord`] minus the signature, used for signature input.
#[derive(Serialize)]
struct UnsignedRecord<'a> {
    flow_id: &'a FlowId,
    event_seq: u64,
    ts: &'a Timestamp,
    kind: &'a EventType,
    payload: &'a ByteBuf,
    #[serde(with = "serde_bytes")]
    prev_hash: &'a [u8; 32],
}

/// Append-only signed log for a single flow.
///
/// Wire format on disk: a concatenation of `length-prefixed CBOR records`. Each
/// record is preceded by a 4-byte big-endian length (the number of bytes of the
/// following CBOR encoding). This makes recovery from a torn write tractable.
pub struct EventLog {
    flow_id: FlowId,
    path: PathBuf,
    file: File,
    signer: SigningKey,
    next_seq: u64,
    last_hash: [u8; 32],
    owner_node_id: NodeId,
}

impl EventLog {
    /// Open or create a flow log. Loads existing records if present and verifies
    /// chain integrity on open.
    pub fn open(
        path: impl AsRef<Path>,
        flow_id: FlowId,
        signer: SigningKey,
    ) -> Result<Self, EventLogError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| EventLogError::Io(e.to_string()))?;
        }

        let owner_node_id = NodeId::from_pubkey(&signer.verifying_key().to_bytes());

        // Verify-and-replay any existing records to compute `next_seq` and `last_hash`.
        let (next_seq, last_hash) = if path.exists() {
            verify_chain(&path, &signer.verifying_key())?
        } else {
            (0, [0u8; 32])
        };

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(|e| EventLogError::Io(e.to_string()))?;

        Ok(Self {
            flow_id,
            path,
            file,
            signer,
            next_seq,
            last_hash,
            owner_node_id,
        })
    }

    /// Append an event. Computes hash chain, signs, fsyncs, returns the new event_seq.
    ///
    /// LOG-BEFORE-ACT: caller MUST invoke this BEFORE the side effect the event
    /// records. After this returns Ok, the write is durable on disk.
    pub fn append(&mut self, kind: EventType, payload: Vec<u8>) -> Result<u64, EventLogError> {
        let seq = self.next_seq;
        let ts = Timestamp::now();
        let payload = ByteBuf::from(payload);

        let unsigned = UnsignedRecord {
            flow_id: &self.flow_id,
            event_seq: seq,
            ts: &ts,
            kind: &kind,
            payload: &payload,
            prev_hash: &self.last_hash,
        };
        let to_sign = codec::encode(&unsigned)?;
        let signature = self.signer.sign(&to_sign).to_bytes();

        let record = EventRecord {
            flow_id: self.flow_id,
            event_seq: seq,
            ts,
            kind,
            payload,
            prev_hash: self.last_hash,
            signature,
        };

        let record_bytes = codec::encode(&record)?;
        if record_bytes.len() > u32::MAX as usize {
            return Err(EventLogError::TooLarge);
        }
        let len = (record_bytes.len() as u32).to_be_bytes();

        self.file
            .write_all(&len)
            .map_err(|e| EventLogError::Io(e.to_string()))?;
        self.file
            .write_all(&record_bytes)
            .map_err(|e| EventLogError::Io(e.to_string()))?;
        self.file
            .sync_data()
            .map_err(|e| EventLogError::Io(e.to_string()))?;

        self.last_hash = codec::content_hash(&record_bytes);
        self.next_seq = seq + 1;
        Ok(seq)
    }

    /// Owning node id (= BLAKE3 of signing pubkey).
    pub fn owner_node_id(&self) -> NodeId {
        self.owner_node_id
    }

    /// Current `next_seq`. Useful for tests and `relix-flow-inspect`.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Path on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read all records from a flow log file. Used by `relix-flow-inspect`.
pub fn read_records(path: impl AsRef<Path>) -> Result<Vec<EventRecord>, EventLogError> {
    let file = File::open(path.as_ref()).map_err(|e| EventLogError::Io(e.to_string()))?;
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    loop {
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(EventLogError::Io(e.to_string())),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| EventLogError::TornWrite(format!("at record {}: {}", out.len(), e)))?;
        let rec: EventRecord = codec::decode(&buf)
            .map_err(|e| EventLogError::Decode(format!("record {}: {}", out.len(), e)))?;
        out.push(rec);
    }
    Ok(out)
}

/// Verify the chain integrity of an existing log. Returns `(next_seq, last_hash)`.
///
/// Verifies: monotonic seq, hash-chain linkage, signature on each record.
pub fn verify_chain(
    path: impl AsRef<Path>,
    expected_signer_pubkey: &VerifyingKey,
) -> Result<(u64, [u8; 32]), EventLogError> {
    let records = read_records(path.as_ref())?;
    let mut last_hash = [0u8; 32];
    let mut expected_seq = 0u64;
    let mut last_record_bytes: Vec<u8> = Vec::new();
    for rec in &records {
        if rec.event_seq != expected_seq {
            return Err(EventLogError::Integrity(format!(
                "expected seq {}, got {}",
                expected_seq, rec.event_seq
            )));
        }
        if rec.prev_hash != last_hash {
            return Err(EventLogError::Integrity(format!(
                "chain break at seq {}",
                rec.event_seq
            )));
        }

        // Verify signature.
        let unsigned = UnsignedRecord {
            flow_id: &rec.flow_id,
            event_seq: rec.event_seq,
            ts: &rec.ts,
            kind: &rec.kind,
            payload: &rec.payload,
            prev_hash: &rec.prev_hash,
        };
        let to_verify = codec::encode(&unsigned)?;
        let sig = ed25519_dalek::Signature::from_bytes(&rec.signature);
        expected_signer_pubkey
            .verify(&to_verify, &sig)
            .map_err(|_| {
                EventLogError::Integrity(format!("bad signature at seq {}", rec.event_seq))
            })?;

        last_record_bytes = codec::encode(rec)?;
        last_hash = codec::content_hash(&last_record_bytes);
        expected_seq += 1;
    }
    let _ = last_record_bytes; // suppress unused-var if zero records
    Ok((expected_seq, last_hash))
}

/// Event-log errors.
#[derive(Debug, thiserror::Error)]
pub enum EventLogError {
    /// I/O failure.
    #[error("io: {0}")]
    Io(String),
    /// Codec failure.
    #[error("codec: {0}")]
    Codec(#[from] CodecError),
    /// Record larger than u32::MAX bytes.
    #[error("record too large")]
    TooLarge,
    /// Truncated record at end of file (likely crash mid-write).
    #[error("torn write: {0}")]
    TornWrite(String),
    /// Record failed to decode (corruption mid-file).
    #[error("decode: {0}")]
    Decode(String),
    /// Integrity check failure (hash chain or signature).
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

    #[test]
    fn append_and_read_back() {
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("flow.log");
        let flow = FlowId::new();
        let key = fresh_key();
        let mut log = EventLog::open(&path, flow, key.clone()).expect("open");

        let s0 = log
            .append(EventType::FlowStarted, b"trigger=test".to_vec())
            .expect("append start");
        let s1 = log
            .append(
                EventType::RemoteCallIssued,
                b"method=memory.search".to_vec(),
            )
            .expect("append issued");
        let s2 = log
            .append(EventType::RemoteCallCompleted, b"ok".to_vec())
            .expect("append completed");
        assert_eq!(s0, 0);
        assert_eq!(s1, 1);
        assert_eq!(s2, 2);

        let recs = read_records(&path).expect("read");
        assert_eq!(recs.len(), 3);
        assert_eq!(recs[0].kind, EventType::FlowStarted);
        assert_eq!(recs[1].kind, EventType::RemoteCallIssued);
        assert_eq!(recs[2].kind, EventType::RemoteCallCompleted);
    }

    #[test]
    fn chain_verifies_after_append() {
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("flow.log");
        let flow = FlowId::new();
        let key = fresh_key();
        {
            let mut log = EventLog::open(&path, flow, key.clone()).expect("open");
            log.append(EventType::FlowStarted, b"x".to_vec())
                .expect("a1");
            log.append(EventType::FlowCompleted, b"y".to_vec())
                .expect("a2");
        }
        let (next_seq, _last_hash) = verify_chain(&path, &key.verifying_key()).expect("verify");
        assert_eq!(next_seq, 2);
    }

    #[test]
    fn tampering_payload_breaks_chain() {
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("flow.log");
        let flow = FlowId::new();
        let key = fresh_key();
        {
            let mut log = EventLog::open(&path, flow, key.clone()).expect("open");
            log.append(EventType::FlowStarted, b"original".to_vec())
                .expect("a1");
            log.append(EventType::FlowCompleted, b"y".to_vec())
                .expect("a2");
        }
        // Flip one byte deep in the file.
        let mut buf = std::fs::read(&path).expect("read");
        let mid = buf.len() / 2;
        buf[mid] ^= 0xFF;
        std::fs::write(&path, &buf).expect("write");

        let err = verify_chain(&path, &key.verifying_key()).expect_err("must fail");
        // Either signature mismatch or decode error — both are integrity-related.
        match err {
            EventLogError::Integrity(_)
            | EventLogError::Decode(_)
            | EventLogError::TornWrite(_) => {}
            other => panic!("unexpected error kind: {other:?}"),
        }
    }

    #[test]
    fn reopen_resumes_sequence() {
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("flow.log");
        let flow = FlowId::new();
        let key = fresh_key();
        {
            let mut log = EventLog::open(&path, flow, key.clone()).expect("open1");
            log.append(EventType::FlowStarted, b"a".to_vec())
                .expect("a");
        }
        let mut log2 = EventLog::open(&path, flow, key.clone()).expect("open2");
        assert_eq!(log2.next_seq(), 1);
        let s = log2
            .append(EventType::FlowCompleted, b"b".to_vec())
            .expect("a2");
        assert_eq!(s, 1);
        assert_eq!(log2.next_seq(), 2);
    }
}
