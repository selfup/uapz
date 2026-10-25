//! The run history: one JSON record per run under `runs/<graph>/<timestamp>.json`.
//!
//! A record holds the log, the outcome, and for every node that took part its state,
//! its content at the time, the label of its output, a preview of the text, and the
//! [`content_key`](exec::content_key) under which the executor's [`Cache`] keeps the full
//! bytes. The inspector lists a graph's runs, reopens one without running anything (the
//! outputs come back from the cache, or the preview when the cache has let them go), and
//! shows a node's previous output next to its current one.
//!
//! Records are written by the thread that ran the graph, pruned to [`MAX_RUNS`] per
//! graph, and never read as instructions: a broken file is skipped.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::doc::GraphDoc;
use crate::exec::{self, Cache, Key, Output, Report};
use crate::library;
use crate::node_data::NodeData;
use crate::validate;

/// Directory, relative to the working directory, that holds run records.
pub const DIR: &str = "runs";
/// Records kept per graph; the oldest go as new ones are written.
pub const MAX_RUNS: usize = 200;
/// Characters of a text output kept in the record itself.
pub const PREVIEW_CHARS: usize = 4000;

/// One run of a graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// When the run finished, as a UTC timestamp (`YYYYMMDDTHHMMSSmmmZ`); also the file
    /// stem.
    pub id: String,
    /// The graph's name as saved, `untitled` for a graph never saved.
    pub graph: String,
    /// The nodes Run was pressed for, in the order they were started.
    pub targets: Vec<String>,
    pub elapsed_ms: u64,
    pub outcome: Outcome,
    pub log: Vec<String>,
    /// Every node that ran, was reused or failed, by document id.
    pub nodes: BTreeMap<String, NodeRun>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Outcome {
    Ok,
    Failed { error: String },
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Ran,
    Reused,
    Failed,
}

/// What one node did in a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeRun {
    pub state: State,
    /// The node's content when it ran, so a later reader knows which prompt or query
    /// produced the output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<NodeData>,
    /// The output's label, e.g. `Text · 2 KB`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Where an agent's output was saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved: Option<PathBuf>,
    /// The content key the cache holds the full output under, when it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The first [`PREVIEW_CHARS`] characters of a text output.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preview: String,
    /// Whether the preview is the whole output.
    #[serde(default)]
    pub complete: bool,
    /// Why the node failed, for a failed node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl NodeRun {
    fn empty(state: State, data: Option<NodeData>) -> Self {
        Self {
            state,
            data,
            label: None,
            saved: None,
            key: None,
            preview: String::new(),
            complete: false,
            error: None,
        }
    }

    /// The content key, parsed back.
    pub fn content_key(&self) -> Option<Key> {
        Key::from_str_radix(self.key.as_deref()?, 16).ok()
    }

    /// The output as the record alone can give it: the preview as text, marked as such
    /// when it is not the whole output.
    fn preview_output(&self) -> Option<Output> {
        let label = self.label.clone()?;
        if self.preview.is_empty() {
            return None;
        }
        Some(Output {
            bytes: self.preview.clone().into_bytes(),
            text: true,
            label: if self.complete {
                label
            } else {
                format!("{label} · preview only")
            },
            saved: self.saved.clone(),
        })
    }

    /// The full output from the cache, else what the record kept of it.
    pub fn output(&self, cache: Option<&Mutex<Cache>>) -> Option<Output> {
        if let (Some(key), Some(cache)) = (self.content_key(), cache)
            && let Some(output) = exec::lock(cache).fetch(key)
        {
            return Some(output);
        }
        self.preview_output()
    }
}

/// The graph name a record files under: the document's, made safe, `untitled` when empty.
pub fn graph_name(name: &str) -> String {
    validate::file_stem(name)
}

/// Build the record of a finished run. Every output is also handed to the cache under its
/// content key, so the run can be reopened in full later.
pub fn record(
    doc: &GraphDoc,
    targets: &[String],
    report: &Report,
    cache: Option<&Mutex<Cache>>,
) -> Run {
    let data_of = |id: &str| {
        doc.nodes
            .iter()
            .find(|n| n.id == id)
            .and_then(|n| n.data.clone())
    };
    let mut nodes = BTreeMap::new();
    for (id, output) in &report.outputs {
        let data = data_of(id);
        let key = data
            .as_ref()
            .map(|d| exec::content_key(d.node_type(), output));
        let kept = match (key, cache) {
            (Some(key), Some(cache)) => exec::lock(cache).keep(key, output),
            _ => false,
        };
        let (preview, complete) = match output.as_text() {
            Some(text) => (
                text.chars().take(PREVIEW_CHARS).collect::<String>(),
                text.chars().count() <= PREVIEW_CHARS,
            ),
            None => (String::new(), false),
        };
        let state = if report.reused.contains(id) {
            State::Reused
        } else {
            State::Ran
        };
        nodes.insert(
            id.clone(),
            NodeRun {
                state,
                data,
                label: Some(output.label.clone()),
                saved: output.saved.clone(),
                key: kept.then(|| format!("{:032x}", key.unwrap_or_default())),
                preview,
                complete,
                error: None,
            },
        );
    }
    let outcome = match &report.result {
        Ok(_) => Outcome::Ok,
        Err(e) if e == exec::CANCELLED => Outcome::Cancelled,
        Err(e) => Outcome::Failed { error: e.clone() },
    };
    if let (Some(id), Outcome::Failed { error }) = (&report.failed, &outcome) {
        let mut failed = NodeRun::empty(State::Failed, data_of(id));
        failed.error = Some(error.clone());
        nodes.insert(id.clone(), failed);
    }
    Run {
        id: library::timestamp(),
        graph: graph_name(&doc.name),
        targets: targets.to_vec(),
        elapsed_ms: u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX),
        outcome,
        log: report.log.clone(),
        nodes,
    }
}

impl Run {
    /// `HH:MM:SS` (UTC) out of the id, or the id itself when it has another shape.
    pub fn time(&self) -> String {
        let id = &self.id;
        let digits = |range: std::ops::Range<usize>| {
            id.get(range)
                .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
        };
        match (digits(9..11), digits(11..13), digits(13..15)) {
            (Some(h), Some(m), Some(s)) if id.as_bytes().get(8) == Some(&b'T') => {
                format!("{h}:{m}:{s}")
            }
            _ => id.clone(),
        }
    }

    /// One line for a list of runs: outcome, time, targets, duration and reuse count.
    pub fn headline(&self) -> String {
        let targets = match self.targets.len() {
            0 => "nothing".to_string(),
            1 => self.targets[0].clone(),
            n => format!("all ({n})"),
        };
        let reused = self
            .nodes
            .values()
            .filter(|n| n.state == State::Reused)
            .count();
        let reused = if reused == 0 {
            String::new()
        } else {
            format!(" · {reused} reused")
        };
        let secs = self.elapsed_ms as f64 / 1000.0;
        match &self.outcome {
            Outcome::Ok => format!("✓ {} · {targets} · {secs:.1}s{reused}", self.time()),
            Outcome::Failed { .. } => {
                let node = self
                    .nodes
                    .iter()
                    .find(|(_, n)| n.state == State::Failed)
                    .map(|(id, _)| format!(" · {id} failed"))
                    .unwrap_or_else(|| " · failed".to_string());
                format!("✗ {} · {targets}{node} · {secs:.1}s", self.time())
            }
            Outcome::Cancelled => format!("■ {} · {targets} · cancelled", self.time()),
        }
    }

    /// The run as a [`Report`], as if it had just finished, with the outputs fetched from
    /// the cache (or the previews the record kept) so the inspector shows it the same way.
    pub fn report(&self, cache: Option<&Mutex<Cache>>) -> Report {
        let outputs: std::collections::HashMap<String, Output> = self
            .nodes
            .iter()
            .filter(|(_, n)| n.state != State::Failed)
            .filter_map(|(id, n)| Some((id.clone(), n.output(cache)?)))
            .collect();
        let failed = self
            .nodes
            .iter()
            .find(|(_, n)| n.state == State::Failed)
            .map(|(id, _)| id.clone());
        let result = match &self.outcome {
            Outcome::Ok => self
                .targets
                .last()
                .and_then(|t| outputs.get(t).cloned())
                .ok_or_else(|| "the output of this run is no longer stored".to_string()),
            Outcome::Failed { error } => Err(error.clone()),
            Outcome::Cancelled => Err(exec::CANCELLED.to_string()),
        };
        Report {
            log: self.log.clone(),
            result,
            elapsed: Duration::from_millis(self.elapsed_ms),
            reused: self
                .nodes
                .iter()
                .filter(|(_, n)| n.state == State::Reused)
                .map(|(id, _)| id.clone())
                .collect(),
            outputs,
            failed,
        }
    }
}

fn graph_dir(root: &Path, graph: &str) -> PathBuf {
    root.join(DIR).join(validate::file_stem(graph))
}

/// Write the record as `runs/<graph>/<id>.json` and drop the oldest records past
/// [`MAX_RUNS`]. Two runs ending in the same millisecond get a suffix, not a clobber.
pub fn save(root: &Path, run: &Run) -> Result<PathBuf, String> {
    let dir = graph_dir(root, &run.graph);
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let mut path = dir.join(format!("{}.json", run.id));
    let mut n = 1;
    while path.exists() {
        path = dir.join(format!("{}-{n}.json", run.id));
        n += 1;
    }
    let json = serde_json::to_string_pretty(run).map_err(|e| e.to_string())?;
    fs::write(&path, json + "\n").map_err(|e| format!("write {}: {e}", path.display()))?;
    let mut files = record_files(&dir);
    if files.len() > MAX_RUNS {
        files.sort();
        for old in &files[..files.len() - MAX_RUNS] {
            let _ = fs::remove_file(old);
        }
    }
    Ok(path)
}

/// The `.json` files in a graph's run directory, in no particular order.
fn record_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "json"))
        .collect()
}

/// A graph's runs, newest first, at most [`MAX_RUNS`]. Files that don't parse are skipped.
pub fn list(root: &Path, graph: &str) -> Vec<Run> {
    let mut files = record_files(&graph_dir(root, graph));
    files.sort();
    files.reverse();
    files
        .iter()
        .filter_map(|path| serde_json::from_str::<Run>(&fs::read_to_string(path).ok()?).ok())
        .take(MAX_RUNS)
        .collect()
}

/// Delete a graph's run records. Returns how many went.
pub fn clear(root: &Path, graph: &str) -> usize {
    let files = record_files(&graph_dir(root, graph));
    let count = files.len();
    for file in files {
        let _ = fs::remove_file(file);
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::{EdgeDoc, NodeDoc, View};
    use crate::node_data::{DataConfig, DataFormat};
    use std::collections::HashMap;

    fn scratch(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("uapz-history-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn output(text: &str) -> Output {
        Output {
            bytes: text.as_bytes().to_vec(),
            text: true,
            label: format!("Text · {} B", text.len()),
            saved: None,
        }
    }

    fn doc() -> GraphDoc {
        let data = |id: &str, path: &str| NodeDoc {
            id: id.into(),
            x: 0.0,
            y: 0.0,
            kind: None,
            name: None,
            color: None,
            data: Some(NodeData::Data(DataConfig {
                path: path.into(),
                format: DataFormat::Text,
            })),
        };
        GraphDoc {
            version: 1,
            name: "My Graph".into(),
            view: View::default(),
            nodes: vec![data("n0", "a.txt"), data("n1", "b.txt")],
            edges: vec![EdgeDoc {
                from: "n0".into(),
                to: "n1".into(),
            }],
        }
    }

    fn report(result: Result<Output, String>, failed: Option<&str>) -> Report {
        let mut outputs = HashMap::new();
        outputs.insert("n0".to_string(), output("alpha"));
        if let Ok(out) = &result {
            outputs.insert("n1".to_string(), out.clone());
        }
        Report {
            log: vec!["n0: data".into(), "n1: data".into()],
            result,
            elapsed: Duration::from_millis(1500),
            reused: vec!["n0".into()],
            outputs,
            failed: failed.map(str::to_string),
        }
    }

    #[test]
    fn records_round_trip_through_disk_and_come_back_as_reports() {
        let root = scratch("round-trip");
        let cache = Mutex::new(Cache::persistent(root.join("cache")));
        let doc = doc();
        let targets = vec!["n1".to_string()];
        let long = "x".repeat(PREVIEW_CHARS + 10);

        let ok = record(
            &doc,
            &targets,
            &report(Ok(output(&long)), None),
            Some(&cache),
        );
        assert_eq!(ok.graph, "My Graph");
        assert_eq!(ok.outcome, Outcome::Ok);
        assert_eq!(ok.nodes["n0"].state, State::Reused);
        assert_eq!(ok.nodes["n1"].state, State::Ran);
        assert!(ok.nodes["n0"].complete);
        assert!(!ok.nodes["n1"].complete);
        assert_eq!(ok.nodes["n1"].preview.len(), PREVIEW_CHARS);
        assert!(ok.nodes["n1"].data.is_some());
        assert_eq!(ok.time().len(), 8, "{}", ok.time());
        assert!(ok.headline().starts_with('✓'), "{}", ok.headline());
        assert!(ok.headline().contains("1 reused"), "{}", ok.headline());

        let path = save(&root, &ok).unwrap();
        assert!(path.starts_with(root.join(DIR).join("My Graph")));
        let listed = list(&root, "My Graph");
        assert_eq!(listed, vec![ok.clone()]);
        assert!(list(&root, "other").is_empty());

        // The full output comes back from the cache; without it, the preview stands in.
        let back = listed[0].report(Some(&cache));
        assert_eq!(back.result.as_ref().unwrap().bytes, long.as_bytes());
        assert_eq!(back.reused, ["n0"]);
        assert_eq!(back.elapsed, Duration::from_millis(1500));
        assert_eq!(back.log, ok.log);
        let without = listed[0].report(None);
        let preview = without.result.unwrap();
        assert_eq!(preview.bytes.len(), PREVIEW_CHARS);
        assert!(preview.label.ends_with("preview only"), "{}", preview.label);
        assert_eq!(
            without.outputs["n0"].label, "Text · 5 B",
            "a complete preview keeps its label"
        );

        // A failure names the node and keeps its error; a cancel is neither.
        let failed = record(
            &doc,
            &targets,
            &report(Err("b.txt: no such file".into()), Some("n1")),
            Some(&cache),
        );
        assert_eq!(failed.nodes["n1"].state, State::Failed);
        assert_eq!(
            failed.nodes["n1"].error.as_deref(),
            Some("b.txt: no such file")
        );
        assert!(
            failed.headline().contains("n1 failed"),
            "{}",
            failed.headline()
        );
        let back = failed.report(Some(&cache));
        assert_eq!(back.failed.as_deref(), Some("n1"));
        assert_eq!(back.result.unwrap_err(), "b.txt: no such file");
        assert!(back.outputs.contains_key("n0") && !back.outputs.contains_key("n1"));
        let cancelled = record(
            &doc,
            &targets,
            &report(Err(exec::CANCELLED.into()), Some("n1")),
            Some(&cache),
        );
        assert_eq!(cancelled.outcome, Outcome::Cancelled);
        assert!(!cancelled.nodes.contains_key("n1"));
        assert_eq!(cancelled.report(None).result.unwrap_err(), exec::CANCELLED);
        assert!(cancelled.headline().starts_with('■'));

        // Same millisecond, two files; clear removes them all.
        save(&root, &ok).unwrap();
        assert_eq!(list(&root, "My Graph").len(), 2);
        assert_eq!(clear(&root, "My Graph"), 2);
        assert!(list(&root, "My Graph").is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn listings_are_newest_first_skip_junk_and_saving_prunes_the_oldest() {
        let root = scratch("prune");
        let doc = doc();
        let targets = vec!["n1".to_string()];
        let mut run = record(&doc, &targets, &report(Ok(output("v")), None), None);
        assert!(run.nodes["n1"].key.is_none(), "no cache, nothing kept");
        for n in 0..MAX_RUNS + 3 {
            run.id = format!("20261025T0000{n:05}Z");
            save(&root, &run).unwrap();
        }
        let dir = root.join(DIR).join("My Graph");
        fs::write(dir.join("junk.json"), "not a record").unwrap();
        fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let listed = list(&root, "My Graph");
        assert_eq!(listed.len(), MAX_RUNS);
        assert_eq!(listed[0].id, format!("20261025T0000{:05}Z", MAX_RUNS + 2));
        assert!(listed.windows(2).all(|w| w[0].id > w[1].id));
        // The junk file was counted by the pruning and the oldest records went first.
        assert!(!dir.join("20261025T000000000Z.json").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn times_and_names_are_forgiving() {
        let mut run = record(&doc(), &[], &report(Ok(output("v")), None), None);
        run.id = "20261025T142233123Z".into();
        assert_eq!(run.time(), "14:22:33");
        assert!(run.headline().contains("nothing"), "{}", run.headline());
        run.id = "custom".into();
        assert_eq!(run.time(), "custom");
        run.targets = vec!["n1".into(), "n2".into()];
        assert!(run.headline().contains("all (2)"), "{}", run.headline());
        assert_eq!(graph_name(""), "untitled");
        assert_eq!(graph_name("../x"), "---x");
        assert!(NodeRun::empty(State::Ran, None).content_key().is_none());
        let mut node = NodeRun::empty(State::Ran, None);
        node.key = Some("0000000000000000000000000000002a".into());
        assert_eq!(node.content_key(), Some(42));
        assert!(
            node.output(None).is_none(),
            "no label, no preview, no output"
        );
    }
}
