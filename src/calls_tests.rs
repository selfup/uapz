use super::*;
use crate::doc::{EdgeDoc, NodeDoc};
use crate::exec::{Cache, Control, Dirs, NodeState, Progress, Report};
use crate::history::{self, Outcome, State};
use crate::node_data::{
    AgentConfig, DataConfig, DataFormat, FileConfig, Provider, Runtime, ScriptConfig, WriteMode,
};
use crate::providers::tests::FakeHttp;
use crate::review;
use crate::workflow::{InputConfig, InputType};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "uapz-calls-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for dir in ["graphs", "data", "scripts"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        Self(root)
    }

    fn save(&self, name: &str, doc: &GraphDoc) {
        fs::write(
            self.0.join(format!("graphs/{name}.json")),
            serde_json::to_vec(doc).unwrap(),
        )
        .unwrap();
    }

    fn run(&self, doc: &GraphDoc, target: &str, http: &FakeHttp, control: &Control) -> Report {
        exec::run_with(doc, target, Dirs::under(&self.0), http, &|_| None, control)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn node(id: &str, data: NodeData) -> NodeDoc {
    NodeDoc {
        id: id.into(),
        x: 0.0,
        y: 0.0,
        kind: None,
        name: None,
        color: None,
        data: Some(data),
    }
}

fn input(id: &str, name: &str, kind: InputType, default: &str) -> NodeDoc {
    let mut node = node(
        id,
        NodeData::Input(InputConfig {
            kind,
            default: default.into(),
            required: true,
        }),
    );
    node.name = Some(name.into());
    node
}

fn graph(nodes: Vec<NodeDoc>, edges: &[(&str, &str)]) -> GraphDoc {
    GraphDoc {
        name: "parent".into(),
        nodes,
        edges: edges
            .iter()
            .map(|(from, to)| EdgeDoc {
                output: None,
                from: (*from).into(),
                to: (*to).into(),
            })
            .collect(),
        ..GraphDoc::default()
    }
}

fn call(id: &str, name: &str, output: &str, inputs: &[(&str, Binding)]) -> NodeDoc {
    node(
        id,
        NodeData::Workflow(Config {
            graph: name.into(),
            output: output.into(),
            inputs: inputs
                .iter()
                .map(|(n, v)| ((*n).into(), v.clone()))
                .collect(),
        }),
    )
}

fn value(value: &str) -> Binding {
    Binding::Value {
        value: value.into(),
    }
}
fn wire(node: &str) -> Binding {
    Binding::Output { node: node.into() }
}
fn writer(id: &str, path: &str, review: bool) -> NodeDoc {
    node(
        id,
        NodeData::File(FileConfig {
            path: path.into(),
            mode: WriteMode::Overwrite,
            review,
            unfence: false,
            ..FileConfig::default()
        }),
    )
}

fn agent(id: &str) -> NodeDoc {
    node(
        id,
        NodeData::Agent(AgentConfig {
            provider: Provider::Custom,
            endpoint: "http://localhost:1234/v1".into(),
            model: "test".into(),
            prompt: "Respond.".into(),
            ..AgentConfig::default()
        }),
    )
}

fn http() -> FakeHttp {
    FakeHttp::new(200, r#"{"choices":[{"message":{"content":"answer"}}]}"#)
}

fn port(from: &str, name: &str, to: &str) -> EdgeDoc {
    EdgeDoc {
        from: from.into(),
        to: to.into(),
        output: Some(name.into()),
    }
}

#[test]
fn published_outputs_share_execution_templates_cache_and_history() {
    let f = Fixture::new();
    let mut child = graph(
        vec![
            agent("n0"),
            writer("n1", "one.txt", false),
            writer("n2", "two.txt", false),
        ],
        &[("n0", "n1"), ("n0", "n2")],
    );
    child.outputs = BTreeMap::from([
        ("summary".into(), "n1".into()),
        ("details".into(), "n2".into()),
    ]);
    f.save("child", &child);
    let mut source = call("n0", "child", "", &[]);
    source.name = Some("report".into());
    let mut consumer = agent("n1");
    if let Some(NodeData::Agent(config)) = &mut consumer.data {
        config.message = "Summary: {{report.summary}}; details: {{n0.details}}".into();
    }
    let mut parent = graph(vec![source, consumer], &[]);
    parent.edges = vec![port("n0", "summary", "n1"), port("n0", "details", "n1")];
    let control = Control {
        cache: Some(Arc::new(Mutex::new(Cache::persistent(f.0.join("cache"))))),
        ..Control::default()
    };
    let http = http();
    let first = f.run(&parent, "n1", &http, &control);
    first.result.as_ref().unwrap();
    assert_eq!(
        http.seen.lock().unwrap().len(),
        2,
        "the shared child agent runs once for both ports"
    );
    assert_eq!(first.calls["n0"].run.nodes.len(), 3);
    assert_eq!(
        first.named_outputs["n0"]["summary"].as_text(),
        Some("answer")
    );
    assert!(
        http.seen.lock().unwrap()[1]
            .body
            .to_string()
            .contains("Summary: answer; details: answer")
    );
    let run = history::record(&parent, &["n1".into()], &first, control.cache.as_deref());
    let encoded = serde_json::to_vec(&run).unwrap();
    let run: history::Run = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(run.nodes["n0"].named_outputs.len(), 2);
    assert_eq!(
        run.report(control.cache.as_deref()).named_outputs["n0"]["details"].bytes,
        b"answer"
    );
    let second = f.run(&parent, "n1", &http, &control);
    second.result.unwrap();
    assert_eq!(http.seen.lock().unwrap().len(), 2);
    assert_eq!(second.calls["n0"].run.nodes["n0"].state, State::Reused);
    assert!(second.reused.contains(&"n1".into()));
    // The immutable saved snapshot participates in every child cache key.
    child.nodes[0].name = Some("changed".into());
    f.save("child", &child);
    f.run(&parent, "n1", &http, &control).result.unwrap();
    assert_eq!(http.seen.lock().unwrap().len(), 4);
}

#[test]
fn nested_publications_forward_exact_text_and_binary_ports_once() {
    let f = Fixture::new();
    fs::write(f.0.join("data/raw.bin"), [0xff, 0x01, 0x00]).unwrap();
    let mut leaf = graph(
        vec![
            input("n0", "caption", InputType::Text, "Nested caption"),
            node(
                "n1",
                NodeData::Data(DataConfig {
                    path: "raw.bin".into(),
                    format: DataFormat::Binary,
                }),
            ),
        ],
        &[],
    );
    leaf.outputs = BTreeMap::from([
        ("caption".into(), "n0".into()),
        ("image".into(), "n1".into()),
    ]);
    f.save("leaf", &leaf);
    let mut middle = graph(vec![call("n0", "leaf", "", &[])], &[]);
    middle.outputs = BTreeMap::from([
        ("headline".into(), "n0.caption".into()),
        ("picture".into(), "n0.image".into()),
    ]);
    f.save("middle", &middle);
    let mut parent = graph(
        vec![
            call("n0", "middle", "", &[]),
            writer("n1", "caption.txt", false),
            writer("n2", "image.bin", false),
        ],
        &[],
    );
    parent.edges = vec![port("n0", "headline", "n1"), port("n0", "picture", "n2")];
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let events = snapshots.clone();
    let control = Control {
        cache: Some(Arc::new(Mutex::new(Cache::persistent(f.0.join("cache"))))),
        progress: Some(Arc::new(move |event| {
            if let Progress::Workflow { id, .. } = event {
                events.lock().unwrap().push(id);
            }
        })),
        ..Control::default()
    };
    let http = http();
    let targets = ["n1".into(), "n2".into()];
    let report = exec::run_all_with(
        &parent,
        &targets,
        Dirs::under(&f.0),
        &http,
        &|_| None,
        &control,
    );
    report.result.as_ref().unwrap();
    assert_eq!(
        *snapshots.lock().unwrap(),
        ["n0", "n0/n0"],
        "each nested Workflow snapshot is published once across both forwarded ports"
    );
    assert_eq!(report.calls["n0"].run.targets, ["n0"]);
    assert_eq!(report.calls["n0"].run.calls["n0"].run.nodes.len(), 2);
    assert_eq!(
        report.output_handle("n0.headline").unwrap().as_text(),
        Some("Nested caption")
    );
    assert_eq!(
        report.output_handle("n0.picture").unwrap().bytes,
        [0xff, 1, 0]
    );
    assert!(!report.output_handle("n0.picture").unwrap().text);
    assert_eq!(
        fs::read_to_string(f.0.join("caption.txt")).unwrap(),
        "Nested caption"
    );
    assert_eq!(fs::read(f.0.join("image.bin")).unwrap(), [0xff, 1, 0]);
    let run = history::record(&parent, &targets, &report, control.cache.as_deref());
    let run: history::Run = serde_json::from_slice(&serde_json::to_vec(&run).unwrap()).unwrap();
    let replay = run.report(control.cache.as_deref());
    assert_eq!(
        replay.output_handle("n0.picture").unwrap().bytes,
        [0xff, 1, 0]
    );
    assert!(!replay.output_handle("n0.picture").unwrap().text);
    let middle_replay = run.calls["n0"].run.report(control.cache.as_deref());
    assert_eq!(
        middle_replay.output_handle("n0.caption").unwrap().as_text(),
        Some("Nested caption")
    );
    assert!(!middle_replay.output_handle("n0.image").unwrap().text);
    assert!(http.seen.lock().unwrap().is_empty());
}

#[test]
fn prepared_snapshots_include_unstarted_nested_calls_before_execution() {
    let f = Fixture::new();
    let mut leaf = graph(vec![input("n0", "text", InputType::Text, "leaf")], &[]);
    leaf.outputs.insert("text".into(), "n0".into());
    f.save("leaf", &leaf);
    let middle = graph(
        vec![
            input("n0", "count", InputType::Number, ""),
            call("n1", "leaf", "", &[]),
        ],
        &[("n0", "n1")],
    );
    f.save("middle", &middle);
    let mut parent = graph(
        vec![
            input("n0", "raw", InputType::Text, "not a number"),
            call("n1", "middle", "n1", &[("count", wire("n0"))]),
        ],
        &[("n0", "n1")],
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = events.clone();
    let control = Control {
        progress: Some(Arc::new(move |event| captured.lock().unwrap().push(event))),
        ..Control::default()
    };
    let report = f.run(&parent, "n1", &http(), &control);
    assert!(
        report
            .result
            .as_ref()
            .unwrap_err()
            .contains("finite JSON number")
    );
    assert_eq!(
        report
            .workflows
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["n1", "n1/n1"]
    );
    let failed_child = &report.calls["n1"].run;
    assert!(failed_child.nodes.is_empty());
    assert!(failed_child.calls.is_empty());
    assert_eq!(
        failed_child
            .workflows
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["n1"]
    );
    let run = history::record(&parent, &["n1".into()], &report, None);
    history::save(&f.0, &run).unwrap();
    leaf.outputs.clear();
    f.save("leaf", &leaf);
    let loaded = history::list(&f.0, &run.graph).pop().unwrap();
    assert_eq!(loaded.workflows, report.workflows);
    assert!(loaded.workflows["n1/n1"].outputs.contains_key("text"));
    assert_eq!(loaded.report(None).workflows, report.workflows);
    assert_eq!(
        loaded.calls["n1"].run.report(None).workflows,
        failed_child.workflows
    );
    let mut legacy = serde_json::to_value(&run).unwrap();
    legacy.as_object_mut().unwrap().remove("workflows");
    let legacy: history::Run = serde_json::from_value(legacy).unwrap();
    assert!(legacy.workflows.is_empty());
    let recorded = events.lock().unwrap();
    let snapshots: Vec<_> = recorded
        .iter()
        .filter_map(|event| match event {
            Progress::Workflow { id, graph } => Some((id.as_str(), graph)),
            _ => None,
        })
        .collect();
    assert_eq!(
        snapshots.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        ["n1", "n1/n1"]
    );
    assert!(snapshots[1].1.outputs.contains_key("text"));
    assert!(matches!(recorded.first(), Some(Progress::Workflow { .. })));
    assert!(!recorded.iter().any(|event| matches!(event, Progress::Node { id, state: NodeState::Running } if id.contains('/'))), "preparation snapshots do not imply child execution");
    drop(recorded);
    f.save("leaf", &report.workflows["n1/n1"]);
    let canceled = Arc::new(AtomicBool::new(false));
    let cancel_on_snapshot = canceled.clone();
    let canceled_control = Control {
        cancel: canceled,
        progress: Some(Arc::new(move |event| {
            if matches!(event, Progress::Workflow { .. }) {
                cancel_on_snapshot.store(true, Ordering::Relaxed);
            }
        })),
        ..Control::default()
    };
    let canceled_report = f.run(&parent, "n1", &http(), &canceled_control);
    assert_eq!(
        canceled_report.result.as_ref().unwrap_err(),
        exec::CANCELLED
    );
    assert_eq!(canceled_report.workflows.len(), 2);
    assert!(canceled_report.outputs.is_empty());
    assert_eq!(
        history::record(&parent, &["n1".into()], &canceled_report, None)
            .workflows
            .len(),
        2
    );
    events.lock().unwrap().clear();
    if let Some(NodeData::Workflow(config)) = &mut parent.nodes[1].data {
        config.graph = "missing".into();
    }
    f.run(&parent, "n1", &http(), &control).result.unwrap_err();
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, Progress::Workflow { .. })),
        "preparation errors do not publish snapshots"
    );
    events.lock().unwrap().clear();
    control.cancel.store(true, Ordering::Relaxed);
    f.run(&parent, "n1", &http(), &control).result.unwrap_err();
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, Progress::Workflow { .. })),
        "canceled runs do not publish snapshots"
    );
}

#[test]
fn large_allowed_nested_snapshots_save_once_and_restore_relative_maps() {
    let f = Fixture::new();
    let leaf = graph(
        (0..12)
            .map(|index| {
                input(
                    &format!("n{index}"),
                    &format!("field{index}"),
                    InputType::Text,
                    &"x".repeat(workflow::MAX_VALUE_BYTES),
                )
            })
            .collect(),
        &[],
    );
    f.save("large-leaf", &leaf);
    let mut previous = "large-leaf".to_string();
    for depth in 0..5 {
        let name = format!("layer-{depth}");
        f.save(&name, &graph(vec![call("n0", &previous, "n0", &[])], &[]));
        previous = name;
    }
    let parent = graph(vec![call("n0", &previous, "n0", &[])], &[]);
    let report = f.run(&parent, "n0", &http(), &Control::default());
    report.result.as_ref().unwrap();
    assert_eq!(report.workflows.len(), 6);
    let snapshot_bytes: usize = report
        .workflows
        .values()
        .map(|graph| serde_json::to_vec(graph).unwrap().len())
        .sum();
    assert!(snapshot_bytes < MAX_SNAPSHOT_BYTES);
    let run = history::record(&parent, &["n0".into()], &report, None);
    assert!(
        serde_json::to_vec(&run).unwrap().len() > crate::storage::MAX_DOCUMENT_BYTES as usize,
        "uncompacted descendant maps exceed the history limit"
    );
    let path = history::save(&f.0, &run).unwrap();
    assert!(fs::metadata(&path).unwrap().len() < crate::storage::MAX_DOCUMENT_BYTES);
    let compact: history::Run = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(compact.workflows, run.workflows);
    assert!(compact.calls["n0"].run.workflows.is_empty());
    let loaded = history::list(&f.0, &run.graph).pop().unwrap();
    assert_eq!(
        loaded, run,
        "loading restores every child run's relative snapshot API"
    );
    let mut child = &loaded;
    for remaining in (0..6).rev() {
        child = &child.calls["n0"].run;
        assert_eq!(child.report(None).workflows.len(), remaining);
    }
}

#[test]
fn legacy_selected_nodes_ignore_unrelated_invalid_publications() {
    let f = Fixture::new();
    let mut child = graph(
        vec![
            input("n0", "text", InputType::Text, "legacy"),
            call("n1", "missing", "", &[]),
        ],
        &[],
    );
    child.outputs = BTreeMap::from([
        ("bad.name".into(), "not-a-handle".into()),
        ("unreachable".into(), "n1.missing".into()),
    ]);
    f.save("child", &child);
    let parent = graph(vec![call("n0", "child", "n0", &[])], &[]);
    let report = f.run(&parent, "n0", &http(), &Control::default());
    assert_eq!(report.result.as_ref().unwrap().as_text(), Some("legacy"));
    assert!(report.named_outputs.is_empty());
    assert_eq!(report.calls["n0"].run.nodes.len(), 1);
    assert!(report.calls["n0"].run.calls.is_empty());
}

#[test]
fn invalid_nested_publications_fail_before_parent_writes() {
    let f = Fixture::new();
    let mut leaf = graph(vec![input("n0", "text", InputType::Text, "child")], &[]);
    leaf.outputs = BTreeMap::from([
        ("first".into(), "n0".into()),
        ("second".into(), "n0".into()),
    ]);
    f.save("leaf", &leaf);
    let mut middle = graph(vec![call("n0", "leaf", "", &[])], &[]);
    let parent = graph(
        vec![
            input("n0", "trigger", InputType::Text, "parent"),
            writer("n1", "never.txt", false),
            call("n2", "middle", "", &[]),
        ],
        &[("n0", "n1"), ("n1", "n2")],
    );
    for (handle, expected) in [
        ("n0.missing", "does not publish missing"),
        ("n0", "publishes multiple outputs"),
        ("n0.first.extra", "named output handle"),
        ("n99.first", "not in the graph"),
    ] {
        middle.outputs = BTreeMap::from([("forwarded".into(), handle.into())]);
        f.save("middle", &middle);
        let report = f.run(&parent, "n2", &http(), &Control::default());
        let error = report.result.unwrap_err();
        assert!(error.contains(expected), "{handle}: {error}");
        assert!(report.outputs.is_empty());
        assert!(!f.0.join("never.txt").exists());
    }
}

#[test]
fn published_ports_map_to_child_fields_by_actual_wire_handles() {
    let f = Fixture::new();
    let mut child = graph(
        vec![
            input("n0", "left", InputType::Text, "alpha"),
            input("n1", "right", InputType::Text, "beta"),
        ],
        &[],
    );
    child.outputs = BTreeMap::from([
        ("summary".into(), "n0".into()),
        ("details".into(), "n1".into()),
    ]);
    f.save("source", &child);
    f.save(
        "destination",
        &graph(vec![input("n0", "text", InputType::Text, "")], &[]),
    );
    let mut parent = graph(
        vec![
            call("n0", "source", "", &[]),
            call("n1", "destination", "n0", &[("text", wire("n0.details"))]),
        ],
        &[],
    );
    parent.edges = vec![port("n0", "details", "n1")];
    let report = f.run(&parent, "n1", &http(), &Control::default());
    assert_eq!(report.result.unwrap().as_text(), Some("beta"));
    assert_eq!(report.calls["n1"].run.inputs["text"], "beta");
    if let Some(NodeData::Workflow(config)) = &mut parent.nodes[1].data {
        config.inputs.insert("text".into(), wire("n0.summary"));
    }
    let report = f.run(&parent, "n1", &http(), &Control::default());
    assert!(report.result.unwrap_err().contains("wired directly"));
    assert!(
        report.outputs.is_empty(),
        "mapping errors are caught before upstream execution"
    );
}

#[test]
fn published_binary_ports_survive_writes_cache_history_and_single_port_default() {
    let f = Fixture::new();
    fs::write(f.0.join("data/raw.bin"), [0xff, 0x00, 0x01]).unwrap();
    fs::write(f.0.join("data/ascii.bin"), b"caption").unwrap();
    let mut child = graph(
        vec![
            node(
                "n0",
                NodeData::Data(DataConfig {
                    path: "raw.bin".into(),
                    format: DataFormat::Binary,
                }),
            ),
            input("n1", "caption", InputType::Text, "caption"),
        ],
        &[],
    );
    child.outputs = BTreeMap::from([
        ("image".into(), "n0".into()),
        ("caption".into(), "n1".into()),
    ]);
    child.nodes.push(node(
        "n2",
        NodeData::Data(DataConfig {
            path: "ascii.bin".into(),
            format: DataFormat::Binary,
        }),
    ));
    child.outputs.insert("ascii".into(), "n2".into());
    f.save("binary", &child);
    let mut parent = graph(
        vec![
            call("n0", "binary", "", &[]),
            writer("n1", "copy.bin", false),
        ],
        &[],
    );
    parent.edges = vec![port("n0", "image", "n1")];
    let control = Control {
        cache: Some(Arc::new(Mutex::new(Cache::persistent(f.0.join("cache"))))),
        ..Control::default()
    };
    let report = f.run(&parent, "n1", &http(), &control);
    report.result.as_ref().unwrap();
    assert_eq!(fs::read(f.0.join("copy.bin")).unwrap(), [0xff, 0, 1]);
    assert!(!report.named_outputs["n0"]["image"].text);
    let run = history::record(&parent, &["n1".into()], &report, control.cache.as_deref());
    let replay = run.report(control.cache.as_deref());
    assert_eq!(replay.named_outputs["n0"]["image"].bytes, [0xff, 0, 1]);
    assert!(!replay.named_outputs["n0"]["image"].text);
    assert!(
        !replay.named_outputs["n0"]["ascii"].text,
        "binary metadata survives even when another port has identical text bytes"
    );
    assert!(replay.named_outputs["n0"]["caption"].text);
    f.save(
        "text",
        &graph(vec![input("n0", "text", InputType::Text, "")], &[]),
    );
    let mut invalid = graph(
        vec![
            call("n0", "binary", "", &[]),
            writer("n1", "never.bin", false),
            call("n2", "text", "n0", &[("text", wire("n0.image"))]),
        ],
        &[("n1", "n2")],
    );
    invalid
        .edges
        .extend([port("n0", "image", "n1"), port("n0", "image", "n2")]);
    let failure = f.run(&invalid, "n2", &http(), &control);
    assert!(failure.result.unwrap_err().contains("binary data"));
    assert!(
        !f.0.join("never.bin").exists(),
        "known binary mappings fail before parent writes"
    );
    child.outputs.remove("caption");
    child.outputs.remove("ascii");
    f.save("binary", &child);
    parent.edges[0].output = None;
    let report = f.run(&parent, "n1", &http(), &control);
    assert_eq!(report.result.unwrap().bytes, [0xff, 0, 1]);
}

#[test]
fn invalid_publications_and_wires_fail_before_parent_side_effects() {
    let f = Fixture::new();
    let mut child = graph(vec![input("n0", "text", InputType::Text, "child")], &[]);
    let parent = graph(
        vec![
            input("n0", "trigger", InputType::Text, "parent"),
            writer("n1", "never.txt", false),
            call("n2", "child", "", &[]),
        ],
        &[("n0", "n1"), ("n1", "n2")],
    );
    for (outputs, expected) in [
        (BTreeMap::new(), "no published outputs"),
        (
            BTreeMap::from([("bad.name".into(), "n0".into())]),
            "published output",
        ),
        (
            BTreeMap::from([("summary".into(), "n99".into())]),
            "not in the graph",
        ),
    ] {
        child.outputs = outputs;
        f.save("child", &child);
        assert!(
            f.run(&parent, "n2", &http(), &Control::default())
                .result
                .unwrap_err()
                .contains(expected)
        );
        assert!(!f.0.join("never.txt").exists());
    }
    child.outputs = BTreeMap::from([
        ("summary".into(), "n0".into()),
        ("details".into(), "n0".into()),
    ]);
    f.save("child", &child);
    let mut parent = graph(
        vec![
            call("n0", "child", "", &[]),
            writer("n1", "never.txt", false),
        ],
        &[("n0", "n1")],
    );
    assert!(
        f.run(&parent, "n1", &http(), &Control::default())
            .result
            .unwrap_err()
            .contains("choose n0.details, n0.summary")
    );
    parent.edges[0].output = Some("missing".into());
    assert!(
        f.run(&parent, "n1", &http(), &Control::default())
            .result
            .unwrap_err()
            .contains("does not publish missing")
    );
    assert!(!f.0.join("never.txt").exists());
}

#[test]
fn workflow_values_wires_defaults_and_selected_output_round_trip() {
    let f = Fixture::new();
    let child = graph(
        vec![
            input("n0", "text", InputType::Text, ""),
            input("n1", "count", InputType::Number, " 2.5 "),
            input("n2", "flag", InputType::Boolean, "true"),
            input("n3", "unused", InputType::Text, ""),
            agent("n4"),
        ],
        &[("n0", "n4"), ("n1", "n4"), ("n2", "n4")],
    );
    f.save("child", &child);
    let parent = graph(
        vec![
            input("n0", "question", InputType::Text, "hello"),
            call(
                "n1",
                "child",
                "n4",
                &[("text", wire("n0")), ("flag", value("false"))],
            ),
        ],
        &[("n0", "n1")],
    );
    let original = serde_json::to_string(&parent).unwrap();
    let decoded: GraphDoc = serde_json::from_str(&original).unwrap();
    assert_eq!(decoded, parent);
    let http = http();
    let report = f.run(&parent, "n1", &http, &Control::default());
    assert_eq!(report.result.as_ref().unwrap().as_text(), Some("answer"));
    assert_eq!(
        report.inputs,
        Values::from([("question".into(), "hello".into())])
    );
    let child = &report.calls["n1"];
    assert_eq!(
        child.run.inputs,
        Values::from([
            ("text".into(), "hello".into()),
            ("count".into(), "2.5".into()),
            ("flag".into(), "false".into())
        ])
    );
    assert!(!child.run.nodes.contains_key("n3"));
    assert_eq!(serde_json::to_string(&parent).unwrap(), original);
    let request = &http.seen.lock().unwrap()[0];
    assert!(
        request
            .body
            .to_string()
            .contains("hello\\n\\n2.5\\n\\nfalse")
    );
}

#[test]
fn workflow_static_errors_prevent_parent_writes_and_independent_targets_continue() {
    let f = Fixture::new();
    f.save(
        "child",
        &graph(vec![input("n0", "count", InputType::Number, "")], &[]),
    );
    let variants = [
        ("child", "n0", vec![], "required"),
        ("child", "n0", vec![("count", value("NaN"))], "finite"),
        ("child", "n0", vec![("typo", value("2"))], "unknown"),
        ("child", "n0", vec![("count", wire("n9"))], "wired directly"),
        ("child", "n99", vec![], "not in the graph"),
        ("missing", "n0", vec![], "missing"),
        ("../child", "n0", vec![], "saved graph"),
    ];
    for (name, output, bindings, expected) in variants {
        let doc = graph(
            vec![
                input("n0", "text", InputType::Text, "side effect"),
                writer("n1", "should-not-exist.txt", false),
                call("n2", name, output, &bindings),
                input("n3", "independent", InputType::Text, "ok"),
            ],
            &[("n0", "n1"), ("n1", "n2")],
        );
        let report = exec::run_all_with(
            &doc,
            &["n2".into(), "n3".into()],
            Dirs::under(&f.0),
            &http(),
            &|_| None,
            &Control::default(),
        );
        assert!(report.result.unwrap_err().contains(expected), "{expected}");
        assert!(!f.0.join("should-not-exist.txt").exists());
        assert_eq!(report.outputs["n3"].as_text(), Some("ok"));
        assert_eq!(report.failed.as_deref(), Some("n2"));
    }
}

#[test]
fn workflow_dynamic_values_are_validated_before_any_child_effect() {
    let f = Fixture::new();
    f.save(
        "child",
        &graph(
            vec![
                input("n0", "text", InputType::Text, "write"),
                writer("n1", "first.txt", false),
                input("n2", "count", InputType::Number, ""),
                writer("n3", "last.txt", false),
            ],
            &[("n0", "n1"), ("n1", "n3"), ("n2", "n3")],
        ),
    );
    let parent = graph(
        vec![
            input("n0", "bad", InputType::Text, "NaN"),
            call("n1", "child", "n3", &[("count", wire("n0"))]),
        ],
        &[("n0", "n1")],
    );
    let report = f.run(&parent, "n1", &http(), &Control::default());
    assert!(matches!(
        report.calls["n1"].run.outcome,
        Outcome::Failed { .. }
    ));
    assert!(report.calls["n1"].run.nodes.is_empty());
    assert!(report.result.unwrap_err().contains("finite"));
    assert!(!f.0.join("first.txt").exists());
    assert!(!f.0.join("last.txt").exists());
}

#[test]
fn workflow_recursion_and_nested_invalid_branches_fail_before_parent_effects() {
    let f = Fixture::new();
    let parent = graph(
        vec![
            input("n0", "text", InputType::Text, "write"),
            writer("n1", "first.txt", false),
            call("n2", "a", "n0", &[]),
        ],
        &[("n0", "n1"), ("n1", "n2")],
    );
    for other in ["a", "b", "parent"] {
        f.save("a", &graph(vec![call("n0", other, "n0", &[])], &[]));
        f.save("b", &graph(vec![call("n0", "a", "n0", &[])], &[]));
        let report = f.run(&parent, "n2", &http(), &Control::default());
        assert!(report.result.unwrap_err().contains("recursive workflow"));
        assert!(!f.0.join("first.txt").exists());
    }
    f.save(
        "a",
        &graph(
            vec![
                writer("n0", "second.txt", false),
                call("n1", "missing", "n0", &[]),
            ],
            &[("n1", "n0")],
        ),
    );
    assert!(
        f.run(&parent, "n2", &http(), &Control::default())
            .result
            .is_err()
    );
    assert!(!f.0.join("first.txt").exists());
    assert!(!f.0.join("second.txt").exists());
}

#[test]
fn workflow_depth_call_count_and_snapshot_size_are_bounded() {
    let f = Fixture::new();
    for count in [MAX_DEPTH, MAX_DEPTH + 1] {
        for i in 0..count {
            f.save(
                &format!("level{i}"),
                &graph(
                    vec![if i + 1 == count {
                        input("n0", "text", InputType::Text, "done")
                    } else {
                        call("n0", &format!("level{}", i + 1), "n0", &[])
                    }],
                    &[],
                ),
            );
        }
        let parent = graph(vec![call("n0", "level0", "n0", &[])], &[]);
        let report = f.run(&parent, "n0", &http(), &Control::default());
        if count == MAX_DEPTH {
            assert_eq!(report.result.unwrap().as_text(), Some("done"));
        } else {
            assert!(report.result.unwrap_err().contains("at most 8"));
        }
    }
    f.save(
        "tiny",
        &graph(vec![input("n0", "text", InputType::Text, "ok")], &[]),
    );
    let wide = graph(
        (0..=MAX_CALLS)
            .map(|i| call(&format!("n{i}"), "tiny", "n0", &[]))
            .collect(),
        &[],
    );
    let plans = prepare(&wide, &exec::sinks(&wide), &f.0, &AtomicBool::new(false));
    assert!(
        plans
            .values()
            .any(|r| r.as_ref().is_err_and(|e| e.contains("at most 128")))
    );
    let huge = graph(
        (0..20)
            .map(|i| {
                input(
                    &format!("n{i}"),
                    &format!("text{i}"),
                    InputType::Text,
                    &"x".repeat(workflow::MAX_VALUE_BYTES),
                )
            })
            .collect(),
        &[],
    );
    f.save("huge", &huge);
    let parent = graph(vec![call("n0", "huge", "n0", &[])], &[]);
    assert!(
        f.run(&parent, "n0", &http(), &Control::default())
            .result
            .unwrap_err()
            .contains("4 MB")
    );
}

#[test]
fn workflow_cache_tracks_inputs_full_snapshots_and_nested_changes() {
    let f = Fixture::new();
    let mut child = graph(
        vec![input("n0", "text", InputType::Text, "default"), agent("n1")],
        &[("n0", "n1")],
    );
    f.save("leaf", &child);
    f.save(
        "middle",
        &graph(
            vec![
                input("n0", "text", InputType::Text, ""),
                call("n1", "leaf", "n1", &[("text", wire("n0"))]),
            ],
            &[("n0", "n1")],
        ),
    );
    let parent = graph(
        vec![
            input("n0", "text", InputType::Text, "first"),
            call("n1", "middle", "n1", &[("text", wire("n0"))]),
            agent("n2"),
        ],
        &[("n0", "n1"), ("n1", "n2")],
    );
    let http = http();
    let mut control = Control {
        cache: Some(Arc::new(Mutex::new(Cache::persistent(f.0.join("cache"))))),
        ..Control::default()
    };
    for expected in [2, 2] {
        f.run(&parent, "n2", &http, &control).result.unwrap();
        assert_eq!(http.seen.lock().unwrap().len(), expected);
    }
    control.cache = Some(Arc::new(Mutex::new(Cache::persistent(f.0.join("cache")))));
    let cached = f.run(&parent, "n2", &http, &control);
    assert_eq!(
        cached.calls["n1"].run.calls["n1"].run.nodes["n1"].state,
        State::Reused
    );
    assert_eq!(http.seen.lock().unwrap().len(), 2);
    control.inputs.insert("text".into(), "second".into());
    f.run(&parent, "n2", &http, &control).result.unwrap();
    assert_eq!(http.seen.lock().unwrap().len(), 4);
    // Even a snapshot change outside the selected dependency chain changes the key.
    child
        .nodes
        .push(input("n9", "unused", InputType::Text, "changed"));
    f.save("leaf", &child);
    f.run(&parent, "n2", &http, &control).result.unwrap();
    assert_eq!(http.seen.lock().unwrap().len(), 6);
    control.fresh = true;
    f.run(&parent, "n2", &http, &control).result.unwrap();
    assert_eq!(http.seen.lock().unwrap().len(), 8);
}

#[test]
fn workflow_file_paths_and_contents_invalidate_cache_and_binary_output_survives() {
    let f = Fixture::new();
    fs::write(f.0.join("data/a.txt"), "same").unwrap();
    fs::write(f.0.join("data/b.txt"), "same").unwrap();
    f.save(
        "child",
        &graph(
            vec![input("n0", "file", InputType::File, ""), agent("n1")],
            &[("n0", "n1")],
        ),
    );
    let parent = graph(
        vec![
            input("n0", "path", InputType::Text, "a.txt"),
            call("n1", "child", "n1", &[("file", wire("n0"))]),
        ],
        &[("n0", "n1")],
    );
    let http = http();
    let mut control = Control {
        cache: Some(Arc::new(Mutex::new(Cache::default()))),
        ..Control::default()
    };
    f.run(&parent, "n1", &http, &control).result.unwrap();
    control.inputs.insert("path".into(), "b.txt".into());
    f.run(&parent, "n1", &http, &control).result.unwrap();
    fs::write(f.0.join("data/b.txt"), "changed").unwrap();
    f.run(&parent, "n1", &http, &control).result.unwrap();
    assert_eq!(http.seen.lock().unwrap().len(), 3);
    fs::write(f.0.join("data/raw.bin"), [0xff, 0x01]).unwrap();
    f.save(
        "binary",
        &graph(
            vec![node(
                "n0",
                NodeData::Data(DataConfig {
                    path: "raw.bin".into(),
                    format: DataFormat::Binary,
                }),
            )],
            &[],
        ),
    );
    let parent = graph(
        vec![
            call("n0", "binary", "n0", &[]),
            writer("n1", "copy.bin", false),
        ],
        &[("n0", "n1")],
    );
    f.run(&parent, "n1", &http, &Control::default())
        .result
        .unwrap();
    assert_eq!(fs::read(f.0.join("copy.bin")).unwrap(), [0xff, 1]);
    let parent = graph(
        vec![
            call("n0", "binary", "n0", &[]),
            call("n1", "child", "n1", &[("file", wire("n0"))]),
        ],
        &[("n0", "n1")],
    );
    assert!(
        f.run(&parent, "n1", &http, &Control::default())
            .result
            .unwrap_err()
            .contains("binary data")
    );
}

#[test]
fn workflow_snapshot_is_fixed_before_upstream_execution_and_persists_in_history() {
    let f = Fixture::new();
    let child = graph(vec![input("n0", "text", InputType::Text, "snapshot")], &[]);
    f.save("child", &child);
    let parent = graph(
        vec![
            input("n0", "text", InputType::Text, "trigger"),
            call("n1", "child", "n0", &[]),
        ],
        &[("n0", "n1")],
    );
    let path = f.0.join("graphs/child.json");
    let control = Control {
        progress: Some(Arc::new(move |p| {
            if matches!(p, Progress::Node { id, state: NodeState::Running } if id == "n0") {
                fs::write(&path, "invalid JSON after execution started").unwrap();
            }
        })),
        ..Control::default()
    };
    let report = f.run(&parent, "n1", &http(), &control);
    assert_eq!(report.result.as_ref().unwrap().as_text(), Some("snapshot"));
    let record = history::record(&parent, &["n1".into()], &report, None);
    history::save(&f.0, &record).unwrap();
    let loaded = history::list(&f.0, "parent").pop().unwrap();
    assert_eq!(loaded.calls["n1"].graph.nodes, child.nodes);
    assert_eq!(loaded.calls["n1"].run.inputs["text"], "snapshot");
    assert_eq!(loaded.report(None).calls, record.calls);
    let mut old = serde_json::to_value(record).unwrap();
    old.as_object_mut().unwrap().remove("calls");
    assert!(
        serde_json::from_value::<history::Run>(old)
            .unwrap()
            .calls
            .is_empty()
    );
}

#[test]
fn workflow_repeated_calls_have_distinct_progress_inputs_and_history() {
    let f = Fixture::new();
    f.save(
        "child",
        &graph(vec![input("n0", "text", InputType::Text, "")], &[]),
    );
    let parent = graph(
        vec![
            call("n0", "child", "n0", &[("text", value("first"))]),
            call("n1", "child", "n0", &[("text", value("second"))]),
            writer("n2", "joined.txt", false),
        ],
        &[("n0", "n2"), ("n1", "n2")],
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let control = Control {
        progress: Some(Arc::new(move |p| sink.lock().unwrap().push(p))),
        ..Control::default()
    };
    let report = f.run(&parent, "n2", &http(), &control);
    report.result.unwrap();
    assert_eq!(
        fs::read_to_string(f.0.join("joined.txt")).unwrap(),
        "first\n\nsecond"
    );
    assert_eq!(report.calls["n0"].run.inputs["text"], "first");
    assert_eq!(report.calls["n1"].run.inputs["text"], "second");
    let seen = seen.lock().unwrap();
    for expected in ["n0/n0", "n1/n0"] {
        assert!(
            seen.iter().any(
                |p| matches!(p, Progress::Node { id, state: NodeState::Done } if id == expected)
            )
        );
    }
}

#[test]
fn workflow_nested_reviews_share_apply_reject_cancel_and_never_skip_writes() {
    let f = Fixture::new();
    let child: GraphDoc =
        serde_json::from_str(include_str!("../examples/reviewed-note.json")).unwrap();
    f.save("leaf", &child);
    f.save("middle", &graph(vec![call("n0", "leaf", "n1", &[])], &[]));
    let parent = graph(
        vec![call("n0", "middle", "n0", &[]), agent("n1")],
        &[("n0", "n1")],
    );
    let http = http();
    let mut control = Control {
        cache: Some(Arc::new(Mutex::new(Cache::default()))),
        ..Control::default()
    };
    assert!(
        f.run(&parent, "n1", &http, &control)
            .result
            .unwrap_err()
            .contains("requires review")
    );
    for cancel in [false, true] {
        let flag = control.cancel.clone();
        control.review = Some(Arc::new(move |request| {
            assert_eq!(request.node, "n0/n0/n1");
            if cancel {
                flag.store(true, Ordering::Relaxed);
                Ok(review::Decision::Apply)
            } else {
                Ok(review::Decision::Reject)
            }
        }));
        let report = f.run(&parent, "n1", &http, &control);
        let error = report.result.unwrap_err();
        if cancel {
            assert_eq!(error, exec::CANCELLED);
        } else {
            assert!(error.contains(review::REJECTED));
            assert!(matches!(
                report.calls["n0"].run.calls["n0"].run.outcome,
                Outcome::Failed { .. }
            ));
        }
        assert!(!f.0.join("generated").exists());
        assert!(http.seen.lock().unwrap().is_empty());
    }
    control.cancel.store(false, Ordering::Relaxed);
    let reviews = Arc::new(Mutex::new(0));
    let count = reviews.clone();
    control.review = Some(Arc::new(move |_| {
        *count.lock().unwrap() += 1;
        Ok(review::Decision::Apply)
    }));
    for _ in 0..2 {
        f.run(&parent, "n1", &http, &control).result.unwrap();
    }
    assert_eq!(
        *reviews.lock().unwrap(),
        2,
        "a workflow must never cache away a file write"
    );
    assert_eq!(http.seen.lock().unwrap().len(), 1);
    assert!(f.0.join("generated/note.md").exists());
}

#[test]
fn workflow_child_failures_and_streams_keep_their_namespace() {
    let f = Fixture::new();
    f.save("child", &graph(vec![agent("n0")], &[]));
    let parent = graph(
        vec![
            call("n0", "child", "n0", &[]),
            input("n1", "other", InputType::Text, "ok"),
        ],
        &[],
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let control = Control {
        progress: Some(Arc::new(move |p| sink.lock().unwrap().push(p))),
        ..Control::default()
    };
    let broken = FakeHttp::new(500, "service failed");
    let report = exec::run_all_with(
        &parent,
        &["n0".into(), "n1".into()],
        Dirs::under(&f.0),
        &broken,
        &|_| None,
        &control,
    );
    assert_eq!(report.failed.as_deref(), Some("n0"), "{report:#?}");
    assert!(report.calls.contains_key("n0"), "{report:#?}");
    assert_eq!(report.calls["n0"].run.nodes["n0"].state, State::Failed);
    assert!(report.outputs.contains_key("n1"));
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|p| matches!(p, Progress::Node { id, state: NodeState::Failed } if id == "n0/n0"))
    );
    let streaming = FakeHttp::new(
        200,
        "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: [DONE]\n\n",
    );
    f.run(&parent, "n0", &streaming, &control).result.unwrap();
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|p| matches!(p, Progress::Text { id, text } if id == "n0/n0" && text == "hello"))
    );
}

#[test]
fn workflow_example_runs_offline_with_parent_values_and_a_child_review() {
    let f = Fixture::new();
    let child: GraphDoc =
        serde_json::from_str(include_str!("../examples/reviewed-note.json")).unwrap();
    let parent: GraphDoc =
        serde_json::from_str(include_str!("../examples/call-reviewed-note.json")).unwrap();
    f.save("reviewed-note", &child);
    let http = http();
    let control = Control {
        inputs: Values::from([("message".into(), "# From the parent\n".into())]),
        review: Some(Arc::new(|request| {
            assert_eq!(request.node, "n1/n1");
            assert!(request.files[0].diff.contains("+# From the parent"));
            Ok(review::Decision::Apply)
        })),
        ..Control::default()
    };
    let report = f.run(&parent, "n1", &http, &control);
    assert_eq!(
        report.result.unwrap().as_text(),
        Some("# From the parent\n")
    );
    assert_eq!(report.calls["n1"].run.inputs["note"], "# From the parent\n");
    assert_eq!(
        fs::read_to_string(f.0.join("generated/note.md")).unwrap(),
        "# From the parent\n"
    );
    assert!(http.seen.lock().unwrap().is_empty());
}

#[test]
fn named_output_example_maps_inputs_and_reviews_each_port_offline() {
    let f = Fixture::new();
    let child: GraphDoc =
        serde_json::from_str(include_str!("../examples/note-parts.json")).unwrap();
    let parent: GraphDoc =
        serde_json::from_str(include_str!("../examples/use-note-parts.json")).unwrap();
    f.save("note-parts", &child);
    let reviewed = Arc::new(Mutex::new(Vec::new()));
    let reviews = reviewed.clone();
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let child_snapshots = snapshots.clone();
    let control = Control {
        inputs: Values::from([
            ("title".into(), "# Named heading\n".into()),
            ("message".into(), "A separately reviewed body.\n".into()),
        ]),
        review: Some(Arc::new(move |request| {
            assert_eq!(request.files.len(), 1);
            match request.node.as_str() {
                "n3" => assert!(request.files[0].diff.contains("+# Named heading")),
                "n4" => assert!(
                    request.files[0]
                        .diff
                        .contains("+A separately reviewed body.")
                ),
                other => panic!("unexpected review node {other}"),
            }
            reviews.lock().unwrap().push(request.node.clone());
            Ok(review::Decision::Apply)
        })),
        progress: Some(Arc::new(move |event| {
            if let Progress::Workflow { id, graph } = event {
                assert_eq!(graph.outputs.len(), 2);
                child_snapshots.lock().unwrap().push(id);
            }
        })),
        ..Control::default()
    };
    let http = http();
    let targets = ["n3".into(), "n4".into()];
    let report = exec::run_all_with(
        &parent,
        &targets,
        Dirs::under(&f.0),
        &http,
        &|_| None,
        &control,
    );
    report.result.as_ref().unwrap();
    assert_eq!(*reviewed.lock().unwrap(), ["n3", "n4"]);
    assert_eq!(*snapshots.lock().unwrap(), ["n2"]);
    assert_eq!(report.calls.len(), 1);
    assert_eq!(report.calls["n2"].run.nodes.len(), 2);
    assert_eq!(
        report.calls["n2"].run.inputs,
        Values::from([
            ("heading".into(), "# Named heading\n".into()),
            ("body".into(), "A separately reviewed body.\n".into()),
        ])
    );
    assert_eq!(
        report.named_outputs["n2"]["heading"].as_text(),
        Some("# Named heading\n")
    );
    assert_eq!(
        report.named_outputs["n2"]["body"].as_text(),
        Some("A separately reviewed body.\n")
    );
    assert_eq!(
        fs::read_to_string(f.0.join("generated/heading.md")).unwrap(),
        "# Named heading\n"
    );
    assert_eq!(
        fs::read_to_string(f.0.join("generated/body.md")).unwrap(),
        "A separately reviewed body.\n"
    );
    assert!(http.seen.lock().unwrap().is_empty());
}

#[test]
fn workflow_mapped_values_obey_individual_and_combined_limits() {
    let f = Fixture::new();
    let mut nodes: Vec<_> = (0..5)
        .map(|i| input(&format!("n{i}"), &format!("text{i}"), InputType::Text, ""))
        .collect();
    nodes.push(writer("n5", "never.txt", false));
    let mut child = graph(nodes, &[]);
    child.edges = (0..5)
        .map(|i| EdgeDoc {
            output: None,
            from: format!("n{i}"),
            to: "n5".into(),
        })
        .collect();
    f.save("child", &child);
    let mut config = Config {
        graph: "child".into(),
        output: "n5".into(),
        ..Config::default()
    };
    for i in 0..5 {
        config.inputs.insert(format!("text{i}"), wire("n0"));
    }
    let parent = graph(
        vec![
            node(
                "n0",
                NodeData::Data(DataConfig {
                    path: "large.txt".into(),
                    format: DataFormat::Text,
                }),
            ),
            node("n1", NodeData::Workflow(config)),
        ],
        &[("n0", "n1")],
    );
    for (bytes, expected) in [
        (workflow::MAX_VALUE_BYTES + 1, "longer"),
        (workflow::MAX_VALUE_BYTES, "combined limit"),
    ] {
        fs::write(f.0.join("data/large.txt"), "x".repeat(bytes)).unwrap();
        let report = f.run(&parent, "n1", &http(), &Control::default());
        let error = report.result.unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert!(report.calls["n1"].run.nodes.is_empty());
        assert!(!f.0.join("never.txt").exists());
    }
}

#[test]
fn workflow_cancel_reaches_a_running_child_script() {
    let f = Fixture::new();
    fs::write(
        f.0.join("scripts/wait.sh"),
        "printf started > started.txt\nsleep 30\nprintf finished\n",
    )
    .unwrap();
    f.save(
        "child",
        &graph(
            vec![node(
                "n0",
                NodeData::Script(ScriptConfig {
                    runtime: Runtime::Shell,
                    path: "wait.sh".into(),
                    timeout_secs: 60,
                    ..ScriptConfig::default()
                }),
            )],
            &[],
        ),
    );
    let parent = graph(
        vec![
            call("n0", "child", "n0", &[]),
            writer("n1", "never.txt", false),
        ],
        &[("n0", "n1")],
    );
    let control = Control::default();
    let cancel = control.cancel.clone();
    let dirs = Dirs::under(&f.0);
    let worker = std::thread::spawn(move || {
        exec::run_with(&parent, "n1", dirs, &http(), &|_| None, &control)
    });
    let start = std::time::Instant::now();
    while !f.0.join("scripts/started.txt").exists()
        && start.elapsed() < std::time::Duration::from_secs(5)
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    cancel.store(true, Ordering::Relaxed);
    let report = worker.join().unwrap();
    assert!(
        f.0.join("scripts/started.txt").exists(),
        "script never started"
    );
    assert_eq!(report.result.unwrap_err(), exec::CANCELLED);
    assert_eq!(report.calls["n0"].run.outcome, Outcome::Cancelled);
    assert!(!f.0.join("never.txt").exists());
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
}

#[cfg(unix)]
#[test]
fn workflow_refuses_linked_graph_files_and_directories() {
    let f = Fixture::new();
    let outside = Fixture::new();
    outside.save(
        "child",
        &graph(vec![input("n0", "text", InputType::Text, "outside")], &[]),
    );
    std::os::unix::fs::symlink(
        outside.0.join("graphs/child.json"),
        f.0.join("graphs/child.json"),
    )
    .unwrap();
    let parent = graph(vec![call("n0", "child", "n0", &[])], &[]);
    assert!(
        f.run(&parent, "n0", &http(), &Control::default())
            .result
            .unwrap_err()
            .contains("regular file")
    );
    fs::remove_file(f.0.join("graphs/child.json")).unwrap();
    fs::remove_dir(f.0.join("graphs")).unwrap();
    std::os::unix::fs::symlink(outside.0.join("graphs"), f.0.join("graphs")).unwrap();
    assert!(
        f.run(&parent, "n0", &http(), &Control::default())
            .result
            .unwrap_err()
            .contains("symlinks are refused")
    );
}
