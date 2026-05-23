//! Agent-to-agent messaging — direct point-to-point channel
//! between two agents that lives independently of the task
//! ledger.
//!
//! Distinct from delegation:
//!
//! - **Delegation** spawns a child task; the parent waits on
//!   the outcome. The relationship is parent/child and the
//!   child has full task-lifecycle bookkeeping (origin
//!   surface, chronicle, retries, status transitions).
//! - **Messaging** is a peer-to-peer mail drop. Sender posts
//!   to a recipient's inbox; recipient reads + replies. No
//!   task is created, no chronicle event lands on a task
//!   row (a single `msg.sent` chronicle entry on the
//!   coordinator's system task captures the audit trail
//!   without storing the body).
//!
//! Lifecycle:
//!
//! ```text
//! delivered → read → expired
//!     │           ↑
//!     ╰── (auto-expire after ttl_secs from sent_at)
//! ```
//!
//! Soft delete writes `status = expired` so the row remains
//! visible to audit but disappears from operator inboxes.

pub mod handlers;
pub mod store;

pub use store::{MessageRecord, MessageStatus, MessageStore, MessageStoreError};
