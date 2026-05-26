//! `GET /v1/schema` — JSON shape of the bridge's request +
//! response types.
//!
//! Honest about scope: this is hand-written, not generated from
//! the types. The Relix HTTP surface is large enough that a
//! true automatic emitter would be its own project; the
//! hand-written contract covers the routes SDK authors and
//! app developers care about (chat + memory + tasks).
//!
//! The shape is a flat object keyed by operation name; each
//! entry has `request` and `response` sub-objects describing
//! the JSON keys, their types, and a short prose hint. This is
//! intentionally NOT JSON Schema (we're not gating requests on
//! it server-side); it's a human-readable contract document
//! that consuming code can also key off.

use axum::Json;
use axum::response::IntoResponse;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct SchemaDoc {
    pub version: &'static str,
    /// Top-level operation map. Each key is a Relix concept
    /// (e.g. `chat`); the value names the HTTP route + describes
    /// the JSON request and response shapes.
    pub operations: Vec<Operation>,
    /// Headers the bridge accepts on every route. The SDK uses
    /// this section to discover the tenant header without
    /// guessing.
    pub headers: Vec<HeaderDoc>,
}

#[derive(Debug, Serialize)]
pub struct Operation {
    pub name: &'static str,
    pub method: &'static str,
    pub path: &'static str,
    pub description: &'static str,
    pub request: Vec<Field>,
    pub response: Vec<Field>,
}

#[derive(Debug, Serialize)]
pub struct Field {
    pub name: &'static str,
    pub ty: &'static str,
    pub required: bool,
    pub description: &'static str,
}

#[derive(Debug, Serialize)]
pub struct HeaderDoc {
    pub name: &'static str,
    pub required: bool,
    pub description: &'static str,
}

pub async fn schema() -> impl IntoResponse {
    Json(SchemaDoc {
        version: env!("CARGO_PKG_VERSION"),
        operations: vec![
            Operation {
                name: "chat",
                method: "POST",
                path: "/chat",
                description: "One-shot chat. Returns the assistant's final reply text in `reply`.",
                request: vec![
                    Field {
                        name: "session_id",
                        ty: "string",
                        required: true,
                        description: "Stable conversation id. Used to scope memory lookups.",
                    },
                    Field {
                        name: "message",
                        ty: "string",
                        required: true,
                        description: "The user's prompt text.",
                    },
                ],
                response: vec![
                    Field {
                        name: "reply",
                        ty: "string",
                        required: true,
                        description: "Final assistant text.",
                    },
                    Field {
                        name: "flow_id",
                        ty: "string",
                        required: false,
                        description: "Per-flow event log id for traceability.",
                    },
                    Field {
                        name: "task_id",
                        ty: "string",
                        required: false,
                        description: "Coordinator task id when persistence was wired.",
                    },
                ],
            },
            Operation {
                name: "chat_stream",
                method: "POST",
                path: "/chat/stream",
                description: "Streaming chat. Response is `text/event-stream`; each `data:` frame is one chunk with `{chunk: \"...\"}`. Terminated by `data: [DONE]`.",
                request: vec![
                    Field {
                        name: "session_id",
                        ty: "string",
                        required: true,
                        description: "Same as `chat`.",
                    },
                    Field {
                        name: "message",
                        ty: "string",
                        required: true,
                        description: "Same as `chat`.",
                    },
                ],
                response: vec![Field {
                    name: "(stream)",
                    ty: "text/event-stream",
                    required: true,
                    description: "Each `data:` frame is `{chunk: string}`; final frame is `{type: \"done\"}` then `data: [DONE]`.",
                }],
            },
            Operation {
                name: "openai_chat",
                method: "POST",
                path: "/v1/chat/completions",
                description: "OpenAI-compatible shim. Set `stream: true` for SSE delta events.",
                request: vec![
                    Field {
                        name: "model",
                        ty: "string",
                        required: false,
                        description: "Operator-defined model alias. Empty ⇒ bridge default.",
                    },
                    Field {
                        name: "messages",
                        ty: "array<{role,content}>",
                        required: true,
                        description: "OpenAI message array; the last user turn is taken as the prompt.",
                    },
                    Field {
                        name: "stream",
                        ty: "boolean",
                        required: false,
                        description: "When true, emit `text/event-stream` deltas.",
                    },
                ],
                response: vec![
                    Field {
                        name: "choices",
                        ty: "array<{message,finish_reason}>",
                        required: true,
                        description: "Standard OpenAI shape; single choice today.",
                    },
                    Field {
                        name: "relix",
                        ty: "{flow_id,trace_id,flow_log,session_id,task_id?}",
                        required: true,
                        description: "Relix extension fields.",
                    },
                ],
            },
            Operation {
                name: "info",
                method: "GET",
                path: "/v1/info",
                description: "Bridge info — system name, version, configured provider/model, capabilities.",
                request: vec![],
                response: vec![
                    Field {
                        name: "system",
                        ty: "string",
                        required: true,
                        description: "Always \"relix\".",
                    },
                    Field {
                        name: "version",
                        ty: "string",
                        required: true,
                        description: "Bridge semver.",
                    },
                    Field {
                        name: "provider",
                        ty: "string",
                        required: true,
                        description: "Configured default provider (or \"mesh\" when unknown).",
                    },
                    Field {
                        name: "model",
                        ty: "string",
                        required: true,
                        description: "Configured default model id.",
                    },
                    Field {
                        name: "capabilities",
                        ty: "array<string>",
                        required: true,
                        description: "High-level capability tags.",
                    },
                ],
            },
            Operation {
                name: "models",
                method: "GET",
                path: "/v1/models",
                description: "OpenAI-shape model list: curated entries + manifest-cache-derived discoveries.",
                request: vec![],
                response: vec![
                    Field {
                        name: "object",
                        ty: "\"list\"",
                        required: true,
                        description: "",
                    },
                    Field {
                        name: "data",
                        ty: "array<{id, object, created, owned_by, description}>",
                        required: true,
                        description: "One entry per known model.",
                    },
                ],
            },
            Operation {
                name: "memory_embed",
                method: "POST",
                path: "/v1/memory/embed",
                description: "Persist a memory chunk against a subject_id/target pair.",
                request: vec![
                    Field {
                        name: "subject_id",
                        ty: "string",
                        required: true,
                        description: "Scope for the memory. SDK uses `tenant:<id>`.",
                    },
                    Field {
                        name: "target",
                        ty: "\"agent\" | \"user\"",
                        required: true,
                        description: "Which memory bucket to write to.",
                    },
                    Field {
                        name: "chunk",
                        ty: "string",
                        required: true,
                        description: "Text to embed + persist.",
                    },
                    Field {
                        name: "tags",
                        ty: "array<string>",
                        required: false,
                        description: "Optional operator tags.",
                    },
                ],
                response: vec![Field {
                    name: "embedding_id",
                    ty: "string",
                    required: true,
                    description: "Stable id of the persisted entry.",
                }],
            },
            Operation {
                name: "memory_search",
                method: "POST",
                path: "/v1/memory/search",
                description: "Cosine-similarity vector search across a subject_id/target pair.",
                request: vec![
                    Field {
                        name: "subject_id",
                        ty: "string",
                        required: true,
                        description: "",
                    },
                    Field {
                        name: "target",
                        ty: "string",
                        required: true,
                        description: "",
                    },
                    Field {
                        name: "query",
                        ty: "string",
                        required: true,
                        description: "Text to embed + search by.",
                    },
                    Field {
                        name: "top_k",
                        ty: "integer",
                        required: false,
                        description: "Default 10, capped at 20.",
                    },
                ],
                response: vec![Field {
                    name: "hits",
                    ty: "array<{id, content, score, tags?}>",
                    required: true,
                    description: "Sorted by score descending.",
                }],
            },
        ],
        headers: vec![
            HeaderDoc {
                name: "Authorization",
                required: true,
                description: "Bearer <bridge-token>. See ~/.relix/bridge-token.",
            },
            HeaderDoc {
                name: "X-Relix-Tenant",
                required: false,
                description: "Opaque tenant id. Defaults to \"default\" when absent. Flows through to task creation + audit log entries.",
            },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn schema_response_lists_documented_operations() {
        let resp = schema().await.into_response();
        let bytes = to_bytes(resp.into_body(), 32 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let ops = v["operations"].as_array().expect("operations array");
        // Sanity: every operation listed in the SDK has a schema
        // entry the SDK can rely on.
        for needed in ["chat", "chat_stream", "openai_chat", "info", "models"] {
            assert!(
                ops.iter().any(|o| o["name"] == needed),
                "missing operation {needed} in schema"
            );
        }
        assert!(
            v["headers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|h| h["name"] == "X-Relix-Tenant"),
            "tenant header missing from schema doc"
        );
    }
}
