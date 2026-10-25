//! Saved graphs called by Workflow nodes. Preparation is read-only and snapshots the
//! whole reachable call tree before execution, including calls on later branches.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::doc::{self, GraphDoc};
use crate::exec::{self, Key};
use crate::node_data::NodeData;
use crate::validate::{self, Issue};
use crate::workflow::{self, Field, Values};

pub const MAX_DEPTH: usize = 8;
pub const MAX_CALLS: usize = 128;
pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum Binding {
    Value {
        value: String,
    },
    /// A directly wired parent node's id, never a potentially ambiguous display name.
    Output {
        node: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub graph: String,
    pub output: String,
    pub inputs: BTreeMap<String, Binding>,
}

impl Config {
    pub fn summary(&self) -> String {
        if self.graph.is_empty() {
            "choose a saved graph".into()
        } else {
            format!(
                "{} → {}",
                self.graph,
                if self.output.is_empty() {
                    "published outputs"
                } else {
                    &self.output
                }
            )
        }
    }

    pub fn validate(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        if self.graph.is_empty() || validate::file_stem(&self.graph) != self.graph {
            issues.push(Issue::new("workflow", "choose a saved graph name"));
        }
        if !self.output.is_empty() && !validate::is_node_id(&self.output) {
            issues.push(Issue::new("output", "choose an output node"));
        }
        if self.inputs.len() > workflow::MAX_FIELDS {
            issues.push(Issue::new(
                "inputs",
                "at most 64 input mappings are allowed",
            ));
        }
        let mut bytes = 0usize;
        for (name, binding) in &self.inputs {
            if let Err(error) = validate::node_name(name) {
                issues.push(Issue::new("inputs", error));
            }
            match binding {
                Binding::Value { value } => {
                    bytes = bytes.saturating_add(value.len());
                    if let Err(error) = validate::text("value", value, workflow::MAX_VALUE_BYTES) {
                        issues.push(Issue::new("inputs", format!("{name}: {error}")));
                    }
                }
                Binding::Output { node } if !validate::is_output_handle(node) => {
                    issues.push(Issue::new("inputs", format!("{name}: select a wired node")));
                }
                Binding::Output { .. } => {}
            }
        }
        if bytes > workflow::MAX_VALUES_BYTES {
            issues.push(Issue::new(
                "inputs",
                "mapped values exceed the 1 MB combined limit",
            ));
        }
        issues
    }

    /// Check names, defaults, literal types and direct wires before running the parent.
    /// Dynamic wire values are type-checked together just before the child starts.
    pub fn validate_bindings(
        &self,
        fields: &[Field],
        upstream: &[&str],
        data: &Path,
    ) -> Result<(), String> {
        if let Some(name) = self
            .inputs
            .keys()
            .find(|name| !fields.iter().any(|f| &f.name == *name))
        {
            return Err(format!("unknown workflow input: {name}"));
        }
        let mut literal_fields = Vec::new();
        let mut supplied = Values::new();
        for field in fields {
            match self.inputs.get(&field.name) {
                Some(Binding::Output { node }) => {
                    if !upstream.contains(&node.as_str()) {
                        return Err(format!(
                            "{}: {node} must be wired directly into this Workflow node",
                            field.name
                        ));
                    }
                }
                binding => {
                    literal_fields.push(field.clone());
                    if let Some(Binding::Value { value }) = binding {
                        supplied.insert(field.name.clone(), value.clone());
                    }
                }
            }
        }
        workflow::resolve(&literal_fields, &supplied, data).map(|_| ())
    }
}

pub type Plans = BTreeMap<String, Result<Arc<Prepared>, String>>;

#[derive(Debug)]
pub struct Prepared {
    pub graph: Arc<GraphDoc>,
    pub targets: Vec<String>,
    pub fields: Vec<Field>,
    pub calls: Plans,
    pub key: Key,
}

/// Only inspect reachable calls. Errors belong to their Workflow node, so independent
/// top-level targets retain the executor's normal ability to finish after a failure.
pub fn prepare(doc: &GraphDoc, targets: &[String], root: &Path, cancel: &AtomicBool) -> Plans {
    let mut loader = Loader {
        root,
        cancel,
        loaded: HashMap::new(),
        count: 0,
        bytes: 0,
    };
    let mut stack = vec![doc.name.clone()];
    loader.calls(doc, targets, &mut stack)
}

struct Loader<'a> {
    root: &'a Path,
    cancel: &'a AtomicBool,
    loaded: HashMap<String, Arc<GraphDoc>>,
    count: usize,
    bytes: usize,
}

impl Loader<'_> {
    fn calls(&mut self, doc: &GraphDoc, targets: &[String], stack: &mut Vec<String>) -> Plans {
        let nodes: HashMap<_, _> = doc.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let mut incoming: HashMap<&str, Vec<&str>> = HashMap::new();
        let mut incoming_handles: HashMap<&str, Vec<String>> = HashMap::new();
        for edge in &doc.edges {
            incoming.entry(&edge.to).or_default().push(&edge.from);
            incoming_handles
                .entry(&edge.to)
                .or_default()
                .push(edge.handle());
        }
        let mut pending: Vec<&str> = targets.iter().map(String::as_str).collect();
        let mut seen = HashSet::new();
        let mut calls = Plans::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            let upstream_nodes = incoming.get(id).map(Vec::as_slice).unwrap_or_default();
            pending.extend(upstream_nodes);
            let upstream: Vec<&str> = incoming_handles
                .get(id)
                .into_iter()
                .flatten()
                .map(String::as_str)
                .collect();
            if let Some(NodeData::Workflow(config)) = nodes.get(id).and_then(|n| n.data.as_ref()) {
                calls.insert(
                    id.to_string(),
                    self.call(config, &upstream, stack).map(Arc::new),
                );
            }
            if self.cancel.load(Ordering::Relaxed) {
                break;
            }
        }
        calls
    }

    fn call(
        &mut self,
        config: &Config,
        upstream: &[&str],
        stack: &mut Vec<String>,
    ) -> Result<Prepared, String> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(exec::CANCELLED.into());
        }
        if let Some(issue) = config.validate().first() {
            return Err(issue.message.clone());
        }
        if stack.contains(&config.graph) {
            return Err(format!(
                "recursive workflow call: {} → {}",
                stack.join(" → "),
                config.graph
            ));
        }
        if stack.len() > MAX_DEPTH {
            return Err(format!("workflows can nest at most {MAX_DEPTH} calls deep"));
        }
        self.count += 1;
        if self.count > MAX_CALLS {
            return Err(format!(
                "a run can contain at most {MAX_CALLS} workflow calls"
            ));
        }
        if self.bytes >= MAX_SNAPSHOT_BYTES {
            return Err("workflow snapshots exceed the 4 MB combined limit".into());
        }
        let graph = match self.loaded.get(&config.graph) {
            Some(graph) => graph.clone(),
            None => {
                let graph = Arc::new(doc::load_under(self.root, &config.graph)?);
                self.loaded.insert(config.graph.clone(), graph.clone());
                graph
            }
        };
        let bytes = serde_json::to_vec(graph.as_ref()).map_err(|e| e.to_string())?;
        self.bytes = self.bytes.saturating_add(bytes.len());
        if self.bytes > MAX_SNAPSHOT_BYTES {
            return Err("workflow snapshots exceed the 4 MB combined limit".into());
        }
        // This uses the same dependency planner as execution, without performing steps.
        let targets = if config.output.is_empty() {
            if graph.outputs.is_empty() {
                return Err("this graph has no published outputs; publish outputs or choose a legacy output node".into());
            }
            let mut targets: Vec<_> = graph
                .outputs
                .values()
                .map(|handle| {
                    handle
                        .split_once('.')
                        .map_or(handle.as_str(), |(id, _)| id)
                        .to_string()
                })
                .collect();
            targets.sort();
            targets.dedup();
            targets
        } else {
            vec![config.output.clone()]
        };
        if config.output.is_empty() {
            for (name, target) in &graph.outputs {
                validate::output_name(name).map_err(|e| format!("published output {name}: {e}"))?;
                if !validate::is_output_handle(target) {
                    return Err(format!(
                        "published output {name}: choose a node id or named output handle"
                    ));
                }
                let base = target.split_once('.').map_or(target.as_str(), |(id, _)| id);
                if !graph.nodes.iter().any(|node| node.id == base) {
                    return Err(format!(
                        "published output {name}: {target} is not in the graph"
                    ));
                }
            }
            if graph.outputs.len() > workflow::MAX_FIELDS {
                return Err("a graph can publish at most 64 outputs".into());
            }
        }
        for target in &targets {
            exec::preflight(&graph, target, self.cancel)?;
        }
        let fields = workflow::fields(&graph, &targets)?;
        config.validate_bindings(&fields, upstream, &self.root.join(crate::library::DATA_DIR))?;
        stack.push(config.graph.clone());
        let calls = self.calls(&graph, &targets, stack);
        stack.pop();
        let mut keys = Vec::new();
        for (id, prepared) in &calls {
            let child = prepared.as_ref().map_err(|e| format!("{id}: {e}"))?;
            keys.push((id, child.key));
        }
        exec::preflight_wires(&graph, &targets, &calls)?;
        if config.output.is_empty() {
            exec::preflight_publications(&graph, &calls)?;
        }
        let nested = serde_json::to_vec(&keys).map_err(|e| e.to_string())?;
        let key = exec::key_of(&[b"workflow-snapshot-v1", &bytes, &nested]);
        Ok(Prepared {
            graph,
            targets,
            fields,
            calls,
            key,
        })
    }
}

#[cfg(test)]
#[path = "calls_tests.rs"]
mod tests;
