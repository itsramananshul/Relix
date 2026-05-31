//! `POST /v1/yaml/validate` — parse-only validator for the
//! YAML flow frontend. Mirrors the contract of
//! `crate::sol_validate` but uses the YAML compile pipeline.
//! No flow is executed; this is cheap and safe to expose to
//! the dashboard editor.
//!
//! Response shape:
//!
//! - `{ "status": "ok" }` on a clean parse + lower (HTTP 200).
//! - `{ "status": "error", "message": "...", "line": N, "column": M }`
//!   on failure (HTTP 400). `line` and `column` are 1-based;
//!   they are `0` only when the error has no positional info
//!   attached (e.g. a root-level schema error before any step
//!   has been parsed).

use axum::http::StatusCode;
use axum::{Json, response::IntoResponse};
use serde::{Deserialize, Serialize};

use relix_runtime::yaml_flow::{self, YamlFlowError};

#[derive(Debug, Deserialize)]
pub struct YamlValidateRequest {
    /// The YAML flow source text to validate.
    pub source: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct YamlValidateResponse {
    /// `"ok"` on success, `"error"` on failure.
    pub status: String,
    /// Error message — present on failure, omitted on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 1-based source line of the offending token / step. `0`
    /// means "no positional info available".
    #[serde(default, skip_serializing_if = "is_zero")]
    pub line: usize,
    /// 1-based source column. `0` when unavailable.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub column: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

pub async fn validate(Json(req): Json<YamlValidateRequest>) -> impl IntoResponse {
    match yaml_flow::compile_source(&req.source) {
        Ok(_) => (
            StatusCode::OK,
            Json(YamlValidateResponse {
                status: "ok".to_string(),
                message: None,
                line: 0,
                column: 0,
            }),
        )
            .into_response(),
        Err(e) => {
            let (line, column) = match &e {
                YamlFlowError::Parse { line, column, .. } => (*line, *column),
                YamlFlowError::Semantic { line, column, .. } => (*line, *column),
                // SEC PART 3: the new InvalidCondition +
                // InvalidScalar variants are path-only (no
                // source span) — report (0, 0) so the
                // dashboard surfaces the path + message
                // verbatim.
                YamlFlowError::Lower { .. }
                | YamlFlowError::Io { .. }
                | YamlFlowError::InvalidCondition { .. }
                | YamlFlowError::InvalidScalar { .. }
                | YamlFlowError::FileTooLarge { .. }
                | YamlFlowError::NestingTooDeep { .. } => (0, 0),
            };
            (
                StatusCode::BAD_REQUEST,
                Json(YamlValidateResponse {
                    status: "error".to_string(),
                    message: Some(e.to_string()),
                    line,
                    column,
                }),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn validate_endpoint_returns_200_ok_on_valid_yaml() {
        let req = YamlValidateRequest {
            source: r#"
                steps:
                  - let:
                      name: greeting
                      type: str
                      value: "hello"
                  - result: "{{greeting}}"
            "#
            .into(),
        };
        let resp = validate(Json(req)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: YamlValidateResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body.status, "ok".to_string());
        assert!(body.message.is_none());
    }

    #[tokio::test]
    async fn validate_endpoint_returns_400_on_missing_required_field() {
        // `let` step without the required `value` field.
        let req = YamlValidateRequest {
            source: r#"
                steps:
                  - let:
                      name: x
                      type: str
            "#
            .into(),
        };
        let resp = validate(Json(req)).await.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: YamlValidateResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body.status, "error".to_string());
        let message = body.message.expect("error must carry a message");
        assert!(!message.is_empty(), "error message must not be empty");
        assert!(
            message.contains("value"),
            "message should name the missing field: {message}"
        );
    }

    #[tokio::test]
    async fn validate_endpoint_returns_400_with_line_number_on_malformed_yaml() {
        // YAML with an unclosed flow sequence — serde_yaml's
        // parse error carries a real line / column.
        let req = YamlValidateRequest {
            source: "steps: [\n  - let:\n      name: x\n".into(),
        };
        let resp = validate(Json(req)).await.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: YamlValidateResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body.status, "error".to_string());
        assert!(body.message.is_some_and(|m| !m.is_empty()));
        assert!(
            body.line > 0,
            "malformed YAML must carry a positive line number, got {}",
            body.line
        );
    }

    #[tokio::test]
    async fn validate_endpoint_returns_400_with_step_line_on_unknown_step_type() {
        // The semantic error must carry the line of the
        // offending step from the source-text scan.
        let req = YamlValidateRequest {
            source: "steps:\n  - bonk:\n      foo: bar\n".into(),
        };
        let resp = validate(Json(req)).await.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: YamlValidateResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body.status, "error".to_string());
        assert!(
            body.message.as_deref().is_some_and(|m| m.contains("bonk")),
            "message should name the bad step type"
        );
        assert!(
            body.line > 0,
            "semantic error must carry a positive line number"
        );
    }
}
