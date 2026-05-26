//! Deterministic JSON contracts for tool calls.
//!
//! Each tool optionally declares an `input_schema` and an
//! `output_schema`. When a contract is wired through the
//! dispatcher, both the inbound args and the handler's reply
//! are validated against their schema before the tool sees
//! them / the caller sees the reply.
//!
//! The validator is deliberately small: a handful of
//! jsonschema-shaped rules implemented in Rust without an
//! external crate. The supported keywords are documented
//! below and cover every contract the alpha currently needs.
//! Schemas that use unsupported keywords pass through (the
//! goal is "catch malformed args," not "fully spec-compliant
//! schema engine").

use serde_json::Value;

/// One tool's input + output contract.
#[derive(Clone, Debug)]
pub struct ToolContract {
    pub tool_name: String,
    pub input_schema: Value,
    pub output_schema: Value,
}

impl ToolContract {
    pub fn new(tool_name: impl Into<String>, input_schema: Value, output_schema: Value) -> Self {
        Self {
            tool_name: tool_name.into(),
            input_schema,
            output_schema,
        }
    }

    /// Validate `input` against the contract's
    /// `input_schema`. Returns the list of human-readable
    /// validation errors (empty on success).
    pub fn validate_input(&self, input: &Value) -> Result<(), Vec<String>> {
        let errs = validate(input, &self.input_schema, "$");
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }

    /// Validate `output` against the contract's
    /// `output_schema`. Same return shape as
    /// `validate_input`.
    pub fn validate_output(&self, output: &Value) -> Result<(), Vec<String>> {
        let errs = validate(output, &self.output_schema, "$");
        if errs.is_empty() { Ok(()) } else { Err(errs) }
    }
}

/// Built-in contract for the filesystem write/read pair.
/// Public so tests + bridge surfaces can reuse it directly.
pub fn fs_write_contract() -> ToolContract {
    ToolContract::new(
        "tool.fs.write_file",
        serde_json::json!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            }
        }),
        serde_json::json!({
            "type": "object",
            "required": ["ok"],
            "properties": {
                "ok": { "type": "string" }
            }
        }),
    )
}

/// Run the validation recursively. `path` is the JSON
/// pointer-like prefix used to name fields in the error
/// messages.
fn validate(value: &Value, schema: &Value, path: &str) -> Vec<String> {
    let mut errs: Vec<String> = Vec::new();
    let schema_obj = match schema.as_object() {
        Some(o) => o,
        None => return errs, // non-object schema passes through
    };
    // type
    if let Some(ty) = schema_obj.get("type").and_then(|t| t.as_str())
        && !value_matches_type(value, ty)
    {
        errs.push(format!(
            "{path}: expected type `{ty}`, got `{}`",
            value_type_name(value)
        ));
        // No point continuing the other checks when the
        // top-level type is wrong — required/properties
        // assume the value is at least the right shape.
        return errs;
    }
    // required (only meaningful for objects)
    if let Some(req) = schema_obj.get("required").and_then(|r| r.as_array())
        && let Some(obj) = value.as_object()
    {
        for r in req {
            if let Some(name) = r.as_str()
                && !obj.contains_key(name)
            {
                errs.push(format!("{path}: missing required field `{name}`"));
            }
        }
    }
    // properties — recurse into each present child.
    if let Some(props) = schema_obj.get("properties").and_then(|p| p.as_object())
        && let Some(obj) = value.as_object()
    {
        for (prop_name, sub_schema) in props {
            if let Some(sub_value) = obj.get(prop_name) {
                let child_path = format!("{path}.{prop_name}");
                errs.extend(validate(sub_value, sub_schema, &child_path));
            }
        }
    }
    errs
}

fn value_matches_type(v: &Value, ty: &str) -> bool {
    match ty {
        "object" => v.is_object(),
        "string" => v.is_string(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        _ => true, // unknown type → pass through
    }
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_input_passes_validation() {
        let c = fs_write_contract();
        let v = serde_json::json!({"path": "/tmp/a", "content": "hello"});
        assert_eq!(c.validate_input(&v), Ok(()));
    }

    #[test]
    fn missing_required_field_fails_validation() {
        let c = fs_write_contract();
        let v = serde_json::json!({"path": "/tmp/a"});
        let errs = c.validate_input(&v).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("missing required field `content`"))
        );
    }

    #[test]
    fn wrong_type_fails_validation() {
        let c = fs_write_contract();
        // path is a number, not a string.
        let v = serde_json::json!({"path": 42, "content": "x"});
        let errs = c.validate_input(&v).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("expected type `string`")));
        assert!(errs.iter().any(|e| e.contains("$.path")));
    }

    #[test]
    fn top_level_type_mismatch_short_circuits() {
        let c = fs_write_contract();
        // Pass a string where the schema expects an object —
        // we report the top-level mismatch only.
        let v = serde_json::json!("not an object");
        let errs = c.validate_input(&v).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("expected type `object`"));
    }

    #[test]
    fn valid_output_passes_validation() {
        let c = fs_write_contract();
        let v = serde_json::json!({"ok": "wrote 5 bytes"});
        assert_eq!(c.validate_output(&v), Ok(()));
    }

    #[test]
    fn invalid_output_fails_validation() {
        let c = fs_write_contract();
        // ok is missing.
        let v = serde_json::json!({});
        let errs = c.validate_output(&v).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| e.contains("missing required field `ok`"))
        );
    }

    #[test]
    fn unknown_type_keyword_passes_through() {
        // Some schemas use non-spec types; we don't reject
        // the value just because we don't understand the
        // type keyword.
        let c = ToolContract::new(
            "tool.weird",
            serde_json::json!({"type": "alien"}),
            serde_json::json!({}),
        );
        let v = serde_json::json!({"foo": "bar"});
        assert_eq!(c.validate_input(&v), Ok(()));
    }

    #[test]
    fn schema_with_no_type_or_required_is_a_passthrough() {
        let c = ToolContract::new(
            "tool.anything",
            serde_json::json!({}),
            serde_json::json!({}),
        );
        assert_eq!(c.validate_input(&serde_json::json!("any value")), Ok(()));
        assert_eq!(c.validate_output(&serde_json::json!(123)), Ok(()));
    }

    #[test]
    fn validation_walks_into_nested_properties() {
        let c = ToolContract::new(
            "nested",
            serde_json::json!({
                "type": "object",
                "required": ["payload"],
                "properties": {
                    "payload": {
                        "type": "object",
                        "required": ["inner"],
                        "properties": {
                            "inner": { "type": "string" }
                        }
                    }
                }
            }),
            serde_json::json!({}),
        );
        // payload.inner is a number — should fail.
        let v = serde_json::json!({"payload": {"inner": 7}});
        let errs = c.validate_input(&v).unwrap_err();
        assert!(errs.iter().any(|e| e.contains("$.payload.inner")));
        assert!(errs.iter().any(|e| e.contains("expected type `string`")));
    }
}
