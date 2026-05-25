//! `POST /v1/sol/validate` — parse-only validator for the SOL and Sflow
//! languages. Returns `{ valid: true }` on success or
//! `{ valid: false, errors: [{ line, message }] }` on failure.
//!
//! The endpoint is operator-facing: dashboard editors hit it to surface
//! line-numbered errors inline before a flow is deployed. No flow is
//! actually executed, so this is cheap and safe to expose.

use axum::http::StatusCode;
use axum::{Json, response::IntoResponse};
use serde::{Deserialize, Serialize};

use relix_runtime::sflow;
use relix_runtime::sol;

#[derive(Debug, Deserialize)]
pub struct ValidateRequest {
    /// The source text to validate.
    pub source: String,
    /// `"sflow"` or `"sol"`. Defaults to `"sflow"` when omitted.
    #[serde(default = "default_kind")]
    pub kind: String,
}

fn default_kind() -> String {
    "sflow".into()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ValidateResponse {
    pub valid: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ValidateError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ValidateError {
    /// 1-indexed source line, `0` for non-positional errors.
    pub line: usize,
    pub message: String,
}

pub async fn validate(Json(req): Json<ValidateRequest>) -> impl IntoResponse {
    match req.kind.as_str() {
        "sflow" => respond(validate_sflow(&req.source)),
        "sol" => respond(validate_sol(&req.source)),
        other => (
            StatusCode::BAD_REQUEST,
            Json(ValidateResponse {
                valid: false,
                errors: vec![ValidateError {
                    line: 0,
                    message: format!("unknown kind `{other}` (expected `sflow` or `sol`)"),
                }],
            }),
        )
            .into_response(),
    }
}

/// 200 on a clean parse, 400 on errors. The dashboard renders the
/// `errors` array inline next to the editor; the 400 status makes
/// curl / scripts gate on the call without reading the body.
fn respond(errors: Vec<ValidateError>) -> axum::response::Response {
    if errors.is_empty() {
        (
            StatusCode::OK,
            Json(ValidateResponse {
                valid: true,
                errors: vec![],
            }),
        )
            .into_response()
    } else {
        (
            StatusCode::BAD_REQUEST,
            Json(ValidateResponse {
                valid: false,
                errors,
            }),
        )
            .into_response()
    }
}

fn validate_sflow(source: &str) -> Vec<ValidateError> {
    sflow::validate(source)
        .into_iter()
        .map(|e| ValidateError {
            line: e.line,
            message: e.message,
        })
        .collect()
}

/// Validate a SOL source. Delegates to the SOL crate's public
/// [`sol::compile_source`] entry point, which owns the
/// catch_unwind boundary that converts the verbatim port's
/// panic-on-bad-input into a regular `Result`. The bridge stays
/// pure presentation — line number is `0` because the SOL parser
/// does not currently track line numbers; the dashboard surfaces
/// the message string verbatim.
fn validate_sol(source: &str) -> Vec<ValidateError> {
    match sol::compile_source(source) {
        Ok(_) => Vec::new(),
        Err(message) => vec![ValidateError { line: 0, message }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn valid_sflow_returns_no_errors() {
        let errs = validate_sflow("set x = \"y\"\nreturn\n");
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn invalid_sflow_returns_line_numbered_error() {
        let errs = validate_sflow("if true\nreturn\n");
        assert!(!errs.is_empty());
        assert!(errs[0].message.to_lowercase().contains("end"));
    }

    #[test]
    fn valid_sol_returns_no_errors() {
        let src = "function start() -> str {\n    return \"ok\";\n}\n";
        let errs = validate_sol(src);
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn invalid_sol_returns_structured_error() {
        let src = "function start() -> str { let x: str = ";
        let errs = validate_sol(src);
        assert!(!errs.is_empty());
    }

    /// The validate endpoint MUST return 400 on a malformed
    /// payload — historically, bad SOL panicked through the
    /// stack and killed the process. The 400 contract is what
    /// the dashboard relies on.
    #[tokio::test]
    async fn validate_endpoint_returns_400_on_invalid_sol() {
        let req = ValidateRequest {
            source: "function start() -> str { let x: str = ".into(),
            kind: "sol".into(),
        };
        let resp = validate(Json(req)).await.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body: ValidateResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(!body.valid);
        assert!(!body.errors.is_empty());
    }

    #[tokio::test]
    async fn validate_endpoint_returns_200_on_valid_sol() {
        let req = ValidateRequest {
            source: "function start() -> str { return \"ok\"; }\n".into(),
            kind: "sol".into(),
        };
        let resp = validate(Json(req)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn validate_endpoint_returns_400_on_invalid_sflow() {
        let req = ValidateRequest {
            source: "if true\nreturn\n".into(),
            kind: "sflow".into(),
        };
        let resp = validate(Json(req)).await.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Synthesise the historical worst case: input that used to
    /// hard-kill the controller via process::exit. The endpoint
    /// must surface a clean error instead of taking the test
    /// process down.
    #[tokio::test]
    async fn validate_endpoint_survives_unknown_token() {
        let req = ValidateRequest {
            source: "function start() -> str { @ }\n".into(),
            kind: "sol".into(),
        };
        let resp = validate(Json(req)).await.into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
