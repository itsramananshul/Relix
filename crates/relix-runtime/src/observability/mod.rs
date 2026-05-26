//! Two-sink observability — pure metadata in Sink A,
//! short-retention content in Sink B.
//!
//! Sink A (`MetadataSink`) is safe to export anywhere:
//! session id, event type, timestamps, latency, token count,
//! cost, model name, tool name, error class. Sink B
//! (`ContentSink`) is local only and short-retention:
//! prompt / response / tool args / tool output text, linked
//! to Sink A by `event_id`. Operators with elevated access
//! fetch one event's content; OTel export (when it lands)
//! NEVER carries Sink B content.
//!
//! Sub-modules:
//!
//! - [`sinks`] — both stores plus [`sinks::ObservabilityContext`]
//!   which holds both Arcs.
//!
//! The session debugger, provenance registry, and optional
//! OTel exporter land as separate commits + sub-modules.

pub mod sinks;

pub use sinks::{
    ContentEvent, ContentSink, MetadataEvent, MetadataSink, ObservabilityContext, SinkError,
};
