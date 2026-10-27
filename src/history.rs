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
use crate::storage;
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
    /// Captured child graphs by relative path, independently of whether they ran.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workflows: BTreeMap<String, GraphDoc>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub calls: BTreeMap<String, WorkflowRun>,
    /// Values supplied to Run input nodes, including their effective defaults.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inputs: crate::workflow::Values,
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
pub struct WorkflowRun {
    pub graph: GraphDoc,
    pub run: Run,
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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub named_outputs: BTreeMap<String, NodeRun>,
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
            named_outputs: BTreeMap::new(),
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
        if self.preview.is_empty() && !self.complete {
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

/// Capture one completed output using the same cache keys and preview policy for
/// live progress and persisted runs. No output bytes are embedded in the record.
pub(crate) fn record_node(
    data: Option<NodeData>,
    output: &Output,
    state: State,
    named_outputs: Option<&BTreeMap<String, Output>>,
    cache: Option<&Mutex<Cache>>,
) -> NodeRun {
    let key = data
        .as_ref()
        .map(|data| exec::content_key(data.node_type(), output));
    let kept = match (key, cache) {
        (Some(key), Some(cache)) => exec::lock(cache).keep(key, output),
        _ => false,
    };
    let mut node = NodeRun::empty(state, data);
    node.label = Some(output.label.clone());
    node.saved = output.saved.clone();
    node.key = kept.then(|| format!("{:032x}", key.unwrap_or_default()));
    if let Some(text) = output.as_text() {
        node.preview = text.chars().take(PREVIEW_CHARS).collect();
        node.complete = text.chars().count() <= PREVIEW_CHARS;
    }
    node.named_outputs = named_outputs
        .into_iter()
        .flatten()
        .map(|(name, output)| {
            let saved = serde_json::to_vec(&output.saved).unwrap_or_default();
            let key = exec::key_of(&[
                b"workflow-history-port-v1",
                name.as_bytes(),
                &[u8::from(output.text)],
                output.label.as_bytes(),
                &saved,
                &output.bytes,
            ]);
            let kept = cache.is_some_and(|cache| exec::lock(cache).keep(key, output));
            let mut port = NodeRun::empty(state, None);
            port.label = Some(output.label.clone());
            port.saved = output.saved.clone();
            port.key = kept.then(|| format!("{key:032x}"));
            if let Some(text) = output.as_text() {
                port.preview = text.chars().take(PREVIEW_CHARS).collect();
                port.complete = text.chars().count() <= PREVIEW_CHARS;
            }
            (name.clone(), port)
        })
        .collect();
    node
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
        let state = if report.reused.contains(id) {
            State::Reused
        } else {
            State::Ran
        };
        nodes.insert(
            id.clone(),
            record_node(
                data_of(id),
                output,
                state,
                report.named_outputs.get(id),
                cache,
            ),
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
        workflows: report.workflows.clone(),
        calls: report.calls.clone(),
        inputs: report.inputs.clone(),
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
            workflows: self.workflows.clone(),
            named_outputs: self
                .nodes
                .iter()
                .filter_map(|(id, node)| {
                    let ports: BTreeMap<_, _> = node
                        .named_outputs
                        .iter()
                        .filter_map(|(name, port)| Some((name.clone(), port.output(cache)?)))
                        .collect();
                    (!ports.is_empty()).then(|| (id.clone(), ports))
                })
                .collect(),
            calls: self.calls.clone(),
            inputs: self.inputs.clone(),
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

fn graph_dir(root: &Path, graph: &str, create: bool) -> Result<PathBuf, String> {
    storage::directory(
        root,
        &Path::new(DIR).join(validate::file_stem(graph)),
        create,
    )
}

/// Write the record as `runs/<graph>/<id>.json` and drop the oldest records past
/// [`MAX_RUNS`]. Two runs ending in the same millisecond get a suffix, not a clobber.
pub fn save(root: &Path, run: &Run) -> Result<PathBuf, String> {
    if run.id.is_empty() || validate::file_stem(&run.id) != run.id {
        return Err("run id must be a safe file stem".into());
    }
    // The root captures the whole tree. Descendant runs retain convenient relative
    // maps in memory, but writing those copies at every depth can exceed the record
    // limit even when the prepared call tree fits its snapshot budget.
    let mut compact = run.clone();
    prune_workflow_snapshots(&mut compact, "", &run.workflows);
    let json = serde_json::to_string_pretty(&compact).map_err(|e| e.to_string())? + "\n";
    storage::check_size(json.len(), storage::MAX_DOCUMENT_BYTES)?;
    let dir = graph_dir(root, &run.graph, true)?;
    let path = storage::write_unique(&dir, &run.id, "json", json.as_bytes())?;
    let mut files = record_files(&dir);
    if files.len() > MAX_RUNS {
        files.sort();
        for old in &files[..files.len() - MAX_RUNS] {
            let _ = fs::remove_file(old);
        }
    }
    Ok(path)
}

fn prune_workflow_snapshots(run: &mut Run, prefix: &str, captured: &BTreeMap<String, GraphDoc>) {
    for (id, call) in &mut run.calls {
        let prefix = format!("{prefix}{id}/");
        // Preserve older or partial records whose descendants have extra captures.
        call.run
            .workflows
            .retain(|path, graph| captured.get(&format!("{prefix}{path}")) != Some(graph));
        prune_workflow_snapshots(&mut call.run, &prefix, captured);
    }
}

fn restore_workflow_snapshots(run: &mut Run) {
    for (id, call) in &mut run.calls {
        let prefix = format!("{id}/");
        for (path, graph) in &run.workflows {
            if let Some(relative) = path.strip_prefix(&prefix) {
                call.run
                    .workflows
                    .entry(relative.to_string())
                    .or_insert_with(|| graph.clone());
            }
        }
        restore_workflow_snapshots(&mut call.run);
    }
}

/// The `.json` files in a graph's run directory, in no particular order.
fn record_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect()
}

/// A graph's runs, newest first, at most [`MAX_RUNS`]. Files that don't parse are skipped.
pub fn list(root: &Path, graph: &str) -> Vec<Run> {
    let Ok(dir) = graph_dir(root, graph, false) else {
        return Vec::new();
    };
    let mut files = record_files(&dir);
    files.sort();
    files.reverse();
    files
        .iter()
        .filter_map(|path| {
            serde_json::from_slice::<Run>(&storage::read(path, storage::MAX_DOCUMENT_BYTES).ok()?)
                .ok()
                .map(|mut run| {
                    restore_workflow_snapshots(&mut run);
                    run
                })
        })
        .take(MAX_RUNS)
        .collect()
}

/// Delete a graph's run records. Returns how many went.
pub fn clear(root: &Path, graph: &str) -> usize {
    let Ok(dir) = graph_dir(root, graph, false) else {
        return 0;
    };
    record_files(&dir)
        .iter()
        .filter(|path| fs::remove_file(path).is_ok())
        .count()
}

/// Move a graph's run records under a new graph name, merging into any records the
/// new name already has. A graph without history is not an error.
pub fn rename(root: &Path, from: &str, to: &str) -> Result<usize, String> {
    let Ok(source) = graph_dir(root, from, false) else {
        return Ok(0);
    };
    let target = graph_dir(root, to, true)?;
    if source == target {
        return Ok(0);
    }
    let files = record_files(&source);
    for path in &files {
        let Some(name) = path.file_name() else {
            continue;
        };
        fs::rename(path, target.join(name)).map_err(|e| format!("move {}: {e}", path.display()))?;
    }
    let _ = fs::remove_dir(&source);
    Ok(files.len())
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
            outputs: BTreeMap::new(),
            version: 1,
            name: "My Graph".into(),
            view: View::default(),
            nodes: vec![data("n0", "a.txt"), data("n1", "b.txt")],
            edges: vec![EdgeDoc {
                output: None,
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
            workflows: BTreeMap::new(),
            named_outputs: BTreeMap::new(),
            log: vec!["n0: data".into(), "n1: data".into()],
            calls: BTreeMap::new(),
            inputs: crate::workflow::Values::new(),
            result,
            elapsed: Duration::from_millis(1500),
            reused: vec!["n0".into()],
            outputs,
            failed: failed.map(str::to_string),
        }
    }

    #[test]
    fn run_values_survive_persistence_and_older_records_still_load() {
        let root = scratch("inputs");
        let mut report = report(Ok(output("result")), None);
        report.inputs = crate::workflow::Values::from([
            ("question".into(), "How many?".into()),
            ("count".into(), "3".into()),
        ]);
        let run = record(&doc(), &["n1".into()], &report, None);
        save(&root, &run).unwrap();
        let loaded = list(&root, &run.graph).pop().unwrap();
        assert_eq!(loaded.inputs, report.inputs);
        assert_eq!(loaded.report(None).inputs, report.inputs);
        let mut legacy = serde_json::to_value(&run).unwrap();
        legacy.as_object_mut().unwrap().remove("inputs");
        let legacy: Run = serde_json::from_value(legacy).unwrap();
        assert!(legacy.inputs.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn partial_older_snapshot_maps_survive_compaction_and_hydration() {
        let root = scratch("partial-snapshots");
        let mut child = record(
            &doc(),
            &["n0".into()],
            &report(Ok(output("child")), None),
            None,
        );
        let original = GraphDoc {
            name: "captured-child".into(),
            ..GraphDoc::default()
        };
        child.workflows.insert("n1".into(), original.clone());
        let mut parent = record(
            &doc(),
            &["n0".into()],
            &report(Ok(output("parent")), None),
            None,
        );
        parent.calls.insert(
            "n0".into(),
            WorkflowRun {
                graph: doc(),
                run: child,
            },
        );
        let different = GraphDoc {
            name: "different-root-capture".into(),
            ..GraphDoc::default()
        };
        parent.workflows.insert("n0/n1".into(), different);
        save(&root, &parent).unwrap();
        let loaded = list(&root, &parent.graph).pop().unwrap();
        assert_eq!(
            loaded, parent,
            "explicit child captures survive when an ancestor map differs"
        );
        assert_eq!(loaded.calls["n0"].run.workflows["n1"], original);
        let mut legacy = parent.clone();
        legacy.workflows.clear();
        legacy.id.push_str("-legacy");
        save(&root, &legacy).unwrap();
        assert!(list(&root, &legacy.graph).iter().any(|run| run == &legacy));
        fs::remove_dir_all(root).unwrap();
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
        assert!(path.starts_with(root.canonicalize().unwrap().join(DIR).join("My Graph")));
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

    #[test]
    fn history_refuses_path_ids_and_oversized_records() {
        let root = scratch("bounded");
        let mut run = record(&doc(), &[], &report(Ok(output("v")), None), None);
        run.id = "../../outside".into();
        assert!(save(&root, &run).unwrap_err().contains("safe file stem"));
        run.id = "oversized".into();
        run.log
            .push("x".repeat(storage::MAX_DOCUMENT_BYTES as usize));
        assert!(save(&root, &run).unwrap_err().contains("limit"));
        let dir = graph_dir(&root, &run.graph, true).unwrap();
        fs::write(
            dir.join("oversized.json"),
            serde_json::to_vec(&run).unwrap(),
        )
        .unwrap();
        assert!(list(&root, &run.graph).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn history_never_follows_symlinked_graph_directories_or_records() {
        let root = scratch("links");
        let run = record(&doc(), &[], &report(Ok(output("v")), None), None);
        let dir = graph_dir(&root, &run.graph, true).unwrap();
        let outside = root.join("outside.json");
        fs::write(&outside, serde_json::to_vec(&run).unwrap()).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("linked.json")).unwrap();
        assert!(list(&root, &run.graph).is_empty());
        assert_eq!(clear(&root, &run.graph), 0);
        std::os::unix::fs::symlink(&root, root.join(DIR).join("linked-graph")).unwrap();
        assert!(list(&root, "linked-graph").is_empty());
        assert_eq!(clear(&root, "linked-graph"), 0);
        let mut linked = run;
        linked.graph = "linked-graph".into();
        assert!(save(&root, &linked).is_err());
        assert!(outside.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
