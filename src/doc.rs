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
use crate::storage;
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
    /// The node's own name: its canvas label, and what `{{name}}` in an agent's prompt
    /// refers to. See [`validate::node_name`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// The diagram as a mermaid flowchart. A node is labeled with its name, else the kind
    /// it was made from, else a blank.
    pub fn mermaid(&self) -> String {
        let mut out = String::from("flowchart LR\n");
        for node in &self.nodes {
            let label = node
                .name
                .as_deref()
                .or(node.kind.as_deref())
                .map_or(" ".to_string(), mermaid_label);
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

fn paths_in(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    (
        dir.join(format!("{name}.md")),
        dir.join(format!("{name}.json")),
    )
}

/// A name that is its own sanitized form can't leave the directory it is joined to.
fn safe_name(name: &str) -> Result<(), String> {
    if name.is_empty() || sanitize_name(name) != name {
        return Err(format!("`{name}` is not a safe file name"));
    }
    Ok(())
}

/// A name from a listing: a bare file stem, never a path.
fn listed_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains(['/', '\\']) || name.starts_with('.') {
        return Err(format!("`{name}` is not a graph name"));
    }
    Ok(())
}

/// Write both files, returning their paths. The name must already be sanitized.
pub fn save(doc: &GraphDoc) -> Result<(PathBuf, PathBuf), String> {
    save_in(Path::new(DIR), doc)
}

fn save_in(dir: &Path, doc: &GraphDoc) -> Result<(PathBuf, PathBuf), String> {
    safe_name(&doc.name)?;
    let text = serde_json::to_string_pretty(doc).map_err(|e| e.to_string())? + "\n";
    let markdown = doc.markdown();
    storage::check_size(text.len(), storage::MAX_DOCUMENT_BYTES)?;
    storage::check_size(markdown.len(), storage::MAX_DOCUMENT_BYTES)?;
    fs::create_dir_all(dir).map_err(|e| format!("create {}/: {e}", dir.display()))?;
    let (md, json) = paths_in(dir, &doc.name);
    storage::write_atomic(&md, markdown.as_bytes())?;
    storage::write_atomic(&json, text.as_bytes())?;
    Ok((md, json))
}

/// Read a graph by name: the JSON sidecar when present, else the markdown's mermaid block.
pub fn load(name: &str) -> Result<GraphDoc, String> {
    load_in(Path::new(DIR), name)
}

fn load_in(dir: &Path, name: &str) -> Result<GraphDoc, String> {
    listed_name(name)?;
    let (md, json) = paths_in(dir, name);
    let mut doc = if json.exists() {
        let text = storage::read_text(&json, storage::MAX_DOCUMENT_BYTES)?;
        serde_json::from_str::<GraphDoc>(&text)
            .map_err(|e| format!("parse {}: {e}", json.display()))?
    } else {
        let text = storage::read_text(&md, storage::MAX_DOCUMENT_BYTES)?;
        parse_markdown(&text)
    };
    doc.name = name.to_string();
    Ok(doc)
}

/// Names of every saved graph (markdown files in `graphs/`), sorted.
pub fn list() -> Vec<String> {
    list_in(Path::new(DIR))
}

fn list_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
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
                .filter(|stem| listed_name(stem).is_ok())
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

/// Write the kind's file. The name must already be sanitized.
pub fn save_kind(kind: &NodeKind) -> Result<PathBuf, String> {
    save_kind_in(Path::new(NODES_DIR), kind)
}

fn save_kind_in(dir: &Path, kind: &NodeKind) -> Result<PathBuf, String> {
    safe_name(&kind.name)?;
    let text = serde_json::to_string_pretty(kind).map_err(|e| e.to_string())? + "\n";
    storage::check_size(text.len(), storage::MAX_DOCUMENT_BYTES)?;
    fs::create_dir_all(dir).map_err(|e| format!("create {}/: {e}", dir.display()))?;
    let path = dir.join(format!("{}.json", kind.name));
    storage::write_atomic(&path, text.as_bytes())?;
    Ok(path)
}

pub fn delete_kind(name: &str) -> Result<(), String> {
    delete_kind_in(Path::new(NODES_DIR), name)
}

fn delete_kind_in(dir: &Path, name: &str) -> Result<(), String> {
    listed_name(name)?;
    let path = dir.join(format!("{name}.json"));
    fs::remove_file(&path).map_err(|e| format!("delete {}: {e}", path.display()))
}

/// Every saved kind, sorted by name. Files that don't parse are skipped.
pub fn list_kinds() -> Vec<NodeKind> {
    list_kinds_in(Path::new(NODES_DIR))
}

fn list_kinds_in(dir: &Path) -> Vec<NodeKind> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut kinds: Vec<NodeKind> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                return None;
            }
            serde_json::from_slice(&storage::read(&path, storage::MAX_DOCUMENT_BYTES).ok()?).ok()
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
/// style edges are read, plus the label a node was drawn with; positions come from a
/// simple left-to-right layering. The diagram is the fallback for a missing sidecar, so
/// it carries no positions, pan or node data — those live in the `.json` next to it, and
/// a file rebuilt from its diagram alone comes back laid out and blank.
pub fn parse_markdown(text: &str) -> GraphDoc {
    let mut ids: Vec<String> = Vec::new();
    let mut edges: Vec<EdgeDoc> = Vec::new();
    // The label each node was last drawn with, so a fallback load keeps the kind names
    // the diagram already carries instead of dropping them.
    let mut kinds: Vec<Option<String>> = Vec::new();
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
        let Some((from, label, to)) = parse_statement(line) else {
            continue;
        };
        if push_unique(&mut ids, &from) {
            kinds.push(label.filter(|label| !label.trim().is_empty()));
        }
        if let Some(to) = to {
            if push_unique(&mut ids, &to) {
                kinds.push(None);
            }
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
            .zip(kinds)
            .map(|((id, (x, y)), kind)| NodeDoc {
                id,
                x,
                y,
                color: None,
                data: None,
                kind,
                name: None,
            })
            .collect(),
        edges,
    }
}

/// Add `id` if it is new; returns whether it was.
fn push_unique(ids: &mut Vec<String>, id: &str) -> bool {
    if !ids.iter().any(|existing| existing == id) {
        ids.push(id.to_string());
        true
    } else {
        false
    }
}

/// `a` or `a[" "] --> b`, tolerating node shapes and edge labels like `-->|yes|`. The
/// label is the quoted text inside the node's shape, if it has one.
fn parse_statement(line: &str) -> Option<(String, Option<String>, Option<String>)> {
    if line.is_empty() || KEYWORDS.iter().any(|k| line.starts_with(k)) {
        return None;
    }
    let (from, rest) = take_ident(line)?;
    let shaped = rest.trim_start();
    let (label, rest) = shape_label(shaped);
    let rest = rest.trim_start();
    if !rest.starts_with(['-', '=', '.']) {
        return Some((from, label, None));
    }
    let rest = rest.trim_start_matches(['-', '=', '.', '>']).trim_start();
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
    Some((from, label, Some(to)))
}

/// The label a node's shape carries and the text after the shape. `a[" "]` is a blank
/// node, `a["Agent"]` a labelled one, and a shape without quotes (`a[x]`) is no label.
fn shape_label(s: &str) -> (Option<String>, &str) {
    let rest = skip_shape(s);
    // `skip_shape` hands back `s` itself only when there was no shape to skip.
    if rest.len() == s.len() {
        return (None, s);
    }
    // Everything from the first quote to the matching one is the label.
    let Some(open) = s.find('"') else {
        return (None, rest);
    };
    let body = &s[open + 1..];
    let Some(close) = body.find('"') else {
        return (None, rest);
    };
    (Some(mermaid_unquote(&body[..close])), rest)
}

/// `#quot;` back to `"`, the escape [`mermaid_label`] writes.
fn mermaid_unquote(label: &str) -> String {
    label.replace("#quot;", "\"")
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
                    name: None,
                    color: None,
                    data: None,
                },
                NodeDoc {
                    id: "n1".into(),
                    x: 3.0,
                    y: 4.0,
                    kind: Some("Fetch".into()),
                    name: None,
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
    fn the_diagram_round_trips_through_its_own_parser() {
        // What `save` writes is what a sidecar-less load reads back. Ids, edges and the
        // kind labels have to survive; positions and pan do not, and are documented as
        // living in the sidecar.
        let doc = GraphDoc {
            version: VERSION,
            name: "demo".into(),
            view: View {
                pan_x: 123.0,
                pan_y: 456.0,
            },
            nodes: vec![
                NodeDoc {
                    id: "n0".into(),
                    x: 400.0,
                    y: 250.0,
                    kind: Some("My Agent".into()),
                    name: None,
                    color: None,
                    data: None,
                },
                NodeDoc {
                    id: "n1".into(),
                    x: 900.0,
                    y: 600.0,
                    kind: None,
                    name: None,
                    color: None,
                    data: None,
                },
            ],
            edges: vec![EdgeDoc {
                from: "n0".into(),
                to: "n1".into(),
            }],
        };
        let back = parse_markdown(&doc.markdown());
        let ids: Vec<&str> = back.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["n0", "n1"], "ids survive the diagram");
        assert_eq!(back.edges, doc.edges, "edges survive the diagram");
        assert_eq!(
            back.nodes[0].kind.as_deref(),
            Some("My Agent"),
            "the label a node was drawn with survives"
        );
        // A blank node carries no label, and does not gain one.
        assert_eq!(back.nodes[1].kind, None);
        // The layout is the fallback's own, not the saved canvas positions.
        assert_ne!(back.nodes[0].x, doc.nodes[0].x);
        assert_eq!(back.nodes[0].y, back.nodes[1].y, "laid out in one row");
    }

    #[test]
    fn kind_labels_round_trip_through_their_escaping() {
        // `mermaid()` escapes a quote as `#quot;`; the reader must undo it, or a kind
        // named with a quote would come back with the escape left in its name.
        let doc = GraphDoc {
            version: VERSION,
            name: "q".into(),
            view: View::default(),
            nodes: vec![NodeDoc {
                id: "n0".into(),
                x: 0.0,
                y: 0.0,
                kind: Some(r#"a "quoted" name"#.into()),
                name: None,
                color: None,
                data: None,
            }],
            edges: vec![],
        };
        let text = doc.markdown();
        assert!(text.contains("a #quot;quoted#quot; name"), "{text}");
        let back = parse_markdown(&text);
        assert_eq!(back.nodes[0].kind.as_deref(), Some(r#"a "quoted" name"#));
    }

    #[test]
    fn unquoted_shapes_and_shapes_without_labels_are_tolerated() {
        let doc = parse_markdown(
            "```mermaid\nflowchart LR\n    a[x]\n    b((round))\n    c{\"c\"}\n    a --> b\n```",
        );
        let ids: Vec<&str> = doc.nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        // Only the quoted one is a label.
        assert_eq!(doc.nodes[2].kind.as_deref(), Some("c"));
        assert_eq!(doc.nodes[0].kind, None);
        assert_eq!(doc.nodes[1].kind, None);
    }

    #[test]
    fn names_stay_inside_the_graphs_directory() {
        assert_eq!(sanitize_name("../etc/passwd"), "---etc-passwd");
        assert_eq!(sanitize_name("  My Graph 2 "), "My Graph 2");
        assert_eq!(sanitize_name(""), "untitled");
    }

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("uapz-doc-{tag}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn graphs_save_load_and_list_on_disk() {
        let dir = scratch("graphs");
        let mut doc = sample();
        doc.name = "demo".into();
        let (md, json) = save_in(&dir, &doc).unwrap();
        assert_eq!(md, dir.join("demo.md"));
        assert_eq!(json, dir.join("demo.json"));
        assert!(fs::read_to_string(&md).unwrap().contains("```mermaid"));

        let loaded = load_in(&dir, "demo").unwrap();
        assert_eq!(loaded.name, "demo");
        assert_eq!(
            serde_json::to_value(&loaded).unwrap(),
            serde_json::to_value(&doc).unwrap()
        );

        let mut other = sample();
        other.name = "alpha".into();
        save_in(&dir, &other).unwrap();
        fs::write(dir.join("notes.txt"), "x").unwrap();
        fs::write(dir.join(".hidden.md"), "x").unwrap();
        assert_eq!(list_in(&dir), ["alpha", "demo"]);

        // Without the sidecar the markdown's mermaid block is enough for the shape.
        fs::remove_file(&json).unwrap();
        let fallback = load_in(&dir, "demo").unwrap();
        assert_eq!(fallback.name, "demo");
        let ids = |d: &GraphDoc| d.nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&fallback), ids(&doc));
        assert_eq!(fallback.edges, doc.edges);

        let mut evil = sample();
        evil.name = "../evil".into();
        assert!(save_in(&dir, &evil).unwrap_err().contains("not a safe"));
        assert!(!dir.parent().unwrap().join("evil.md").exists());
        assert!(load_in(&dir, "../demo").is_err());
        assert!(load_in(&dir, "missing").unwrap_err().contains("missing.md"));
        assert!(list_in(&dir.join("nope")).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn kinds_save_list_and_delete_on_disk() {
        let dir = scratch("kinds");
        let alpha = NodeKind {
            name: "Alpha".into(),
            color: "#ff0000".into(),
        };
        let beta = NodeKind {
            name: "Beta 2".into(),
            color: "#00ff00".into(),
        };
        save_kind_in(&dir, &beta).unwrap();
        assert_eq!(save_kind_in(&dir, &alpha).unwrap(), dir.join("Alpha.json"));
        fs::write(dir.join("broken.json"), "{").unwrap();
        fs::write(dir.join("readme.txt"), "x").unwrap();
        assert_eq!(list_kinds_in(&dir), vec![alpha.clone(), beta.clone()]);
        delete_kind_in(&dir, "Beta 2").unwrap();
        assert_eq!(list_kinds_in(&dir), vec![alpha]);
        assert!(delete_kind_in(&dir, "Beta 2").is_err());
        assert!(delete_kind_in(&dir, "../x").is_err());
        let evil = NodeKind {
            name: "../x".into(),
            color: "#000000".into(),
        };
        assert!(save_kind_in(&dir, &evil).is_err());
        assert!(list_kinds_in(&dir.join("nope")).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }
}
