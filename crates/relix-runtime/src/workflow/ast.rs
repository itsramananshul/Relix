//! Typed workflow definition. The parser produces this from
//! YAML; the validator type-checks it; the executor drives
//! it.

use std::collections::BTreeMap;

/// A complete workflow definition.
#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    /// Stable operator-visible name. Used to look up
    /// `.workflow` files from the workflows directory.
    pub name: String,
    /// Schema version. Must be `1` today.
    pub version: u32,
    /// Optional one-line description shown by
    /// `workflow.list`.
    pub description: String,
    /// Agent step catalog keyed by step name. Step names
    /// are used in `flow.start`, `flow.edges`, and as the
    /// `<step>.output` prefix in variable interpolation.
    /// `BTreeMap` for deterministic iteration order (tests
    /// pin trace ordering against this).
    pub agents: BTreeMap<String, AgentSpec>,
    /// Graph definition — entry point, edges, optional
    /// final result expression.
    pub flow: FlowGraph,
}

/// One agent step. Dispatches a single capability call
/// against the named peer with the interpolated input and
/// stores the response under `output`.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSpec {
    /// Peer alias as resolved through `peers.toml` (e.g.
    /// `"ai"`, `"research-agent"`).
    pub peer: String,
    /// Capability method name (e.g. `"ai.chat"`).
    pub capability: String,
    /// Input template. Supports `{{workflow.input}}` and
    /// `{{<other_step>.output}}` interpolation. Emitted to
    /// the dispatcher as UTF-8 bytes after substitution.
    pub input: String,
    /// Output binding name. Other steps reference this
    /// step's response via `{{<step_name>.output}}`. By
    /// convention but not enforced: equal to the step name
    /// or a shorter alias.
    pub output: String,
}

/// The flow graph: entry point, ordered edges, optional
/// result expression.
#[derive(Debug, Clone, PartialEq)]
pub struct FlowGraph {
    /// Name of the agent step the executor visits first.
    pub start: String,
    /// Edges in source order. The executor scans this list
    /// for each completed step; matching edges fire in the
    /// order they appear. For `EdgeCondition::Parallel`,
    /// all matching edges from the same source fire
    /// concurrently.
    pub edges: Vec<Edge>,
    /// Final result expression. Supports the same
    /// `{{workflow.input}}` / `{{<step>.output}}`
    /// interpolation as agent inputs. When `None` the
    /// workflow result is the output of the last step that
    /// ran.
    pub result: Option<String>,
}

/// One directed edge between two agent steps.
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    /// Source step name.
    pub from: String,
    /// Destination step name.
    pub to: String,
    /// Fire condition.
    pub condition: EdgeCondition,
}

/// When an edge fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeCondition {
    /// Fires when the source step completed with a
    /// non-error response.
    Success,
    /// Fires when the source step's dispatch returned an
    /// error response.
    Failure,
    /// Fires unconditionally (success OR failure path; used
    /// for cleanup / always-run steps).
    Always,
    /// Fires alongside other `Parallel` edges from the same
    /// source — all parallel targets execute concurrently
    /// after the source resolves successfully.
    Parallel,
}

impl EdgeCondition {
    /// Parse a `condition:` field from YAML. Returns `None`
    /// on unknown values so the parser can emit a precise
    /// schema error pointing at the bad scalar. Named
    /// `parse` instead of `from_str` to avoid colliding
    /// with the `std::str::FromStr` trait shape (clippy
    /// flags the trait-look-alike name).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "success" => Some(Self::Success),
            "failure" => Some(Self::Failure),
            "always" => Some(Self::Always),
            "parallel" => Some(Self::Parallel),
            _ => None,
        }
    }

    /// Canonical YAML rendering (matches `from_str`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Always => "always",
            Self::Parallel => "parallel",
        }
    }
}
