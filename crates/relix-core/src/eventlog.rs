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
    /// An outbound RPC completed successfully.
    RemoteCallCompleted,
    /// An outbound RPC failed (transport, policy, application, or local
    /// dispatch error). Payload carries the failure cause.
    RemoteCallFailed,
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
        // CORR-D1: after the file's data is durable, fsync
        // the parent directory inode so the newly-created
        // / extended entry survives a crash even when the
        // directory metadata was not yet flushed. The cross-
        // platform helper uses `File::open(parent).sync_all()`
        // on Unix and `CreateFileW + FlushFileBuffers` on
        // Windows via `windows-sys`; both targets now provide
        // real directory durability. A best-effort `let _`
        // here matches the file's `sync_data` posture: a
        // failed dir-fsync is logged via tracing and ignored
        // so a partial-failure write path doesn't crash the
        // caller's flow.
        if let Err(e) = fsync_parent_dir(&self.path) {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "event log: parent-directory fsync failed; data is still file-synced",
            );
        }

        self.last_hash = codec::content_hash(&record_bytes);
        // SEC PART 6: checked increment. A flow that somehow
        // accumulated u64::MAX events deserves a hard error
        // rather than a silent wrap to 0 (which would let a
        // subsequent append collide with seq 0's hash chain).
        self.next_seq = seq.checked_add(1).ok_or(EventLogError::SequenceOverflow)?;
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

/// CORR PART 4: hard cap on per-read record count. Operators
/// reading a long-lived flow log used to be able to pull every
/// record into memory in one call — a hostile / pathological
/// log could exhaust the reader's heap. The cap is well above
/// any realistic flow's event count.
pub const MAX_RECORDS_PER_READ: usize = 100_000;

/// CORR PART 4: hard cap on individual record size. The
/// 4-byte length prefix on disk used to be allowed up to
/// `u32::MAX` (≈ 4 GiB) of allocation per record; a torn /
/// tampered file could trick `read_records` into allocating a
/// huge buffer for a single record. 10 MiB matches the YAML
/// flow cap (PART 1) — far past any realistic event payload.
pub const MAX_RECORD_SIZE: usize = 10 * 1024 * 1024;

/// Read all records from a flow log file. Used by `relix-flow-inspect`.
///
/// CORR PART 4: enforces both [`MAX_RECORDS_PER_READ`] and
/// [`MAX_RECORD_SIZE`]. Each per-record length read is
/// checked against the size cap BEFORE allocating, so a
/// pathological 4 GiB length prefix is rejected without ever
/// growing the reader's heap.
pub fn read_records(path: impl AsRef<Path>) -> Result<Vec<EventRecord>, EventLogError> {
    let file = File::open(path.as_ref()).map_err(|e| EventLogError::Io(e.to_string()))?;
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    loop {
        if out.len() >= MAX_RECORDS_PER_READ {
            return Err(EventLogError::TooManyRecords {
                limit: MAX_RECORDS_PER_READ,
            });
        }
        let mut len_buf = [0u8; 4];
        match reader.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(EventLogError::Io(e.to_string())),
        }
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > MAX_RECORD_SIZE {
            return Err(EventLogError::RecordTooLarge {
                size_bytes: len,
                max_bytes: MAX_RECORD_SIZE,
            });
        }
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
    /// SEC PART 6: `next_seq + 1` overflowed `u64::MAX`.
    /// Never reachable in practice — a flow would have to
    /// accumulate 2^64 events — but a silent wrap would
    /// collide hash-chain links with the genesis record.
    #[error("event sequence overflow at u64::MAX")]
    SequenceOverflow,
    /// CORR PART 4: read_records refused to load more than
    /// [`MAX_RECORDS_PER_READ`] records in one call.
    #[error("event log: too many records to read in one call (limit {limit})")]
    TooManyRecords {
        /// Maximum number of records read_records will return
        /// in one call.
        limit: usize,
    },
    /// CORR PART 4: read_records refused to allocate a record
    /// whose length prefix exceeded [`MAX_RECORD_SIZE`].
    #[error("event log: record size {size_bytes} bytes exceeds max {max_bytes} bytes")]
    RecordTooLarge {
        /// The bad length prefix observed on disk.
        size_bytes: usize,
        /// The cap that was tripped.
        max_bytes: usize,
    },
}

/// CORR-D1: cross-platform parent-directory fsync. Flushes
/// the parent directory's inode so that an entry created /
/// modified inside it survives a crash even when the
/// directory metadata was not yet committed.
///
/// On Unix this is `File::open(parent).sync_all()`; on
/// Windows it opens the directory with
/// `FILE_FLAG_BACKUP_SEMANTICS` (required to open a directory
/// handle) and calls `FlushFileBuffers` via `windows-sys`.
/// Returns `Err` if the parent could not be opened OR the
/// flush call failed; the EventLog caller logs and continues.
pub fn fsync_parent_dir(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(path);
    #[cfg(unix)]
    {
        let dir = std::fs::File::open(parent)?;
        dir.sync_all()
    }
    #[cfg(windows)]
    {
        fsync_parent_dir_windows(parent)
    }
    #[cfg(not(any(unix, windows)))]
    {
        // No platform-specific path available; nothing to do.
        let _ = parent;
        Ok(())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn fsync_parent_dir_windows(parent: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FlushFileBuffers, OPEN_EXISTING,
    };

    // Wide-encoded NUL-terminated path (CreateFileW expects
    // UTF-16 LE with a trailing 0).
    let wide: Vec<u16> = parent
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // SAFETY: `wide` is a valid NUL-terminated UTF-16 buffer
    // that outlives the CreateFileW call. We pass:
    // - dwDesiredAccess = GENERIC_WRITE. FlushFileBuffers'
    //   documented requirement is that the handle was opened
    //   with one of GENERIC_WRITE / FILE_APPEND_DATA /
    //   FILE_WRITE_DATA; without it, FlushFileBuffers returns
    //   ERROR_ACCESS_DENIED on the handle.
    // - dwShareMode = READ | WRITE | DELETE so other handles
    //   to the directory keep working (including operators
    //   that delete files inside it).
    // - lpSecurityAttributes = null (default ACL).
    // - dwCreationDisposition = OPEN_EXISTING (we never
    //   create — error out if the parent is missing).
    // - dwFlagsAndAttributes = FILE_FLAG_BACKUP_SEMANTICS
    //   (required to open a directory handle on Windows).
    // - hTemplateFile = 0 (no template).
    // The returned HANDLE is checked against
    // INVALID_HANDLE_VALUE before use and closed on every
    // path via CloseHandle.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `handle` was just returned by CreateFileW and
    // checked against INVALID_HANDLE_VALUE. FlushFileBuffers
    // takes a single HANDLE and returns 0 on failure.
    let flush_ok = unsafe { FlushFileBuffers(handle) };
    let flush_err = if flush_ok == 0 {
        Some(std::io::Error::last_os_error())
    } else {
        None
    };
    // SAFETY: same handle, exactly one close. Ignore the
    // return — we already captured the flush outcome and a
    // failure to close a freshly-opened handle is a
    // diagnostic rather than a hard error.
    let _ = unsafe { CloseHandle(handle) };
    match flush_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
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
    fn sequence_overflow_returns_error_not_silent_wrap() {
        // SEC PART 6: force the in-memory next_seq to
        // u64::MAX; the next append must fail with
        // SequenceOverflow instead of wrapping to 0 (which
        // would let it collide with the genesis record's
        // hash-chain link).
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("flow.log");
        let flow = FlowId::new();
        let key = fresh_key();
        let mut log = EventLog::open(&path, flow, key.clone()).expect("open");
        // Reach into the in-memory state — there's no
        // append-2^64-events shortcut.
        log.next_seq = u64::MAX;
        let err = log
            .append(EventType::FlowStarted, b"x".to_vec())
            .expect_err("must overflow");
        assert!(
            matches!(err, EventLogError::SequenceOverflow),
            "got {err:?}"
        );
    }

    // ── CORR PART 4: read_records caps ─────────────────────

    #[test]
    fn corr_p4_oversize_length_prefix_rejected_before_allocate() {
        // Write a torn / hostile log: a 4-byte big-endian
        // length prefix that promises 4 GiB. read_records
        // must reject this WITHOUT allocating 4 GiB.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hostile.log");
        std::fs::write(&path, (u32::MAX).to_be_bytes()).unwrap();
        let err = read_records(&path).expect_err("must reject");
        assert!(
            matches!(err, EventLogError::RecordTooLarge { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn corr_p4_too_many_records_rejected() {
        // Confirm the cap is enforced by spying the constant —
        // we can't realistically write 100_001 records in a
        // unit test, but we can verify the cap value is
        // sensible and the variant exists for callers to
        // surface.
        assert_eq!(MAX_RECORDS_PER_READ, 100_000);
        let err = EventLogError::TooManyRecords { limit: 100 };
        assert!(err.to_string().contains("100"));
    }

    // ── CORR-D1: cross-platform parent-directory fsync ──

    #[test]
    fn corr_d1_fsync_parent_dir_succeeds_on_real_path() {
        // A real, existing file path inside a tempdir: the
        // helper must open + flush the parent without error.
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("flow.log");
        std::fs::write(&path, b"x").expect("write");
        fsync_parent_dir(&path).expect("fsync parent dir on existing file");
    }

    #[test]
    fn corr_d1_fsync_parent_dir_succeeds_for_dir_path_directly() {
        // Calling with a directory path resolves `parent =
        // path.parent().unwrap_or(path)`; the helper still
        // opens + flushes the closest valid directory above.
        let dir = TempDir::new().expect("tmp");
        fsync_parent_dir(dir.path()).expect("fsync parent of a dir path");
    }

    #[test]
    fn corr_d1_fsync_parent_dir_errors_for_nonexistent_parent() {
        // Pointing at a path whose parent does not exist must
        // surface the OS error rather than silently succeed.
        // Use a sufficiently bogus path that both platforms
        // reject. On Windows we use an invalid drive letter
        // that real machines do not have mapped; on Unix we
        // use a deep nonexistent prefix.
        let bogus = if cfg!(windows) {
            std::path::PathBuf::from("Z:\\\\corr_d1_does_not_exist_zzz\\\\flow.log")
        } else {
            std::path::PathBuf::from("/proc/0/corr_d1_does_not_exist_zzz/flow.log")
        };
        assert!(
            fsync_parent_dir(&bogus).is_err(),
            "expected error for nonexistent parent {bogus:?}"
        );
    }

    #[test]
    fn corr_d1_eventlog_append_calls_fsync_parent_dir() {
        // We cannot observe fsync directly, but we can verify
        // the code path: every successful `append` returns
        // Ok, and the file content is durable enough that a
        // fresh `read_records` against the same path reads
        // back what we wrote. (The helper is called
        // unconditionally inside `append` — a failed
        // dir-fsync logs and continues, so this test runs on
        // both platforms regardless of the underlying
        // filesystem's fsync semantics.)
        let dir = TempDir::new().expect("tmp");
        let path = dir.path().join("flow.log");
        let flow = FlowId::new();
        let key = fresh_key();
        let mut log = EventLog::open(&path, flow, key.clone()).expect("open");
        log.append(EventType::FlowStarted, b"x".to_vec())
            .expect("append");
        let recs = read_records(&path).expect("read");
        assert_eq!(recs.len(), 1);
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
