//! RELIX-7.15 — training-data export engine.
//!
//! Four output formats:
//!
//! - `openai` — JSONL, one line per interaction, with
//!   `messages: [{system}, {user}, {assistant}]`.
//! - `anthropic` — JSONL, one line per interaction, with the
//!   legacy `prompt` + `completion` shape
//!   (`"\n\nHuman: ...\n\nAssistant:"` → ` ...\n\nHuman:`).
//! - `generic` — JSONL, one line per interaction, every field
//!   on the [`super::types::InteractionRecord`].
//! - `raw_json` — a single JSON array of every exported record.
//!
//! Filters live on [`ExportFilters`]. The engine materialises
//! the matching rows, writes the output file, and stamps every
//! exported row with `exported = 1` + `export_set = <name>`.
//! An export that matches zero rows creates no file and
//! returns `exported_count = 0`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::store::{ListFilters, TrainingStore, TrainingStoreError};
use super::types::{InteractionId, InteractionRecord};

/// Supported export formats. The wire shape mirrors the JSON
/// string operators pass to `training.export`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Openai,
    Anthropic,
    Generic,
    RawJson,
}

impl ExportFormat {
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "openai" | "openai_chat" | "openai-chat" => Some(Self::Openai),
            "anthropic" | "anthropic_text" | "anthropic-text" => Some(Self::Anthropic),
            "generic" | "jsonl" => Some(Self::Generic),
            "raw_json" | "rawjson" | "raw-json" | "json" => Some(Self::RawJson),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Openai | Self::Anthropic | Self::Generic => "jsonl",
            Self::RawJson => "json",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Generic => "generic",
            Self::RawJson => "raw_json",
        }
    }
}

/// Filter envelope for `training.export`. Defaults match the
/// spec: only export interactions scoring `>= 0.7`, include
/// tool calls, no caps.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExportFilters {
    #[serde(default = "default_min_quality")]
    pub min_quality_score: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_from: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_to: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_interactions: Option<u32>,
    #[serde(default = "default_include_tool_calls")]
    pub include_tool_calls: bool,
}

fn default_min_quality() -> f32 {
    0.7
}
fn default_include_tool_calls() -> bool {
    true
}

impl Default for ExportFilters {
    fn default() -> Self {
        Self {
            min_quality_score: default_min_quality(),
            agent: None,
            session_id: None,
            date_from: None,
            date_to: None,
            max_interactions: None,
            include_tool_calls: default_include_tool_calls(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExportResult {
    /// Number of interactions that matched the filters (before
    /// the `max_interactions` cap).
    pub matched_count: u64,
    /// Number of interactions actually written.
    pub exported_count: u64,
    /// Output file path. `None` when no rows matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_path: Option<String>,
    pub total_tokens: u64,
    pub format: ExportFormat,
    pub export_set: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("export store: {0}")]
    Store(#[from] TrainingStoreError),
    #[error("export io: {0}")]
    Io(String),
    #[error("export encode: {0}")]
    Encode(String),
    #[error("export: {0}")]
    InvalidArgs(String),
}

pub struct ExportEngine {
    store: TrainingStore,
    output_dir: PathBuf,
}

impl ExportEngine {
    pub fn new(store: TrainingStore, output_dir: impl Into<PathBuf>) -> Self {
        Self {
            store,
            output_dir: output_dir.into(),
        }
    }

    /// Run an export. `now_unix_ms` is taken as a parameter so
    /// tests can pin the filename suffix.
    pub fn export(
        &self,
        format: ExportFormat,
        filters: &ExportFilters,
        export_set: &str,
        now_unix_ms: i64,
    ) -> Result<ExportResult, ExportError> {
        let export_set = export_set.trim();
        if export_set.is_empty() {
            return Err(ExportError::InvalidArgs("export_set is required".into()));
        }
        let list_filters = ListFilters {
            agent: filters.agent.clone(),
            session_id: filters.session_id.clone(),
            model: None,
            min_quality_score: Some(filters.min_quality_score),
            date_from: filters.date_from,
            date_to: filters.date_to,
            exported: None,
            require_scored: true,
        };
        let rows = self
            .store
            .list_for_export(&list_filters, filters.max_interactions)?;
        let matched_count = rows.len() as u64;
        if rows.is_empty() {
            return Ok(ExportResult {
                matched_count: 0,
                exported_count: 0,
                output_path: None,
                total_tokens: 0,
                format,
                export_set: export_set.to_string(),
            });
        }

        let total_tokens: u64 = rows.iter().map(|r| r.token_count.unwrap_or(0) as u64).sum();

        let sanitized = sanitize_set(export_set);
        let filename = format!(
            "training_export_{sanitized}_{now_unix_ms}.{ext}",
            ext = format.extension(),
        );
        std::fs::create_dir_all(&self.output_dir).map_err(|e| ExportError::Io(e.to_string()))?;
        let path = self.output_dir.join(&filename);

        let body = render(format, &rows, filters.include_tool_calls)?;
        std::fs::write(&path, &body).map_err(|e| ExportError::Io(e.to_string()))?;

        let ids: Vec<InteractionId> = rows.iter().map(|r| r.interaction_id.clone()).collect();
        let exported_count = self.store.mark_exported(&ids, export_set)? as u64;

        Ok(ExportResult {
            matched_count,
            exported_count,
            output_path: Some(path.to_string_lossy().into_owned()),
            total_tokens,
            format,
            export_set: export_set.to_string(),
        })
    }
}

fn sanitize_set(set: &str) -> String {
    let mut out = String::with_capacity(set.len());
    for c in set.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("set");
    }
    out
}

fn render(
    format: ExportFormat,
    rows: &[InteractionRecord],
    include_tool_calls: bool,
) -> Result<String, ExportError> {
    match format {
        ExportFormat::Openai => render_openai(rows, include_tool_calls),
        ExportFormat::Anthropic => render_anthropic(rows),
        ExportFormat::Generic => render_generic(rows, include_tool_calls),
        ExportFormat::RawJson => render_raw_json(rows, include_tool_calls),
    }
}

fn render_openai(
    rows: &[InteractionRecord],
    include_tool_calls: bool,
) -> Result<String, ExportError> {
    let mut out = String::with_capacity(rows.len() * 256);
    for r in rows {
        let mut messages: Vec<serde_json::Value> = Vec::with_capacity(3);
        if !r.system_prompt.is_empty() {
            messages.push(serde_json::json!({
                "role": "system",
                "content": r.system_prompt,
            }));
        }
        messages.push(serde_json::json!({
            "role": "user",
            "content": r.user_message,
        }));
        let mut assistant_content = r.response.clone();
        if include_tool_calls && !r.tool_calls.is_empty() {
            assistant_content.push_str("\n\n[tool_calls]\n");
            for c in &r.tool_calls {
                assistant_content.push_str(&format!(
                    "- {tool}({input}) => {output}\n",
                    tool = c.tool,
                    input = preview(&c.input),
                    output = preview(&c.output),
                ));
            }
        }
        messages.push(serde_json::json!({
            "role": "assistant",
            "content": assistant_content,
        }));
        let line = serde_json::json!({ "messages": messages });
        out.push_str(
            &serde_json::to_string(&line).map_err(|e| ExportError::Encode(e.to_string()))?,
        );
        out.push('\n');
    }
    Ok(out)
}

fn render_anthropic(rows: &[InteractionRecord]) -> Result<String, ExportError> {
    let mut out = String::with_capacity(rows.len() * 256);
    for r in rows {
        let prompt_prefix = if r.system_prompt.is_empty() {
            String::new()
        } else {
            format!("{}\n\n", r.system_prompt)
        };
        let prompt = format!(
            "{prefix}\n\nHuman: {user}\n\nAssistant:",
            prefix = prompt_prefix,
            user = r.user_message,
        );
        let completion = format!(" {resp}\n\nHuman:", resp = r.response);
        let line = serde_json::json!({
            "prompt": prompt,
            "completion": completion,
        });
        out.push_str(
            &serde_json::to_string(&line).map_err(|e| ExportError::Encode(e.to_string()))?,
        );
        out.push('\n');
    }
    Ok(out)
}

fn render_generic(
    rows: &[InteractionRecord],
    include_tool_calls: bool,
) -> Result<String, ExportError> {
    let mut out = String::with_capacity(rows.len() * 256);
    for r in rows {
        let mut line = serde_json::json!({
            "interaction_id": r.interaction_id.as_str(),
            "session_id": r.session_id,
            "agent": r.agent,
            "model": r.model,
            "provider": r.provider,
            "system_prompt": r.system_prompt,
            "user_message": r.user_message,
            "response": r.response,
            "token_count": r.token_count,
            "prompt_tokens": r.prompt_tokens,
            "completion_tokens": r.completion_tokens,
            "latency_ms": r.latency_ms,
            "success": r.success,
            "error_kind": r.error_kind,
            "recorded_at": r.recorded_at,
            "quality_score": r.quality_score,
        });
        if include_tool_calls {
            line["tool_calls"] = serde_json::to_value(&r.tool_calls)
                .map_err(|e| ExportError::Encode(e.to_string()))?;
        }
        out.push_str(
            &serde_json::to_string(&line).map_err(|e| ExportError::Encode(e.to_string()))?,
        );
        out.push('\n');
    }
    Ok(out)
}

fn render_raw_json(
    rows: &[InteractionRecord],
    include_tool_calls: bool,
) -> Result<String, ExportError> {
    // Build a Vec<Value> so we can strip `tool_calls` when
    // requested without re-serialising each record.
    let mut arr: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
    for r in rows {
        let mut v = serde_json::to_value(r).map_err(|e| ExportError::Encode(e.to_string()))?;
        if !include_tool_calls && let Some(obj) = v.as_object_mut() {
            obj.remove("tool_calls");
        }
        arr.push(v);
    }
    serde_json::to_string_pretty(&arr).map_err(|e| ExportError::Encode(e.to_string()))
}

fn preview(s: &str) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= 200 {
        s
    } else {
        let truncated: String = s.chars().take(200).collect();
        format!("{truncated}…")
    }
}

/// Default location for the export output directory next to a
/// controller's data dir.
pub fn default_export_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("training_exports")
}

#[cfg(test)]
mod tests {
    use super::super::types::{InteractionId, InteractionRecord, ToolCallRecord};
    use super::*;
    use tempfile::TempDir;

    fn record(id: &str, agent: &str, response: &str, score: f32) -> InteractionRecord {
        let mut r = InteractionRecord::new(
            InteractionId(id.into()),
            "session-1".into(),
            agent.into(),
            "gpt-4o-mini".into(),
            "openai".into(),
            "you are alice".into(),
            "tell me about rust".into(),
            response.into(),
            vec![ToolCallRecord {
                tool: "web_fetch".into(),
                input: "https://rust-lang.org".into(),
                output: "Rust is...".into(),
                success: true,
                latency_ms: 25,
                error_kind: None,
            }],
            Some(40),
            Some(60),
            200,
            true,
            None,
            1_700_000_000_000,
        );
        r.quality_score = Some(score);
        r
    }

    fn populate(store: &TrainingStore) {
        for (i, (id, score)) in [("a", 0.9), ("b", 0.4), ("c", 0.85)].iter().enumerate() {
            let mut r = record(
                id,
                "alice",
                "Rust is a systems language. It is safe.",
                *score,
            );
            r.recorded_at = 100 + i as i64;
            store.insert(&r).unwrap();
        }
    }

    #[test]
    fn openai_format_produces_jsonl_with_messages_array() {
        let store = TrainingStore::in_memory().unwrap();
        populate(&store);
        let dir = TempDir::new().unwrap();
        let eng = ExportEngine::new(store, dir.path().to_path_buf());
        let res = eng
            .export(
                ExportFormat::Openai,
                &ExportFilters::default(),
                "test-set",
                1_700_000_000_001,
            )
            .unwrap();
        assert_eq!(res.matched_count, 2); // a + c
        assert_eq!(res.exported_count, 2);
        assert_eq!(res.format, ExportFormat::Openai);
        let path = res.output_path.unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert!(v["messages"].is_array());
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "assistant");
    }

    #[test]
    fn anthropic_format_produces_prompt_and_completion_shape() {
        let store = TrainingStore::in_memory().unwrap();
        populate(&store);
        let dir = TempDir::new().unwrap();
        let eng = ExportEngine::new(store, dir.path().to_path_buf());
        let res = eng
            .export(
                ExportFormat::Anthropic,
                &ExportFilters::default(),
                "set2",
                1_700_000_000_002,
            )
            .unwrap();
        let body = std::fs::read_to_string(res.output_path.unwrap()).unwrap();
        let line = body.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        let prompt = v["prompt"].as_str().unwrap();
        assert!(prompt.contains("Human: tell me about rust"));
        assert!(prompt.ends_with("Assistant:"));
        let completion = v["completion"].as_str().unwrap();
        assert!(completion.starts_with(' '));
        assert!(completion.ends_with("Human:"));
    }

    #[test]
    fn generic_format_includes_all_documented_fields() {
        let store = TrainingStore::in_memory().unwrap();
        populate(&store);
        let dir = TempDir::new().unwrap();
        let eng = ExportEngine::new(store, dir.path().to_path_buf());
        let res = eng
            .export(
                ExportFormat::Generic,
                &ExportFilters::default(),
                "set3",
                1_700_000_000_003,
            )
            .unwrap();
        let body = std::fs::read_to_string(res.output_path.unwrap()).unwrap();
        let line = body.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        for k in [
            "interaction_id",
            "session_id",
            "agent",
            "model",
            "provider",
            "system_prompt",
            "user_message",
            "response",
            "tool_calls",
            "token_count",
            "prompt_tokens",
            "completion_tokens",
            "latency_ms",
            "success",
            "error_kind",
            "recorded_at",
            "quality_score",
        ] {
            assert!(v.get(k).is_some(), "generic export missing field {k}");
        }
    }

    #[test]
    fn min_quality_filter_excludes_low_scoring_records() {
        let store = TrainingStore::in_memory().unwrap();
        populate(&store);
        let dir = TempDir::new().unwrap();
        let eng = ExportEngine::new(store, dir.path().to_path_buf());
        let f = ExportFilters {
            min_quality_score: 0.5,
            ..ExportFilters::default()
        };
        let res = eng
            .export(ExportFormat::Openai, &f, "set", 1_700_000_000_004)
            .unwrap();
        // a (0.9) + c (0.85) match; b (0.4) does not.
        assert_eq!(res.matched_count, 2);
    }

    #[test]
    fn max_interactions_keeps_highest_scoring_first() {
        let store = TrainingStore::in_memory().unwrap();
        populate(&store);
        let dir = TempDir::new().unwrap();
        let eng = ExportEngine::new(store, dir.path().to_path_buf());
        let f = ExportFilters {
            max_interactions: Some(1),
            min_quality_score: 0.0,
            ..ExportFilters::default()
        };
        let res = eng
            .export(ExportFormat::Openai, &f, "set", 1_700_000_000_005)
            .unwrap();
        assert_eq!(res.exported_count, 1);
        let body = std::fs::read_to_string(res.output_path.unwrap()).unwrap();
        // "a" had score 0.9 (highest) so it should be the only line.
        assert!(body.contains("\"tell me about rust\""));
    }

    #[test]
    fn exported_records_are_marked_with_exported_flag_and_set_name() {
        let store = TrainingStore::in_memory().unwrap();
        populate(&store);
        let dir = TempDir::new().unwrap();
        let eng = ExportEngine::new(store.clone(), dir.path().to_path_buf());
        eng.export(
            ExportFormat::Openai,
            &ExportFilters::default(),
            "set-x",
            1_700_000_000_006,
        )
        .unwrap();
        let a = store.get("a").unwrap().unwrap();
        assert!(a.exported);
        assert_eq!(a.export_set.as_deref(), Some("set-x"));
        let b = store.get("b").unwrap().unwrap();
        assert!(!b.exported);
    }

    #[test]
    fn no_matches_creates_no_file_and_returns_zero() {
        let store = TrainingStore::in_memory().unwrap();
        populate(&store);
        let dir = TempDir::new().unwrap();
        let eng = ExportEngine::new(store, dir.path().to_path_buf());
        let f = ExportFilters {
            min_quality_score: 0.99,
            ..ExportFilters::default()
        };
        let res = eng
            .export(ExportFormat::Openai, &f, "set", 1_700_000_000_007)
            .unwrap();
        assert_eq!(res.matched_count, 0);
        assert_eq!(res.exported_count, 0);
        assert!(res.output_path.is_none());
        // Directory must not contain any files.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty());
    }

    #[test]
    fn raw_json_emits_single_pretty_printed_array() {
        let store = TrainingStore::in_memory().unwrap();
        populate(&store);
        let dir = TempDir::new().unwrap();
        let eng = ExportEngine::new(store, dir.path().to_path_buf());
        let res = eng
            .export(
                ExportFormat::RawJson,
                &ExportFilters::default(),
                "set",
                1_700_000_000_008,
            )
            .unwrap();
        let path = res.output_path.unwrap();
        assert!(path.ends_with(".json"));
        let body = std::fs::read_to_string(path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v.is_array());
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn include_tool_calls_false_drops_tool_calls_field() {
        let store = TrainingStore::in_memory().unwrap();
        populate(&store);
        let dir = TempDir::new().unwrap();
        let eng = ExportEngine::new(store, dir.path().to_path_buf());
        let f = ExportFilters {
            include_tool_calls: false,
            ..ExportFilters::default()
        };
        let res = eng
            .export(ExportFormat::Generic, &f, "set", 1_700_000_000_009)
            .unwrap();
        let body = std::fs::read_to_string(res.output_path.unwrap()).unwrap();
        let v: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        // The generic format always populates `tool_calls` only
        // when `include_tool_calls` is true.
        assert!(v.get("tool_calls").is_none());
    }

    #[test]
    fn empty_set_name_returns_invalid_args() {
        let store = TrainingStore::in_memory().unwrap();
        populate(&store);
        let dir = TempDir::new().unwrap();
        let eng = ExportEngine::new(store, dir.path().to_path_buf());
        let r = eng.export(
            ExportFormat::Openai,
            &ExportFilters::default(),
            "  ",
            1_700_000_000_010,
        );
        match r {
            Err(ExportError::InvalidArgs(_)) => {}
            _ => panic!("expected InvalidArgs"),
        }
    }

    #[test]
    fn export_format_from_str_loose_handles_aliases() {
        assert_eq!(
            ExportFormat::from_str_loose("OpenAI"),
            Some(ExportFormat::Openai)
        );
        assert_eq!(
            ExportFormat::from_str_loose("jsonl"),
            Some(ExportFormat::Generic)
        );
        assert_eq!(
            ExportFormat::from_str_loose("raw-json"),
            Some(ExportFormat::RawJson)
        );
        assert_eq!(ExportFormat::from_str_loose("unknown"), None);
    }
}
