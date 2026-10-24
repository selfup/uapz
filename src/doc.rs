//! On-disk format for a graph.
//!
//! `graphs/<name>.md` holds a mermaid flowchart for humans and other tools, and
//! `graphs/<name>.json` is the sidecar with the full editor state: one JSON block per
//! node (position plus a free-form `data` object), the edges, and the view offset.
//! Loading prefers the sidecar; a lone `.md` is parsed from its mermaid block and laid
//! out automatically.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::node_data::{self, NodeData};
use crate::validate;

/// Directory, relative to the working directory, that holds saved graphs.
pub const DIR: &str = "graphs";
pub const VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GraphDoc {
    #[serde(default = "version")]
    pub version: u32,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub view: View,
    #[serde(default)]
    pub nodes: Vec<NodeDoc>,
    #[serde(default)]
    pub edges: Vec<EdgeDoc>,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct View {
    pub pan_x: f32,
    pub pan_y: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeDoc {
    /// Mermaid-safe identifier, e.g. `n3`.
    pub id: String,
    pub x: f32,
    pub y: f32,
    /// Name of the custom node kind this node was created from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// `#rrggbb` fill, copied from the kind at save time so the graph renders without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Typed content for agent, database, script and data nodes; `{}` for a blank node.
    #[serde(
        default,
        serialize_with = "node_data::serialize_opt",
        deserialize_with = "node_data::deserialize_opt"
    )]
    pub data: Option<NodeData>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeDoc {
    pub from: String,
    pub to: String,
}

fn version() -> u32 {
    VERSION
}

impl GraphDoc {
    /// The diagram as a mermaid flowchart. Nodes are empty, so each gets a blank label.
    pub fn mermaid(&self) -> String {
        let mut out = String::from("flowchart LR\n");
        for node in &self.nodes {
            let label = node.kind.as_deref().map_or(" ".to_string(), mermaid_label);
            out.push_str(&format!("    {}[\"{label}\"]\n", node.id));
        }
        for edge in &self.edges {
            out.push_str(&format!("    {} --> {}\n", edge.from, edge.to));
        }
        for node in &self.nodes {
            let Some((r, g, b)) = node.color.as_deref().and_then(hex_to_rgb) else {
                continue;
            };
            out.push_str(&format!(
                "    style {} fill:{},stroke:{},color:{}\n",
                node.id,
                rgb_to_hex(r, g, b),
                rgb_to_hex(r * 0.75, g * 0.75, b * 0.75),
                if is_light(r, g, b) {
                    "#000000"
                } else {
                    "#ffffff"
                },
            ));
        }
        out
    }

    pub fn markdown(&self) -> String {
        format!(
            "# {name}\n\nLayout and node data live in `{name}.json`.\n\n```mermaid\n{mermaid}```\n",
            name = self.name,
            mermaid = self.mermaid(),
        )
    }
}

/// Turn user input into a file stem that stays inside `graphs/`. See [`validate::file_stem`].
pub fn sanitize_name(raw: &str) -> String {
    validate::file_stem(raw)
}

pub fn paths(name: &str) -> (PathBuf, PathBuf) {
    let dir = Path::new(DIR);
    (
        dir.join(format!("{name}.md")),
        dir.join(format!("{name}.json")),
    )
}

/// Write both files, returning their paths.
pub fn save(doc: &GraphDoc) -> Result<(PathBuf, PathBuf), String> {
    fs::create_dir_all(DIR).map_err(|e| format!("create {DIR}/: {e}"))?;
    let (md, json) = paths(&doc.name);
    fs::write(&md, doc.markdown()).map_err(|e| format!("write {}: {e}", md.display()))?;
    let text = serde_json::to_string_pretty(doc).map_err(|e| e.to_string())?;
    fs::write(&json, text + "\n").map_err(|e| format!("write {}: {e}", json.display()))?;
    Ok((md, json))
}

/// Read a graph by name: the JSON sidecar when present, else the markdown's mermaid block.
pub fn load(name: &str) -> Result<GraphDoc, String> {
    let (md, json) = paths(name);
    let mut doc = if json.exists() {
        let text =
            fs::read_to_string(&json).map_err(|e| format!("read {}: {e}", json.display()))?;
        serde_json::from_str::<GraphDoc>(&text)
            .map_err(|e| format!("parse {}: {e}", json.display()))?
    } else {
        let text = fs::read_to_string(&md).map_err(|e| format!("read {}: {e}", md.display()))?;
        parse_markdown(&text)
    };
    doc.name = name.to_string();
    Ok(doc)
}

/// Names of every saved graph (markdown files in `graphs/`), sorted.
pub fn list() -> Vec<String> {
    let Ok(entries) = fs::read_dir(DIR) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                return None;
            }
            path.file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

// ----- custom node kinds -----

/// Directory, relative to the working directory, that holds saved node kinds.
pub const NODES_DIR: &str = "nodes";

/// A reusable node definition created in the sidebar's node creator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeKind {
    pub name: String,
    /// Fill color as `#rrggbb`.
    pub color: String,
}

pub fn kind_path(name: &str) -> PathBuf {
    Path::new(NODES_DIR).join(format!("{name}.json"))
}

pub fn save_kind(kind: &NodeKind) -> Result<PathBuf, String> {
    fs::create_dir_all(NODES_DIR).map_err(|e| format!("create {NODES_DIR}/: {e}"))?;
    let path = kind_path(&kind.name);
    let text = serde_json::to_string_pretty(kind).map_err(|e| e.to_string())?;
    fs::write(&path, text + "\n").map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

pub fn delete_kind(name: &str) -> Result<(), String> {
    let path = kind_path(name);
    fs::remove_file(&path).map_err(|e| format!("delete {}: {e}", path.display()))
}

/// Every saved kind, sorted by name. Files that don't parse are skipped.
pub fn list_kinds() -> Vec<NodeKind> {
    let Ok(entries) = fs::read_dir(NODES_DIR) else {
        return Vec::new();
    };
    let mut kinds: Vec<NodeKind> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()
        })
        .collect();
    kinds.sort_by(|a, b| a.name.cmp(&b.name));
    kinds
}

// ----- colors -----

/// Parse `#rrggbb` (the `#` is optional) into components in `0.0..=1.0`.
pub fn hex_to_rgb(hex: &str) -> Option<(f32, f32, f32)> {
    let digits = hex.trim().trim_start_matches('#');
    if digits.len() != 6 || !digits.is_ascii() {
        return None;
    }
    let channel = |i: usize| {
        u8::from_str_radix(&digits[i..i + 2], 16)
            .ok()
            .map(|v| v as f32 / 255.0)
    };
    Some((channel(0)?, channel(2)?, channel(4)?))
}

pub fn rgb_to_hex(r: f32, g: f32, b: f32) -> String {
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
}

/// Whether dark text reads better than light text on this fill.
pub fn is_light(r: f32, g: f32, b: f32) -> bool {
    0.2126 * r + 0.7152 * g + 0.0722 * b > 0.5
}

fn mermaid_label(name: &str) -> String {
    name.replace('"', "#quot;")
}

// ----- mermaid fallback -----

const KEYWORDS: &[&str] = &[
    "flowchart",
    "graph",
    "subgraph",
    "end",
    "direction",
    "style",
    "classDef",
    "class",
    "click",
    "linkStyle",
    "%%",
];

/// Build a document from the first mermaid block in `text`. Only node ids and `-->`
/// style edges are read; positions come from a simple left-to-right layering.
pub fn parse_markdown(text: &str) -> GraphDoc {
    let mut ids: Vec<String> = Vec::new();
    let mut edges: Vec<EdgeDoc> = Vec::new();
    let mut in_mermaid = false;

    for raw in text.lines() {
        let line = raw.trim();
        if let Some(fence) = line.strip_prefix("```") {
            in_mermaid = !in_mermaid && fence.trim_start().starts_with("mermaid");
            continue;
        }
        if !in_mermaid {
            continue;
        }
        let Some((from, to)) = parse_statement(line) else {
            continue;
        };
        push_unique(&mut ids, &from);
        if let Some(to) = to {
            push_unique(&mut ids, &to);
            let edge = EdgeDoc { from, to };
            if !edges.contains(&edge) {
                edges.push(edge);
            }
        }
    }

    let positions = auto_layout(&ids, &edges);
    GraphDoc {
        version: VERSION,
        name: String::new(),
        view: View::default(),
        nodes: ids
            .into_iter()
            .zip(positions)
            .map(|(id, (x, y))| NodeDoc {
                id,
                x,
                y,
                kind: None,
                color: None,
                data: None,
            })
            .collect(),
        edges,
    }
}

fn push_unique(ids: &mut Vec<String>, id: &str) {
    if !ids.iter().any(|existing| existing == id) {
        ids.push(id.to_string());
    }
}

/// `a` or `a --> b`, tolerating node shapes like `a[" "]` and edge labels like `-->|yes|`.
fn parse_statement(line: &str) -> Option<(String, Option<String>)> {
    if line.is_empty() || KEYWORDS.iter().any(|k| line.starts_with(k)) {
        return None;
    }
    let (from, rest) = take_ident(line)?;
    let rest = skip_shape(rest.trim_start()).trim_start();
    if !rest.starts_with(['-', '=', '.']) {
        return Some((from, None));
    }
    let rest = rest
        .trim_start_matches(|c: char| matches!(c, '-' | '=' | '.' | '>'))
        .trim_start();
    // Arrow heads `--x` / `--o`, then an optional `|label|`.
    let rest = match rest.strip_prefix(['x', 'o']) {
        Some(after) if after.starts_with(char::is_whitespace) => after.trim_start(),
        _ => rest,
    };
    let rest = match rest.strip_prefix('|') {
        Some(after) => after.split_once('|').map_or("", |(_, tail)| tail),
        None => rest,
    };
    let (to, _) = take_ident(rest.trim_start())?;
    Some((from, Some(to)))
}

fn take_ident(s: &str) -> Option<(String, &str)> {
    let end = s
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    (end > 0).then(|| (s[..end].to_string(), &s[end..]))
}

/// Skip a node shape such as `[text]`, `((text))`, `{text}` or `>text]`.
fn skip_shape(s: &str) -> &str {
    let mut chars = s.char_indices();
    match chars.next() {
        Some((_, '[' | '(' | '{' | '>')) => {}
        _ => return s,
    }
    let (mut depth, mut quoted) = (1usize, false);
    for (i, c) in chars {
        match c {
            '"' => quoted = !quoted,
            _ if quoted => {}
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return &s[i + c.len_utf8()..];
                }
            }
            _ => {}
        }
    }
    ""
}

/// Longest-path layering: column by depth from the sources, rows in order of appearance.
fn auto_layout(ids: &[String], edges: &[EdgeDoc]) -> Vec<(f32, f32)> {
    let index = |id: &str| ids.iter().position(|candidate| candidate == id);
    let mut depth = vec![0usize; ids.len()];
    for _ in 0..ids.len() {
        let mut changed = false;
        for edge in edges {
            let (Some(a), Some(b)) = (index(&edge.from), index(&edge.to)) else {
                continue;
            };
            // The bound keeps cycles from layering forever.
            if depth[a] + 1 > depth[b] && depth[a] + 1 < ids.len() {
                depth[b] = depth[a] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut rows = vec![0usize; ids.len().max(1)];
    depth
        .iter()
        .map(|&d| {
            let row = rows[d];
            rows[d] += 1;
            (80.0 + d as f32 * 220.0, 80.0 + row as f32 * 120.0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node_data::{DataConfig, DataFormat};

    fn sample() -> GraphDoc {
        GraphDoc {
            version: VERSION,
            name: "demo".into(),
            view: View {
                pan_x: 10.0,
                pan_y: -5.0,
            },
            nodes: vec![
                NodeDoc {
                    id: "n0".into(),
                    x: 1.0,
                    y: 2.0,
                    kind: None,
                    color: None,
                    data: None,
                },
                NodeDoc {
                    id: "n1".into(),
                    x: 3.0,
                    y: 4.0,
                    kind: Some("Fetch".into()),
                    color: Some("#ff8800".into()),
                    data: Some(NodeData::Data(DataConfig {
                        path: "a.csv".into(),
                        format: DataFormat::Csv,
                    })),
                },
            ],
            edges: vec![EdgeDoc {
                from: "n0".into(),
                to: "n1".into(),
            }],
        }
    }

    #[test]
    fn mermaid_lists_nodes_then_edges_then_styles() {
        assert_eq!(
            sample().mermaid(),
            "flowchart LR\n    n0[\" \"]\n    n1[\"Fetch\"]\n    n0 --> n1\n    style n1 fill:#ff8800,stroke:#bf6600,color:#000000\n"
        );
    }

    #[test]
    fn mermaid_labels_escape_quotes() {
        assert_eq!(mermaid_label("say \"hi\""), "say #quot;hi#quot;");
    }

    #[test]
    fn hex_colors_round_trip() {
        assert_eq!(hex_to_rgb("#FF8800"), Some((1.0, 136.0 / 255.0, 0.0)));
        assert_eq!(hex_to_rgb("ff8800"), hex_to_rgb("#ff8800"));
        assert_eq!(hex_to_rgb("#fff"), None);
        assert_eq!(hex_to_rgb("#zz0000"), None);
        assert_eq!(rgb_to_hex(1.0, 136.0 / 255.0, 0.0), "#ff8800");
        assert!(is_light(1.0, 1.0, 1.0));
        assert!(!is_light(0.1, 0.1, 0.4));
    }

    #[test]
    fn node_kind_json_round_trips() {
        let kind = NodeKind {
            name: "Fetch".into(),
            color: "#ff8800".into(),
        };
        let text = serde_json::to_string(&kind).unwrap();
        assert_eq!(serde_json::from_str::<NodeKind>(&text).unwrap(), kind);
    }

    #[test]
    fn markdown_wraps_mermaid_in_a_fenced_block() {
        let md = sample().markdown();
        assert!(md.starts_with("# demo\n"));
        assert!(md.contains("```mermaid\nflowchart LR\n"));
        assert!(md.ends_with("```\n"));
    }

    #[test]
    fn json_round_trips_including_node_data() {
        let text = serde_json::to_string(&sample()).unwrap();
        assert!(!text.contains("\"kind\":null"), "absent kinds are omitted");
        let back: GraphDoc = serde_json::from_str(&text).unwrap();
        assert_eq!(back.nodes.len(), 2);
        assert_eq!(back.nodes[0].kind, None);
        assert_eq!(back.nodes[1].kind.as_deref(), Some("Fetch"));
        assert_eq!(back.nodes[1].color.as_deref(), Some("#ff8800"));
        assert!(
            text.contains(r#""data":{}"#),
            "blank nodes write an empty object"
        );
        assert!(text.contains(r#""type":"data""#));
        assert_eq!(back.nodes[1].data, sample().nodes[1].data);
        assert_eq!(back.edges, sample().edges);
        assert_eq!(back.view.pan_x, 10.0);
    }

    #[test]
    fn json_missing_fields_use_defaults() {
        let back: GraphDoc = serde_json::from_str(r#"{"nodes":[{"id":"a","x":0,"y":0}]}"#).unwrap();
        assert_eq!(back.version, VERSION);
        assert!(back.nodes[0].data.is_none());
        assert!(back.edges.is_empty());
    }

    #[test]
    fn markdown_fallback_reads_shapes_labels_and_layers() {
        let md = "# x\n\n```json\n{\"not\": \"mermaid\"}\n```\n\n```mermaid\nflowchart LR\n    a[\" \"]\n    b((round))\n    a --> b\n    b -->|yes| c\n    a --- b\n```\n";
        let doc = parse_markdown(md);
        let ids: Vec<&str> = doc.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(doc.edges.len(), 2);
        assert_eq!(
            doc.edges[1],
            EdgeDoc {
                from: "b".into(),
                to: "c".into()
            }
        );
        let xs: Vec<f32> = doc.nodes.iter().map(|n| n.x).collect();
        assert_eq!(xs, [80.0, 300.0, 520.0]);
    }

    #[test]
    fn cycles_still_lay_out() {
        let md = "```mermaid\nflowchart LR\n    a --> b\n    b --> a\n```";
        let doc = parse_markdown(md);
        assert_eq!(doc.nodes.len(), 2);
    }

    #[test]
    fn names_stay_inside_the_graphs_directory() {
        assert_eq!(sanitize_name("../etc/passwd"), "---etc-passwd");
        assert_eq!(sanitize_name("  My Graph 2 "), "My Graph 2");
        assert_eq!(sanitize_name(""), "untitled");
    }
}
