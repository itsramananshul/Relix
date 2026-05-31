//! Shared in-memory state for the email controller.
//!
//! Mirrors `nodes/slack/state.rs` but tracks two independent
//! connections (SMTP outbound + IMAP inbound) plus a per-channel
//! "last successful send / poll" timestamp the bridge surfaces
//! at `GET /v1/email/status`.

use std::sync::Mutex;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EmailIdentity {
    /// Configured `From:` mailbox (cosmetic — the SMTP client
    /// owns the actual outgoing address).
    pub from: String,
    /// SMTP host (no port) for the status panel.
    pub smtp_host: String,
    /// IMAP host (no port).
    pub imap_host: String,
    /// IMAP folder being watched.
    pub imap_folder: String,
}

/// Connection-status enum surfaced in `email.status`. The bridge
/// renders these strings verbatim.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkStatus {
    #[default]
    Disconnected,
    Connected,
    Error,
}

impl LinkStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkStatus::Disconnected => "disconnected",
            LinkStatus::Connected => "connected",
            LinkStatus::Error => "error",
        }
    }
}

#[derive(Default)]
pub struct EmailChannelState {
    identity: Mutex<EmailIdentity>,
    smtp_status: Mutex<LinkStatus>,
    imap_status: Mutex<LinkStatus>,
    smtp_last_error: Mutex<Option<String>>,
    imap_last_error: Mutex<Option<String>>,
    /// Unix seconds — last successful outbound send.
    last_send_at: Mutex<Option<i64>>,
    /// Unix seconds — last successful IMAP poll / IDLE wake.
    last_poll_at: Mutex<Option<i64>>,
    messages_seen: Mutex<u64>,
    messages_sent: Mutex<u64>,
    last_message_at: Mutex<Option<i64>>,
}

impl EmailChannelState {
    pub fn identity(&self) -> EmailIdentity {
        self.identity
            .lock()
            .unwrap_or_else(|e| {
                tracing::warn!("'poisoned'; recovering inner state");
                e.into_inner()
            })
            .clone()
    }
    pub fn set_identity(&self, id: EmailIdentity) {
        *self.identity.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = id;
    }
    pub fn smtp_status(&self) -> LinkStatus {
        *self.smtp_status.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        })
    }
    pub fn imap_status(&self) -> LinkStatus {
        *self.imap_status.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        })
    }
    pub fn smtp_last_error(&self) -> Option<String> {
        self.smtp_last_error
            .lock()
            .unwrap_or_else(|e| {
                tracing::warn!("'poisoned'; recovering inner state");
                e.into_inner()
            })
            .clone()
    }
    pub fn imap_last_error(&self) -> Option<String> {
        self.imap_last_error
            .lock()
            .unwrap_or_else(|e| {
                tracing::warn!("'poisoned'; recovering inner state");
                e.into_inner()
            })
            .clone()
    }
    pub fn last_send_at(&self) -> Option<i64> {
        *self.last_send_at.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        })
    }
    pub fn last_poll_at(&self) -> Option<i64> {
        *self.last_poll_at.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        })
    }
    pub fn messages_seen(&self) -> u64 {
        *self.messages_seen.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        })
    }
    pub fn messages_sent(&self) -> u64 {
        *self.messages_sent.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        })
    }
    pub fn last_message_at(&self) -> Option<i64> {
        *self.last_message_at.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        })
    }
    pub fn mark_smtp_connected(&self) {
        *self.smtp_status.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = LinkStatus::Connected;
        *self.smtp_last_error.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = None;
    }
    pub fn mark_smtp_error(&self, error: &str) {
        *self.smtp_status.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = LinkStatus::Error;
        *self.smtp_last_error.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = Some(error.to_string());
    }
    pub fn mark_smtp_disconnected(&self) {
        *self.smtp_status.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = LinkStatus::Disconnected;
    }
    pub fn mark_imap_connected(&self) {
        *self.imap_status.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = LinkStatus::Connected;
        *self.imap_last_error.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = None;
    }
    pub fn mark_imap_error(&self, error: &str) {
        *self.imap_status.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = LinkStatus::Error;
        *self.imap_last_error.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = Some(error.to_string());
    }
    pub fn mark_imap_disconnected(&self) {
        *self.imap_status.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = LinkStatus::Disconnected;
    }
    pub fn record_send(&self, ts: i64) {
        *self.last_send_at.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = Some(ts);
        *self.messages_sent.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) += 1;
    }
    pub fn record_poll(&self, ts: i64) {
        *self.last_poll_at.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = Some(ts);
    }
    pub fn record_inbound(&self, ts: i64) {
        *self.messages_seen.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) += 1;
        *self.last_message_at.lock().unwrap_or_else(|e| {
            tracing::warn!("'poisoned'; recovering inner state");
            e.into_inner()
        }) = Some(ts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_status_defaults_disconnected() {
        let s = EmailChannelState::default();
        assert_eq!(s.smtp_status(), LinkStatus::Disconnected);
        assert_eq!(s.imap_status(), LinkStatus::Disconnected);
        assert!(s.smtp_last_error().is_none());
        assert!(s.imap_last_error().is_none());
    }

    #[test]
    fn smtp_connected_clears_prior_error() {
        let s = EmailChannelState::default();
        s.mark_smtp_error("dial: refused");
        assert_eq!(s.smtp_status(), LinkStatus::Error);
        assert_eq!(s.smtp_last_error().as_deref(), Some("dial: refused"));
        s.mark_smtp_connected();
        assert_eq!(s.smtp_status(), LinkStatus::Connected);
        assert!(s.smtp_last_error().is_none());
    }

    #[test]
    fn record_send_bumps_counter_and_timestamp() {
        let s = EmailChannelState::default();
        s.record_send(100);
        s.record_send(200);
        assert_eq!(s.messages_sent(), 2);
        assert_eq!(s.last_send_at(), Some(200));
    }

    #[test]
    fn record_inbound_bumps_counter_and_timestamp() {
        let s = EmailChannelState::default();
        s.record_inbound(50);
        s.record_inbound(75);
        assert_eq!(s.messages_seen(), 2);
        assert_eq!(s.last_message_at(), Some(75));
    }

    #[test]
    fn identity_round_trips() {
        let s = EmailChannelState::default();
        let id = EmailIdentity {
            from: "Relix <bot@example.com>".into(),
            smtp_host: "smtp.example.com".into(),
            imap_host: "imap.example.com".into(),
            imap_folder: "INBOX".into(),
        };
        s.set_identity(id.clone());
        assert_eq!(s.identity(), id);
    }

    #[test]
    fn link_status_str_matches_wire() {
        assert_eq!(LinkStatus::Connected.as_str(), "connected");
        assert_eq!(LinkStatus::Disconnected.as_str(), "disconnected");
        assert_eq!(LinkStatus::Error.as_str(), "error");
    }
}
