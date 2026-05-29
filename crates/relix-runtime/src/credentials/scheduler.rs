//! Background task that walks the credential store every
//! `check_interval_secs` and emits a `rotation_needed`
//! notification for every credential whose
//! `next_rotation_at_ms` has elapsed. Does NOT auto-rotate
//! values — that's an operator-driven action.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::store::{Credential, CredentialStore};

/// Config for the scheduler. Pulled from `[credentials]
/// rotation_check_interval_secs`.
#[derive(Clone, Debug)]
pub struct RotationSchedulerConfig {
    pub check_interval_secs: u64,
}

impl Default for RotationSchedulerConfig {
    fn default() -> Self {
        Self {
            check_interval_secs: 60,
        }
    }
}

/// One rotation-needed notification emitted by the scheduler.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotationNotification {
    pub credential_name: String,
    pub owner_agent: Option<String>,
    pub last_rotated_at_ms: Option<i64>,
    pub next_rotation_at_ms: Option<i64>,
    pub generated_at_ms: i64,
}

impl RotationNotification {
    pub fn from_credential(c: &Credential, now_ms: i64) -> Self {
        Self {
            credential_name: c.name.clone(),
            owner_agent: c.owner_agent.clone(),
            last_rotated_at_ms: c.last_rotated_at_ms,
            next_rotation_at_ms: c.next_rotation_at_ms,
            generated_at_ms: now_ms,
        }
    }
}

/// Sink trait — production wires this to a Telegram /
/// Slack / Email sink (or the existing
/// `MultiChannelAlertSink`). Default `LogRotationNotifier`
/// emits a structured tracing line.
#[async_trait::async_trait]
pub trait RotationNotifier: Send + Sync {
    async fn notify(&self, note: &RotationNotification);
}

/// Default sink — `tracing::warn!` per notification.
#[derive(Clone, Default)]
pub struct LogRotationNotifier;

#[async_trait::async_trait]
impl RotationNotifier for LogRotationNotifier {
    async fn notify(&self, note: &RotationNotification) {
        tracing::warn!(
            credential = %note.credential_name,
            owner = note.owner_agent.as_deref().unwrap_or(""),
            last_rotated_at_ms = note.last_rotated_at_ms.unwrap_or(0),
            next_rotation_at_ms = note.next_rotation_at_ms.unwrap_or(0),
            "credentials: rotation needed"
        );
    }
}

/// Cheap-to-clone scheduler. `spawn()` produces a tokio task
/// that loops until the runtime drops.
#[derive(Clone)]
pub struct RotationScheduler {
    store: CredentialStore,
    notifier: Arc<dyn RotationNotifier>,
    cfg: RotationSchedulerConfig,
}

impl RotationScheduler {
    pub fn new(
        store: CredentialStore,
        notifier: Arc<dyn RotationNotifier>,
        cfg: RotationSchedulerConfig,
    ) -> Self {
        Self {
            store,
            notifier,
            cfg,
        }
    }

    /// Run one sweep — exposed for tests + the
    /// `credentials.rotation_check` cap (one-shot dry-run).
    /// Returns the list of notifications that were dispatched.
    pub async fn sweep_once(&self) -> Vec<RotationNotification> {
        let now = unix_ms();
        let due = match self.store.due_for_rotation(now) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "credentials: rotation sweep failed");
                return Vec::new();
            }
        };
        let mut emitted = Vec::with_capacity(due.len());
        for cred in due {
            let note = RotationNotification::from_credential(&cred, now);
            self.notifier.notify(&note).await;
            emitted.push(note);
        }
        emitted
    }

    /// Spawn the background loop. Returns immediately. The
    /// loop tick equals `cfg.check_interval_secs`. Operators
    /// configure this via `[credentials]`.
    pub fn spawn(self) {
        let interval = Duration::from_secs(self.cfg.check_interval_secs.max(5));
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // First tick fires immediately — sweep on startup so
            // a credential whose rotation date passed while the
            // controller was down still produces a notification.
            ticker.tick().await;
            loop {
                let _ = self.sweep_once().await;
                ticker.tick().await;
            }
        });
    }
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::super::store::CredentialKind;
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Capture {
        notes: Mutex<Vec<RotationNotification>>,
    }

    #[async_trait::async_trait]
    impl RotationNotifier for Capture {
        async fn notify(&self, note: &RotationNotification) {
            self.notes.lock().unwrap().push(note.clone());
        }
    }

    fn fresh_store() -> CredentialStore {
        CredentialStore::open_in_memory("test-master").unwrap()
    }

    #[tokio::test]
    async fn sweep_emits_notification_for_overdue_credential() {
        let store = fresh_store();
        // 1-second interval — next rotation is 1s away. We
        // sweep at +5s to force "due".
        store
            .store(
                "k",
                "v",
                CredentialKind::ApiKey,
                Some("alice"),
                None,
                Some(1),
                None,
            )
            .unwrap();
        // Sleep so next_rotation_at_ms is firmly in the past.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let cap: Arc<Capture> = Arc::new(Capture::default());
        let sched = RotationScheduler::new(
            store,
            cap.clone(),
            RotationSchedulerConfig {
                check_interval_secs: 60,
            },
        );
        let notes = sched.sweep_once().await;
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].credential_name, "k");
        let log = cap.notes.lock().unwrap().clone();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].credential_name, "k");
        assert_eq!(log[0].owner_agent.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn sweep_skips_credentials_without_rotation_interval() {
        let store = fresh_store();
        store
            .store("k", "v", CredentialKind::ApiKey, None, None, None, None)
            .unwrap();
        let cap: Arc<Capture> = Arc::new(Capture::default());
        let sched = RotationScheduler::new(
            store,
            cap.clone(),
            RotationSchedulerConfig {
                check_interval_secs: 60,
            },
        );
        let notes = sched.sweep_once().await;
        assert!(notes.is_empty());
    }

    #[tokio::test]
    async fn sweep_skips_revoked_credentials() {
        let store = fresh_store();
        store
            .store("k", "v", CredentialKind::ApiKey, None, None, Some(1), None)
            .unwrap();
        store.revoke("k", Some("compromised"), None).unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let cap: Arc<Capture> = Arc::new(Capture::default());
        let sched = RotationScheduler::new(
            store,
            cap.clone(),
            RotationSchedulerConfig {
                check_interval_secs: 60,
            },
        );
        let notes = sched.sweep_once().await;
        assert!(notes.is_empty());
    }
}
