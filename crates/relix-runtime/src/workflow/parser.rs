//! Workflow YAML → typed AST. Built on `saphyr::MarkedYamlOwned`
//! so every schema error carries the exact (line, column) of
//! the offending node in the source.

use std::collections::BTreeMap;

use saphyr::{LoadableYamlNode, MarkedYamlOwned, ScalarOwned, YamlDataOwned};

use super::ast::{AgentSpec, Edge, EdgeCondition, FlowGraph, Workflow};

/// Workflow parse / schema error. Always carries a 1-based
/// `(line, column)` so operators can locate the bug in their
/// `.workflow` file directly. `line == 0` only when the
/// source had no position info (an empty document, for
/// example).
#[derive(Debug, Clone, thiserror::Error)]
#[error("workflow parse error at line {line}, column {column}: {message}")]
pub struct ParseError {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl ParseError {
    fn new(node: Option<&MarkedYamlOwned>, message: impl Into<String>) -> Self {
        let (line, column) = node.map_or((0, 0), node_pos);
        Self {
            line,
            column,
            message: message.into(),
        }
    }
}

/// Parse a workflow source string. Returns the typed
/// [`Workflow`] on success or a [`ParseError`] pointing at
/// the offending node.
pub fn parse_str(source: &str) -> Result<Workflow, ParseError> {
    let docs = MarkedYamlOwned::load_from_str(source).map_err(|e| {
        let marker = e.marker();
        ParseError {
            line: marker.line(),
            column: marker.col(),
            message: e.info().to_string(),
        }
    })?;
    let root = docs
        .first()
        .ok_or_else(|| ParseError::new(None, "workflow source is empty"))?;
    parse_root(root)
}

fn parse_root(root: &MarkedYamlOwned) -> Result<Workflow, ParseError> {
    expect_mapping(root, "root")?;
    deny_unknown_root_fields(root)?;

    let name = required_string(root, "name")?;
    let version = required_u32(root, "version")?;
    if version != 1 {
        return Err(ParseError::new(
            root.data.as_mapping_get("version"),
            format!("workflow version must be 1; got {version}"),
        ));
    }
    let description = optional_string(root, "description")?.unwrap_or_default();

    let agents_node = root
        .data
        .as_mapping_get("agents")
        .ok_or_else(|| ParseError::new(Some(root), "missing required top-level field `agents`"))?;
    let agents = parse_agents(agents_node)?;

    let flow_node = root
        .data
        .as_mapping_get("flow")
        .ok_or_else(|| ParseError::new(Some(root), "missing required top-level field `flow`"))?;
    let flow = parse_flow(flow_node)?;

    Ok(Workflow {
        name,
        version,
        description,
        agents,
        flow,
    })
}

fn parse_agents(value: &MarkedYamlOwned) -> Result<BTreeMap<String, AgentSpec>, ParseError> {
    expect_mapping(value, "agents")?;
    let map = value
        .data
        .as_mapping()
        .expect("expect_mapping validated the node is a mapping");
    let mut out = BTreeMap::new();
    for (key_node, agent_node) in map.iter() {
        let name = scalar_string(key_node)
            .ok_or_else(|| ParseError::new(Some(key_node), "agent name must be a scalar string"))?;
        if name.is_empty() {
            return Err(ParseError::new(
                Some(key_node),
                "agent name cannot be empty",
            ));
        }
        let spec = parse_agent_spec(agent_node, &name)?;
        if out.insert(name.clone(), spec).is_some() {
            return Err(ParseError::new(
                Some(key_node),
                format!("agent name `{name}` declared twice"),
            ));
        }
    }
    if out.is_empty() {
        return Err(ParseError::new(
            Some(value),
            "agents map must contain at least one agent",
        ));
    }
    Ok(out)
}

fn parse_agent_spec(value: &MarkedYamlOwned, agent_name: &str) -> Result<AgentSpec, ParseError> {
    expect_mapping(value, &format!("agent `{agent_name}` body"))?;
    deny_unknown_agent_fields(value, agent_name)?;
    let peer = required_string_in(value, "peer", agent_name)?;
    let capability = required_string_in(value, "capability", agent_name)?;
    let input = required_string_in(value, "input", agent_name)?;
    let output = required_string_in(value, "output", agent_name)?;
    Ok(AgentSpec {
        peer,
        capability,
        input,
        output,
    })
}

fn parse_flow(value: &MarkedYamlOwned) -> Result<FlowGraph, ParseError> {
    expect_mapping(value, "flow")?;
    deny_unknown_flow_fields(value)?;
    let start = required_string(value, "start")?;
    let edges_node = value.data.as_mapping_get("edges");
    let edges = match edges_node {
        Some(n) => parse_edges(n)?,
        None => Vec::new(),
    };
    let result = optional_string(value, "result")?;
    Ok(FlowGraph {
        start,
        edges,
        result,
    })
}

fn parse_edges(value: &MarkedYamlOwned) -> Result<Vec<Edge>, ParseError> {
    let seq = match &value.data {
        YamlDataOwned::Sequence(s) => s,
        _ => {
            return Err(ParseError::new(
                Some(value),
                "flow.edges must be a sequence of edge mappings",
            ));
        }
    };
    let mut out = Vec::with_capacity(seq.len());
    for (i, edge_node) in seq.iter().enumerate() {
        out.push(parse_edge(edge_node, i)?);
    }
    Ok(out)
}

fn parse_edge(value: &MarkedYamlOwned, index: usize) -> Result<Edge, ParseError> {
    expect_mapping(value, &format!("edges[{index}]"))?;
    deny_unknown_edge_fields(value, index)?;
    let from = required_string(value, "from")?;
    let to = required_string(value, "to")?;
    let cond_str = required_string(value, "condition")?;
    let condition = EdgeCondition::parse(&cond_str).ok_or_else(|| {
        ParseError::new(
            value.data.as_mapping_get("condition"),
            format!(
                "edges[{index}].condition must be one of: success, failure, always, parallel; got `{cond_str}`"
            ),
        )
    })?;
    Ok(Edge {
        from,
        to,
        condition,
    })
}

// ─────────────────────── helpers ─────────────────────────────

/// Walk to the first available source position. `MarkedYamlOwned`
/// stores spans only on scalars in saphyr 0.0.6; collection
/// nodes default to (0, 0). Falls back to the first child
/// position so collection errors still surface a useful line.
fn node_pos(n: &MarkedYamlOwned) -> (usize, usize) {
    let direct = (n.span.start.line(), n.span.start.col());
    if direct.0 > 0 {
        return direct;
    }
    match &n.data {
        YamlDataOwned::Mapping(m) => {
            if let Some((k, _)) = m.iter().next() {
                let from_key = (k.span.start.line(), k.span.start.col());
                if from_key.0 > 0 {
                    return from_key;
                }
            }
            direct
        }
        YamlDataOwned::Sequence(seq) => {
            if let Some(first) = seq.first() {
                let from_first = (first.span.start.line(), first.span.start.col());
                if from_first.0 > 0 {
                    return from_first;
                }
            }
            direct
        }
        YamlDataOwned::Tagged(_, inner) => node_pos(inner),
        _ => direct,
    }
}

/// Validate that `value` is a mapping. Returns `()` on
/// success; callers that need the inner map can then call
/// `value.data.as_mapping()` and unwrap. The two-step shape
/// keeps `hashlink::LinkedHashMap` out of our public surface
/// (it's a transitive dep through saphyr).
fn expect_mapping(value: &MarkedYamlOwned, what: &str) -> Result<(), ParseError> {
    if value.data.as_mapping().is_some() {
        Ok(())
    } else {
        Err(ParseError::new(
            Some(value),
            format!("{what} must be a mapping"),
        ))
    }
}

fn scalar_string(node: &MarkedYamlOwned) -> Option<String> {
    match &node.data {
        YamlDataOwned::Value(ScalarOwned::String(s)) => Some(s.clone()),
        YamlDataOwned::Value(ScalarOwned::Integer(i)) => Some(i.to_string()),
        YamlDataOwned::Value(ScalarOwned::Boolean(b)) => Some(b.to_string()),
        YamlDataOwned::Representation(s, _, _) => Some(s.clone()),
        _ => None,
    }
}

fn required_string(parent: &MarkedYamlOwned, key: &str) -> Result<String, ParseError> {
    let node = parent
        .data
        .as_mapping_get(key)
        .ok_or_else(|| ParseError::new(Some(parent), format!("missing required field `{key}`")))?;
    scalar_string(node).ok_or_else(|| {
        ParseError::new(Some(node), format!("field `{key}` must be a scalar string"))
    })
}

fn required_string_in(
    parent: &MarkedYamlOwned,
    key: &str,
    context: &str,
) -> Result<String, ParseError> {
    let node = parent.data.as_mapping_get(key).ok_or_else(|| {
        ParseError::new(
            Some(parent),
            format!("`{context}`: missing required field `{key}`"),
        )
    })?;
    scalar_string(node).ok_or_else(|| {
        ParseError::new(
            Some(node),
            format!("`{context}`: field `{key}` must be a scalar string"),
        )
    })
}

fn optional_string(parent: &MarkedYamlOwned, key: &str) -> Result<Option<String>, ParseError> {
    match parent.data.as_mapping_get(key) {
        Some(node) => match &node.data {
            YamlDataOwned::Value(ScalarOwned::Null) => Ok(None),
            _ => Ok(Some(scalar_string(node).ok_or_else(|| {
                ParseError::new(Some(node), format!("field `{key}` must be a scalar string"))
            })?)),
        },
        None => Ok(None),
    }
}

fn required_u32(parent: &MarkedYamlOwned, key: &str) -> Result<u32, ParseError> {
    let node = parent
        .data
        .as_mapping_get(key)
        .ok_or_else(|| ParseError::new(Some(parent), format!("missing required field `{key}`")))?;
    match &node.data {
        YamlDataOwned::Value(ScalarOwned::Integer(v)) if *v >= 0 && *v <= u32::MAX as i64 => {
            Ok(*v as u32)
        }
        YamlDataOwned::Value(ScalarOwned::String(s)) => s.parse::<u32>().map_err(|_| {
            ParseError::new(
                Some(node),
                format!("field `{key}` must be a non-negative integer (got `{s}`)"),
            )
        }),
        YamlDataOwned::Representation(s, _, _) => s.parse::<u32>().map_err(|_| {
            ParseError::new(
                Some(node),
                format!("field `{key}` must be a non-negative integer (got `{s}`)"),
            )
        }),
        _ => Err(ParseError::new(
            Some(node),
            format!("field `{key}` must be a non-negative integer"),
        )),
    }
}

fn deny_unknown_root_fields(root: &MarkedYamlOwned) -> Result<(), ParseError> {
    let allowed = ["name", "version", "description", "agents", "flow"];
    deny_unknown_fields(root, &allowed, "root")
}

fn deny_unknown_agent_fields(node: &MarkedYamlOwned, agent_name: &str) -> Result<(), ParseError> {
    let allowed = ["peer", "capability", "input", "output"];
    deny_unknown_fields(node, &allowed, &format!("agent `{agent_name}`"))
}

fn deny_unknown_flow_fields(node: &MarkedYamlOwned) -> Result<(), ParseError> {
    let allowed = ["start", "edges", "result"];
    deny_unknown_fields(node, &allowed, "flow")
}

fn deny_unknown_edge_fields(node: &MarkedYamlOwned, index: usize) -> Result<(), ParseError> {
    let allowed = ["from", "to", "condition"];
    deny_unknown_fields(node, &allowed, &format!("edges[{index}]"))
}

fn deny_unknown_fields(
    node: &MarkedYamlOwned,
    allowed: &[&str],
    what: &str,
) -> Result<(), ParseError> {
    let map = match node.data.as_mapping() {
        Some(m) => m,
        None => return Ok(()),
    };
    for (k, _) in map.iter() {
        let name = scalar_string(k).ok_or_else(|| {
            ParseError::new(Some(k), format!("`{what}` field names must be strings"))
        })?;
        if !allowed.contains(&name.as_str()) {
            return Err(ParseError::new(
                Some(k),
                format!(
                    "unknown `{what}` field `{name}` (allowed: {})",
                    allowed.join(", ")
                ),
            ));
        }
    }
    Ok(())
}
