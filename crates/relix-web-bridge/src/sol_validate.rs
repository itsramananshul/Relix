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
use relix_runtime::sol::{analyzer::Analyzer, bytecode::Codegen, lexer::Lexer, parser::Parser};

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

#[derive(Debug, Serialize)]
pub struct ValidateResponse {
    pub valid: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<ValidateError>,
}

#[derive(Debug, Serialize)]
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
            StatusCode::OK,
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

/// SOL has no in-process Result-returning parse entry point — the verbatim
/// port panics / exits on bad input. We isolate the compile pipeline in a
/// `catch_unwind` so a malformed `.sol` produces a structured error
/// instead of taking the bridge process down.
fn validate_sol(source: &str) -> Vec<ValidateError> {
    let tmp = match tempfile::Builder::new()
        .prefix("relix-validate-")
        .suffix(".sol")
        .tempfile()
    {
        Ok(t) => t,
        Err(e) => {
            return vec![ValidateError {
                line: 0,
                message: format!("tempfile: {e}"),
            }];
        }
    };
    if let Err(e) = std::fs::write(tmp.path(), source.as_bytes()) {
        return vec![ValidateError {
            line: 0,
            message: format!("write tempfile: {e}"),
        }];
    }
    let path = tmp.path().to_path_buf();
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let path_str = path
            .to_str()
            .ok_or_else(|| "non-utf8 tempfile path".to_string())?;
        let mut lexer = Lexer::from(path_str);
        let tokens = lexer.tokens();
        let mut parser = Parser::from(tokens);
        let mut program = parser.run();
        let mut analyzer = Analyzer::new();
        analyzer.run(&mut program);
        let mut codegen = Codegen::from(analyzer.tt_arena);
        let _ = codegen.gen_bcode(&program);
        Ok::<_, String>(())
    }));
    match res {
        Ok(Ok(())) => Vec::new(),
        Ok(Err(msg)) => vec![ValidateError {
            line: 0,
            message: msg,
        }],
        Err(panic) => {
            let msg = panic_to_string(panic);
            vec![ValidateError {
                line: 0,
                message: format!("sol parse failed: {msg}"),
            }]
        }
    }
}

fn panic_to_string(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = panic.downcast_ref::<String>() {
        return s.clone();
    }
    "unknown panic".into()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
