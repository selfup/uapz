//! Named, typed values supplied for one execution of a saved graph.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::doc::GraphDoc;
use crate::node_data::NodeData;
use crate::validate::{self, Issue};

pub type Values = BTreeMap<String, String>;
pub const MAX_FIELDS: usize = 64;
pub const MAX_VALUE_BYTES: usize = 256 * 1024;
pub const MAX_VALUES_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputType {
    #[default]
    Text,
    Number,
    Boolean,
    Json,
    File,
}

impl InputType {
    pub const ALL: [Self; 5] = [
        Self::Text,
        Self::Number,
        Self::Boolean,
        Self::Json,
        Self::File,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "Text",
            Self::Number => "Number",
            Self::Boolean => "Boolean",
            Self::Json => "JSON",
            Self::File => "File from data/",
        }
    }
}

/// The node's ordinary Name is also its field name in the Run form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputConfig {
    pub kind: InputType,
    pub default: String,
    pub required: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            kind: InputType::Text,
            default: String::new(),
            required: true,
        }
    }
}

impl InputConfig {
    pub fn summary(&self) -> String {
        format!(
            "{} · {}",
            self.kind.label(),
            if self.required {
                "required"
            } else {
                "optional"
            }
        )
    }

    pub fn validate(&self) -> Vec<Issue> {
        if let Err(e) = validate::text("default", &self.default, MAX_VALUE_BYTES) {
            return vec![Issue::new("default", e)];
        }
        // A required field may intentionally have no default: the Run form supplies it.
        if self.default.trim().is_empty() {
            return Vec::new();
        }
        self.value(&self.default)
            .err()
            .map(|e| Issue::new("default", e))
            .into_iter()
            .collect()
    }

    pub fn value(&self, raw: &str) -> Result<String, String> {
        validate::text("input", raw, MAX_VALUE_BYTES)?;
        if raw.trim().is_empty() {
            return if self.required {
                Err("a value is required".into())
            } else {
                Ok(String::new())
            };
        }
        match self.kind {
            InputType::Text => Ok(raw.to_string()),
            InputType::Number => raw
                .trim()
                .parse::<serde_json::Number>()
                .map(|n| n.to_string())
                .map_err(|_| "enter a finite JSON number".into()),
            InputType::Boolean => match raw.trim() {
                "true" | "false" => Ok(raw.trim().to_string()),
                _ => Err("choose true or false".into()),
            },
            InputType::Json => serde_json::from_str::<serde_json::Value>(raw)
                .map(|v| v.to_string())
                .map_err(|e| format!("invalid JSON: {e}")),
            InputType::File => {
                validate::relative_path(raw)?;
                Ok(raw.trim().to_string())
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Field {
    pub node: String,
    pub name: String,
    pub config: InputConfig,
}

/// Only ask for fields that can affect the requested targets. Walk iteratively, also
/// accepting cyclic graphs here so the executor can report its usual cycle diagnostic.
pub fn fields(doc: &GraphDoc, targets: &[String]) -> Result<Vec<Field>, String> {
    let nodes: HashMap<&str, _> = doc.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let mut incoming: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &doc.edges {
        incoming.entry(&edge.to).or_default().push(&edge.from);
    }
    let mut pending: Vec<&str> = targets.iter().map(String::as_str).collect();
    let mut visited = HashSet::new();
    let mut names = HashSet::new();
    let mut fields = Vec::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        if let Some(from) = incoming.get(id) {
            pending.extend(from);
        }
        let Some(node) = nodes.get(id) else {
            continue;
        };
        let Some(NodeData::Input(config)) = &node.data else {
            continue;
        };
        let name = node.name.as_deref().unwrap_or_default().trim();
        validate::node_name(name)
            .map_err(|e| format!("{id}: give this Run input a valid Name: {e}"))?;
        if !names.insert(name) {
            return Err(format!("more than one Run input is named {name}"));
        }
        if incoming.get(id).is_some_and(|from| !from.is_empty()) {
            return Err(format!(
                "{id}: a Run input is a source; remove its incoming wires"
            ));
        }
        if let Some(issue) = config.validate().first() {
            return Err(format!("{name}: {}", issue.message));
        }
        fields.push(Field {
            node: id.to_string(),
            name: name.to_string(),
            config: config.clone(),
        });
        if fields.len() > MAX_FIELDS {
            return Err(format!("a run can have at most {MAX_FIELDS} inputs"));
        }
    }
    fields.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(fields)
}

/// Apply defaults only for omitted values; an explicitly cleared required field is an
/// error. Validate every field before executing any node or making a provider call.
pub fn resolve(fields: &[Field], supplied: &Values, data: &Path) -> Result<Values, String> {
    if let Some(name) = supplied
        .keys()
        .find(|name| !fields.iter().any(|f| &f.name == *name))
    {
        return Err(format!("unknown run input: {name}"));
    }
    let mut values = Values::new();
    let mut total = 0usize;
    for field in fields {
        let raw = supplied.get(&field.name).unwrap_or(&field.config.default);
        let value = field
            .config
            .value(raw)
            .map_err(|e| format!("{}: {e}", field.name))?;
        total = total.saturating_add(value.len());
        if total > MAX_VALUES_BYTES {
            return Err("run inputs exceed the 1 MB combined limit".into());
        }
        if field.config.kind == InputType::File && !value.is_empty() {
            let path = validate::resolve_inside(data, &value)
                .map_err(|e| format!("{}: {e}", field.name))?;
            let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", field.name))?;
            if !meta.is_file() || meta.len() > crate::exec::MAX_FILE_BYTES {
                return Err(format!(
                    "{}: choose a regular file of at most 64 MB inside data/",
                    field.name
                ));
            }
        }
        values.insert(field.name.clone(), value);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{EdgeDoc, NodeDoc};

    fn input(id: &str, name: &str, kind: InputType, default: &str) -> NodeDoc {
        NodeDoc {
            id: id.into(),
            x: 0.0,
            y: 0.0,
            name: Some(name.into()),
            kind: None,
            color: None,
            data: Some(NodeData::Input(InputConfig {
                kind,
                default: default.into(),
                required: true,
            })),
        }
    }

    #[test]
    fn typed_values_preserve_text_and_validate_structured_inputs() {
        let config = |kind| InputConfig {
            kind,
            ..InputConfig::default()
        };
        assert_eq!(
            config(InputType::Text).value("  question\n").unwrap(),
            "  question\n"
        );
        assert_eq!(config(InputType::Number).value(" 1.25 ").unwrap(), "1.25");
        for value in ["NaN", "Infinity", "1e9999", "01", "1;echo"] {
            assert!(config(InputType::Number).value(value).is_err(), "{value}");
        }
        assert_eq!(config(InputType::Boolean).value("false").unwrap(), "false");
        assert!(config(InputType::Boolean).value("yes").is_err());
        assert_eq!(
            config(InputType::Json)
                .value("{ \"x\": [1, true] }")
                .unwrap(),
            r#"{"x":[1,true]}"#
        );
        assert!(config(InputType::Json).value("{broken").is_err());
        assert!(config(InputType::File).value("../private.txt").is_err());
        assert!(config(InputType::Text).value("\0").is_err());
        assert!(
            config(InputType::Text)
                .value(&"x".repeat(MAX_VALUE_BYTES + 1))
                .is_err()
        );
    }

    #[test]
    fn fields_only_include_reachable_inputs_and_require_unique_names() {
        let mut doc = GraphDoc {
            nodes: vec![
                input("n0", "question", InputType::Text, "hi"),
                input("n1", "unused", InputType::Text, ""),
            ],
            ..GraphDoc::default()
        };
        let reachable = fields(&doc, &["n0".into()]).unwrap();
        assert_eq!(reachable.len(), 1);
        assert_eq!(reachable[0].name, "question");
        doc.nodes[1].name = Some("question".into());
        assert!(
            fields(&doc, &["n0".into(), "n1".into()])
                .unwrap_err()
                .contains("more than one")
        );
        doc.nodes[0].name = None;
        assert!(fields(&doc, &["n0".into()]).unwrap_err().contains("Name"));
        doc.nodes[0].name = Some("question".into());
        doc.edges.push(EdgeDoc {
            from: "n1".into(),
            to: "n0".into(),
        });
        assert!(fields(&doc, &["n0".into()]).unwrap_err().contains("source"));
    }

    #[test]
    fn required_defaults_and_explicit_empty_values_are_distinct() {
        let doc = GraphDoc {
            nodes: vec![input("n0", "question", InputType::Text, "default question")],
            ..GraphDoc::default()
        };
        let mut fields = fields(&doc, &["n0".into()]).unwrap();
        assert_eq!(
            resolve(&fields, &Values::new(), Path::new(".")).unwrap()["question"],
            "default question"
        );
        let empty = Values::from([("question".into(), String::new())]);
        assert!(
            resolve(&fields, &empty, Path::new("."))
                .unwrap_err()
                .contains("required")
        );
        fields[0].config.required = false;
        assert_eq!(
            resolve(&fields, &empty, Path::new(".")).unwrap()["question"],
            ""
        );
        assert!(
            resolve(
                &fields,
                &Values::from([("typo".into(), "x".into())]),
                Path::new(".")
            )
            .is_err()
        );
        assert_eq!(
            doc.nodes[0].data,
            Some(NodeData::Input(InputConfig {
                default: "default question".into(),
                ..InputConfig::default()
            }))
        );
    }

    #[test]
    fn run_input_schema_round_trips_and_limits_are_enforced() {
        let node = input("n0", "sample", InputType::Json, "{}");
        let encoded = serde_json::to_string(&node).unwrap();
        let decoded: NodeDoc = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.name, node.name);
        assert_eq!(decoded.data, node.data);
        let bad = InputConfig {
            default: " ".repeat(MAX_VALUE_BYTES + 1),
            ..InputConfig::default()
        };
        assert!(!bad.validate().is_empty());
        let doc = GraphDoc {
            nodes: (0..=MAX_FIELDS)
                .map(|i| input(&format!("n{i}"), &format!("field{i}"), InputType::Text, ""))
                .collect(),
            ..GraphDoc::default()
        };
        let targets = doc.nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>();
        assert!(fields(&doc, &targets).unwrap_err().contains("at most"));
        let fields = fields(&doc, &targets[..5]).unwrap();
        let values = fields
            .iter()
            .map(|f| (f.name.clone(), "x".repeat(MAX_VALUE_BYTES)))
            .collect();
        assert!(
            resolve(&fields, &values, Path::new("."))
                .unwrap_err()
                .contains("combined limit")
        );
    }
}
