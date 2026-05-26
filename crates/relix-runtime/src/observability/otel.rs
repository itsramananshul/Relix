//! OpenTelemetry-shaped export for Sink A.
//!
//! `OtelExporter` translates [`MetadataEvent`] rows into
//! [`OtelSpan`] structs that mirror the OTel span data model
//! (trace_id / span_id / name / attributes / status). Spans
//! are buffered in memory and flushed by the caller — this
//! keeps the implementation runtime-agnostic and lets tests
//! assert on flush results directly.
//!
//! **Sink B content is never read by this module.** The
//! `enabled_events` set lets operators opt specific event
//! types into export. Whitelisted attribute keys (`OtelConfig::
//! allowed_attribute_keys`) further constrain what gets
//! attached — the default is the metadata-only set
//! (`event_type`, `latency_ms`, `model`, `tool`, `success`,
//! `error_type`). The single integration test pins that the
//! exporter never produces a `content`-shaped attribute even
//! when a Sink B row exists for the same event id.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::sinks::MetadataEvent;

/// Per-event-type opt-in. Lets operators turn export on for
/// `model_call` without leaking, say, `secret_access` rows
/// into the trace backend.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtelEventConfig {
    pub enabled_events: BTreeSet<String>,
}

impl OtelEventConfig {
    pub fn enable<S: Into<String>>(mut self, event_type: S) -> Self {
        self.enabled_events.insert(event_type.into());
        self
    }

    pub fn is_enabled(&self, event_type: &str) -> bool {
        self.enabled_events.contains(event_type)
    }
}

/// Top-level exporter config. The attribute whitelist is the
/// real privacy guard — any key not in the set is dropped at
/// build time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtelConfig {
    pub service_name: String,
    pub events: OtelEventConfig,
    pub allowed_attribute_keys: BTreeSet<String>,
}

impl Default for OtelConfig {
    fn default() -> Self {
        let mut keys = BTreeSet::new();
        for k in [
            "event_type",
            "session_id",
            "agent_id",
            "latency_ms",
            "token_count",
            "cost_cents",
            "model",
            "tool",
            "success",
            "error_type",
        ] {
            keys.insert(k.to_string());
        }
        Self {
            service_name: "relix-runtime".into(),
            events: OtelEventConfig::default(),
            allowed_attribute_keys: keys,
        }
    }
}

/// Attribute value. Restricted to the JSON-ish primitives
/// OTel collectors accept.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttrValue {
    Bool(bool),
    Int(i64),
    Str(String),
}

/// One OTel-shaped span built from a Sink A row. `trace_id`
/// maps to `session_id`, `span_id` to `event_id`. Attributes
/// are an ordered map for deterministic test assertions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtelSpan {
    pub trace_id: String,
    pub span_id: String,
    pub name: String,
    pub timestamp_unix: i64,
    pub duration_ms: u64,
    pub status_ok: bool,
    pub attributes: Vec<(String, AttrValue)>,
}

#[derive(Default)]
struct ExporterState {
    pending: Vec<OtelSpan>,
    total_dropped: u64,
}

/// Buffered exporter. `record_event` filters + maps the
/// Sink A row into a span and pushes it; `flush` drains the
/// buffer and returns the batch the caller can ship.
pub struct OtelExporter {
    config: OtelConfig,
    state: Arc<Mutex<ExporterState>>,
}

impl OtelExporter {
    pub fn new(config: OtelConfig) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(ExporterState::default())),
        }
    }

    pub fn config(&self) -> &OtelConfig {
        &self.config
    }

    /// Push one metadata event. Returns `true` when the span
    /// was buffered, `false` when the event type was not in
    /// the enabled set (the buffered counter does NOT move).
    pub fn record_event(&self, event: &MetadataEvent) -> bool {
        if !self.config.events.is_enabled(&event.event_type) {
            return false;
        }
        let span = self.build_span(event);
        let mut s = match self.state.lock() {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!("otel exporter: lock poisoned, dropping span");
                return false;
            }
        };
        s.pending.push(span);
        true
    }

    /// Drain the buffer and return the batch.
    pub fn flush(&self) -> Vec<OtelSpan> {
        let mut s = match self.state.lock() {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!("otel exporter: lock poisoned at flush");
                return Vec::new();
            }
        };
        std::mem::take(&mut s.pending)
    }

    pub fn pending(&self) -> usize {
        self.state.lock().map(|s| s.pending.len()).unwrap_or(0)
    }

    pub fn total_dropped(&self) -> u64 {
        self.state.lock().map(|s| s.total_dropped).unwrap_or(0)
    }

    /// Drop the oldest span. Test helper for simulated
    /// backpressure.
    #[cfg(test)]
    pub fn drop_oldest(&self) {
        if let Ok(mut s) = self.state.lock()
            && !s.pending.is_empty()
        {
            s.pending.remove(0);
            s.total_dropped += 1;
        }
    }

    fn build_span(&self, event: &MetadataEvent) -> OtelSpan {
        let attrs_raw: Vec<(&str, AttrValue)> = vec![
            ("event_type", AttrValue::Str(event.event_type.clone())),
            ("session_id", AttrValue::Str(event.session_id.clone())),
            ("agent_id", AttrValue::Str(event.agent_id.clone())),
            ("success", AttrValue::Bool(event.success)),
        ]
        .into_iter()
        .chain(
            event
                .latency_ms
                .map(|v| ("latency_ms", AttrValue::Int(v as i64))),
        )
        .chain(
            event
                .token_count
                .map(|v| ("token_count", AttrValue::Int(v as i64))),
        )
        .chain(
            event
                .cost_cents
                .map(|v| ("cost_cents", AttrValue::Int(v as i64))),
        )
        .chain(
            event
                .model_name
                .as_ref()
                .map(|v| ("model", AttrValue::Str(v.clone()))),
        )
        .chain(
            event
                .tool_name
                .as_ref()
                .map(|v| ("tool", AttrValue::Str(v.clone()))),
        )
        .chain(
            event
                .error_type
                .as_ref()
                .map(|v| ("error_type", AttrValue::Str(v.clone()))),
        )
        .collect();

        let attributes: Vec<(String, AttrValue)> = attrs_raw
            .into_iter()
            .filter(|(k, _)| self.config.allowed_attribute_keys.contains(*k))
            .map(|(k, v)| (k.to_string(), v))
            .collect();

        OtelSpan {
            trace_id: event.session_id.clone(),
            span_id: event.event_id.clone(),
            name: format!("relix.{}", event.event_type),
            timestamp_unix: event.timestamp_unix,
            duration_ms: event.latency_ms.unwrap_or(0),
            status_ok: event.success,
            attributes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observability::ContentEvent;
    use crate::observability::ObservabilityContext;

    fn event(event_id: &str, ty: &str) -> MetadataEvent {
        MetadataEvent {
            event_id: event_id.into(),
            session_id: "sess-1".into(),
            agent_id: "alice".into(),
            event_type: ty.into(),
            timestamp_unix: 1234,
            latency_ms: Some(250),
            token_count: Some(900),
            cost_cents: Some(3),
            error_type: None,
            tool_name: None,
            model_name: Some("gpt-test".into()),
            success: true,
        }
    }

    #[test]
    fn record_drops_events_not_in_enabled_set() {
        let cfg = OtelConfig {
            events: OtelEventConfig::default().enable("model_call"),
            ..OtelConfig::default()
        };
        let exp = OtelExporter::new(cfg);
        assert!(exp.record_event(&event("a", "model_call")));
        assert!(!exp.record_event(&event("b", "tool_call")));
        assert_eq!(exp.pending(), 1);
    }

    #[test]
    fn flush_drains_buffer_and_preserves_attributes() {
        let cfg = OtelConfig {
            events: OtelEventConfig::default().enable("model_call"),
            ..OtelConfig::default()
        };
        let exp = OtelExporter::new(cfg);
        exp.record_event(&event("a", "model_call"));
        let spans = exp.flush();
        assert_eq!(spans.len(), 1);
        assert_eq!(exp.pending(), 0);
        let s = &spans[0];
        assert_eq!(s.trace_id, "sess-1");
        assert_eq!(s.span_id, "a");
        assert_eq!(s.name, "relix.model_call");
        assert_eq!(s.duration_ms, 250);
        assert!(s.status_ok);
        let attrs: std::collections::BTreeMap<String, AttrValue> =
            s.attributes.iter().cloned().collect();
        assert_eq!(attrs.get("model"), Some(&AttrValue::Str("gpt-test".into())));
        assert_eq!(attrs.get("latency_ms"), Some(&AttrValue::Int(250)));
    }

    #[test]
    fn whitelist_drops_disallowed_attribute_keys() {
        // Restrict the whitelist so only `event_type` survives.
        let mut keys = BTreeSet::new();
        keys.insert("event_type".to_string());
        let cfg = OtelConfig {
            events: OtelEventConfig::default().enable("model_call"),
            allowed_attribute_keys: keys,
            ..OtelConfig::default()
        };
        let exp = OtelExporter::new(cfg);
        exp.record_event(&event("a", "model_call"));
        let s = exp.flush().pop().unwrap();
        assert_eq!(s.attributes.len(), 1);
        assert_eq!(s.attributes[0].0, "event_type");
    }

    #[test]
    fn spans_never_carry_sink_b_content_even_when_recorded() {
        // Record a content row through the full ObservabilityContext;
        // then build a span for the same event id and assert NO
        // attribute looks like prompt / response / tool_output / args.
        let ctx = ObservabilityContext::in_memory();
        let mut e = event("a", "model_call");
        e.event_type = "model_call".into();
        ctx.metadata.record(&e).unwrap();
        ctx.content
            .record(&ContentEvent {
                event_id: "a".into(),
                content_type: "prompt".into(),
                content: "SECRET-PROMPT-MARKER".into(),
                redacted: false,
                timestamp_unix: 1234,
            })
            .unwrap();
        let cfg = OtelConfig {
            events: OtelEventConfig::default().enable("model_call"),
            ..OtelConfig::default()
        };
        let exp = OtelExporter::new(cfg);
        exp.record_event(&e);
        let s = exp.flush().pop().unwrap();
        let serialised = serde_json::to_string(&s).unwrap();
        assert!(
            !serialised.contains("SECRET-PROMPT-MARKER"),
            "OTel span leaked Sink B content: {serialised}"
        );
        for (k, v) in &s.attributes {
            assert!(
                !matches!(
                    k.as_str(),
                    "content" | "prompt" | "response" | "tool_output" | "tool_args"
                ),
                "disallowed attribute key {k} present"
            );
            if let AttrValue::Str(s) = v {
                assert!(
                    !s.contains("SECRET-PROMPT-MARKER"),
                    "secret marker appeared in attribute {k}"
                );
            }
        }
    }

    #[test]
    fn drop_oldest_increments_total_dropped() {
        let cfg = OtelConfig {
            events: OtelEventConfig::default().enable("model_call"),
            ..OtelConfig::default()
        };
        let exp = OtelExporter::new(cfg);
        exp.record_event(&event("a", "model_call"));
        exp.record_event(&event("b", "model_call"));
        exp.drop_oldest();
        assert_eq!(exp.pending(), 1);
        assert_eq!(exp.total_dropped(), 1);
        let s = exp.flush().pop().unwrap();
        assert_eq!(s.span_id, "b");
    }
}
