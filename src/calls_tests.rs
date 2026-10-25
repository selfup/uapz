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
fn workflow_mapped_values_obey_individual_and_combined_limits() {
    let f = Fixture::new();
    let mut nodes: Vec<_> = (0..5)
        .map(|i| input(&format!("n{i}"), &format!("text{i}"), InputType::Text, ""))
        .collect();
    nodes.push(writer("n5", "never.txt", false));
    let mut child = graph(nodes, &[]);
    child.edges = (0..5)
        .map(|i| EdgeDoc {
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
