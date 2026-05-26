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
    /// W7: master switch. `false` (the default) makes
    /// `record_event` and `flush` no-op so existing
    /// deployments don't sprout an unexpected outbound HTTP
    /// dependency. The controller's `[observability.otel]`
    /// section flips this on.
    #[serde(default)]
    pub enabled: bool,
    /// W7: OTLP/HTTP endpoint URL — should already include
    /// the `/v1/traces` suffix. `None` means buffer-only
    /// (tests use this).
    #[serde(default)]
    pub endpoint_url: Option<String>,
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
            enabled: false,
            endpoint_url: None,
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
    /// the enabled set or the exporter is disabled (the
    /// buffered counter does NOT move).
    pub fn record_event(&self, event: &MetadataEvent) -> bool {
        if !self.config.enabled {
            return false;
        }
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

    /// Drain the in-memory buffer. Synchronous, no transport.
    /// Tests + the export loop use this to read pending spans
    /// without committing them over the network.
    pub fn drain_pending(&self) -> Vec<OtelSpan> {
        let mut s = match self.state.lock() {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!("otel exporter: lock poisoned at drain");
                return Vec::new();
            }
        };
        std::mem::take(&mut s.pending)
    }

    /// Drain the buffer and POST every pending span as OTLP/HTTP
    /// JSON to `config.endpoint_url`. When the exporter is
    /// disabled or no endpoint is configured, the buffer is
    /// drained but no HTTP request fires. Transport errors are
    /// logged at `error` level — never panic, never propagate.
    /// Returns the drained span batch so callers (and tests)
    /// can inspect what would have been sent.
    pub async fn flush(&self) -> Vec<OtelSpan> {
        let batch = self.drain_pending();
        if !self.config.enabled || batch.is_empty() {
            return batch;
        }
        let Some(endpoint) = self.config.endpoint_url.clone() else {
            return batch;
        };
        let body = render_otlp_json(&self.config.service_name, &batch);
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    "otel exporter: failed to build HTTP client"
                );
                return batch;
            }
        };
        match client
            .post(&endpoint)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if !status.is_success() {
                    let preview: String = resp
                        .text()
                        .await
                        .unwrap_or_default()
                        .chars()
                        .take(200)
                        .collect();
                    tracing::error!(
                        endpoint = %endpoint,
                        status = %status,
                        body = %preview,
                        "otel exporter: OTLP collector returned non-success"
                    );
                }
            }
            Err(e) => {
                tracing::error!(
                    endpoint = %endpoint,
                    error = %e,
                    "otel exporter: OTLP POST failed"
                );
            }
        }
        batch
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

/// W7: render a batch of [`OtelSpan`]s as an OTLP/HTTP JSON
/// payload. Shape matches the OpenTelemetry spec for traces —
/// one `resourceSpans` entry per service, one `scopeSpans`
/// entry per batch.
pub fn render_otlp_json(service_name: &str, spans: &[OtelSpan]) -> String {
    let span_objs: Vec<serde_json::Value> = spans.iter().map(otel_span_to_otlp_json).collect();
    let payload = serde_json::json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [{
                    "key": "service.name",
                    "value": { "stringValue": service_name },
                }],
            },
            "scopeSpans": [{
                "scope": { "name": "relix-runtime", "version": env!("CARGO_PKG_VERSION") },
                "spans": span_objs,
            }],
        }],
    });
    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
}

fn otel_span_to_otlp_json(s: &OtelSpan) -> serde_json::Value {
    let start_nanos = (s.timestamp_unix as i128).saturating_mul(1_000_000_000);
    let end_nanos = start_nanos.saturating_add((s.duration_ms as i128).saturating_mul(1_000_000));
    let attrs: Vec<serde_json::Value> = s
        .attributes
        .iter()
        .map(|(k, v)| {
            let val = match v {
                AttrValue::Bool(b) => serde_json::json!({"boolValue": b}),
                AttrValue::Int(i) => serde_json::json!({"intValue": i.to_string()}),
                AttrValue::Str(s) => serde_json::json!({"stringValue": s}),
            };
            serde_json::json!({"key": k, "value": val})
        })
        .collect();
    serde_json::json!({
        "traceId": hex_pad(&s.trace_id, 32),
        "spanId": hex_pad(&s.span_id, 16),
        "name": s.name,
        "kind": 1, // SPAN_KIND_INTERNAL
        "startTimeUnixNano": start_nanos.to_string(),
        "endTimeUnixNano": end_nanos.to_string(),
        "attributes": attrs,
        "status": {
            // 1 = STATUS_CODE_OK, 2 = STATUS_CODE_ERROR
            "code": if s.status_ok { 1 } else { 2 },
        },
    })
}

/// OTLP requires trace IDs to be 32 hex chars (16 bytes) and
/// span IDs 16 hex chars (8 bytes). Relix IDs are operator-
/// chosen strings; we hash them with BLAKE3 and take a stable
/// prefix so the wire shape matches the spec without requiring
/// callers to supply hex-formatted IDs.
fn hex_pad(id: &str, hex_chars: usize) -> String {
    let digest = blake3::hash(id.as_bytes());
    let hex = hex::encode(digest.as_bytes());
    hex.chars().take(hex_chars).collect()
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

    fn enabled_cfg() -> OtelConfig {
        OtelConfig {
            enabled: true,
            events: OtelEventConfig::default().enable("model_call"),
            ..OtelConfig::default()
        }
    }

    #[test]
    fn record_drops_events_not_in_enabled_set() {
        let exp = OtelExporter::new(enabled_cfg());
        assert!(exp.record_event(&event("a", "model_call")));
        assert!(!exp.record_event(&event("b", "tool_call")));
        assert_eq!(exp.pending(), 1);
    }

    #[test]
    fn drain_pending_returns_buffer_and_preserves_attributes() {
        let exp = OtelExporter::new(enabled_cfg());
        exp.record_event(&event("a", "model_call"));
        let spans = exp.drain_pending();
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
            enabled: true,
            events: OtelEventConfig::default().enable("model_call"),
            allowed_attribute_keys: keys,
            ..OtelConfig::default()
        };
        let exp = OtelExporter::new(cfg);
        exp.record_event(&event("a", "model_call"));
        let s = exp.drain_pending().pop().unwrap();
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
        let exp = OtelExporter::new(enabled_cfg());
        exp.record_event(&e);
        let s = exp.drain_pending().pop().unwrap();
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
        let exp = OtelExporter::new(enabled_cfg());
        exp.record_event(&event("a", "model_call"));
        exp.record_event(&event("b", "model_call"));
        exp.drop_oldest();
        assert_eq!(exp.pending(), 1);
        assert_eq!(exp.total_dropped(), 1);
        let s = exp.drain_pending().pop().unwrap();
        assert_eq!(s.span_id, "b");
    }

    // ── W7: real OTLP HTTP transport ──────────────────────────────

    use std::io::{Read, Write};
    use std::sync::Arc as TestArc;
    use std::sync::Mutex as TestMutex;

    /// Single-request OTLP/HTTP collector mock. Spawns a thread,
    /// accepts one connection, records the POST body bytes in
    /// `captured`, returns the configured status.
    fn spawn_one_shot_otlp(status: u16) -> (String, TestArc<TestMutex<Vec<u8>>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let captured: TestArc<TestMutex<Vec<u8>>> = TestArc::new(TestMutex::new(Vec::new()));
        let cap_clone = captured.clone();
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = vec![0u8; 16 * 1024];
                let n = sock.read(&mut buf).unwrap_or(0);
                let raw: Vec<u8> = buf[..n].to_vec();
                if let Some(body_start) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let body = raw[body_start + 4..].to_vec();
                    *cap_clone.lock().unwrap() = body;
                }
                let reason = if (200..300).contains(&status) {
                    "OK"
                } else {
                    "Err"
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(resp.as_bytes());
                let _ = sock.shutdown(std::net::Shutdown::Write);
            }
        });
        (format!("http://{addr}/v1/traces"), captured)
    }

    #[tokio::test]
    async fn flush_posts_otlp_json_to_configured_endpoint_when_enabled() {
        let (url, captured) = spawn_one_shot_otlp(200);
        let cfg = OtelConfig {
            enabled: true,
            endpoint_url: Some(url),
            events: OtelEventConfig::default().enable("model_call"),
            ..OtelConfig::default()
        };
        let exp = OtelExporter::new(cfg);
        exp.record_event(&event("a", "model_call"));
        let drained = exp.flush().await;
        assert_eq!(drained.len(), 1, "flush should drain the buffer");
        // Give the mock thread a brief window to record the
        // captured body. The TcpListener has already returned
        // the response so this is bounded.
        for _ in 0..20 {
            if !captured.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let body = captured.lock().unwrap().clone();
        assert!(
            !body.is_empty(),
            "OTLP collector mock did not receive a body"
        );
        let body_str = String::from_utf8(body).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_str).expect("body is JSON");
        assert!(
            v["resourceSpans"].is_array(),
            "expected OTLP resourceSpans key: {body_str}"
        );
        let scope_spans = &v["resourceSpans"][0]["scopeSpans"];
        let spans = &scope_spans[0]["spans"];
        assert_eq!(spans[0]["name"], "relix.model_call");
        assert!(spans[0]["traceId"].as_str().unwrap().len() == 32);
        assert!(spans[0]["spanId"].as_str().unwrap().len() == 16);
    }

    #[tokio::test]
    async fn flush_makes_no_http_request_when_disabled() {
        // Configure an endpoint URL that — if hit — would
        // surface a connection error. The exporter must NOT
        // POST when `enabled` is false; record_event is a
        // no-op too, so the buffer is empty.
        let cfg = OtelConfig {
            enabled: false,
            endpoint_url: Some("http://127.0.0.1:1/should-not-be-hit".to_string()),
            events: OtelEventConfig::default().enable("model_call"),
            ..OtelConfig::default()
        };
        let exp = OtelExporter::new(cfg);
        assert!(!exp.record_event(&event("a", "model_call")));
        assert_eq!(exp.pending(), 0);
        let drained = exp.flush().await;
        assert!(
            drained.is_empty(),
            "disabled exporter must not buffer or POST"
        );
    }

    #[tokio::test]
    async fn flush_with_unreachable_endpoint_logs_error_and_does_not_panic() {
        // Port 1 is reserved / never listening on a normal
        // host; the POST attempt fails. The exporter must
        // swallow the error rather than panic.
        let cfg = OtelConfig {
            enabled: true,
            endpoint_url: Some("http://127.0.0.1:1/v1/traces".to_string()),
            events: OtelEventConfig::default().enable("model_call"),
            ..OtelConfig::default()
        };
        let exp = OtelExporter::new(cfg);
        exp.record_event(&event("a", "model_call"));
        // No panic. The buffer is drained even though POST fails.
        let drained = exp.flush().await;
        assert_eq!(drained.len(), 1);
        assert_eq!(exp.pending(), 0);
    }

    #[test]
    fn render_otlp_json_emits_resource_spans_with_service_name() {
        let spans = vec![OtelSpan {
            trace_id: "sess-1".into(),
            span_id: "a".into(),
            name: "relix.model_call".into(),
            timestamp_unix: 1_700_000_000,
            duration_ms: 250,
            status_ok: true,
            attributes: vec![("model".into(), AttrValue::Str("gpt-test".into()))],
        }];
        let body = render_otlp_json("relix-runtime", &spans);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let rs = &v["resourceSpans"][0];
        let resource_attrs = rs["resource"]["attributes"].as_array().unwrap();
        assert_eq!(resource_attrs[0]["key"], "service.name");
        assert_eq!(resource_attrs[0]["value"]["stringValue"], "relix-runtime");
        let span = &rs["scopeSpans"][0]["spans"][0];
        assert_eq!(span["name"], "relix.model_call");
        assert_eq!(span["status"]["code"], 1);
    }
}
