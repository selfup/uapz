// Opt-in native integration smoke test; only injected with debug --smoke-test.
// It uses an explicitly supplied disposable workspace, never the user's library.
(async () => {
  if (window.__uapzSmokeStarted) return;
  window.__uapzSmokeStarted = true;
  const invoke = window.__TAURI_INTERNALS__.invoke;
  const request = (method, args = {}) => invoke("request", { method, args });
  const pause = () => new Promise((resolve) => setTimeout(resolve, 50));
  const assert = (condition, message) => {
    if (!condition) throw new Error(message);
  };
  try {
    for (let i = 0; i < 100 && !document.querySelector(".workspace"); i++)
      await pause();
    assert(
      document.querySelector(".workspace"),
      "React workspace did not mount",
    );
    const bootstrap = await request("bootstrap");
    assert(bootstrap.catalog.nodes.length === 8, "Node catalog IPC failed");
    const graph = {
      version: 1,
      name: "native-smoke",
      view: { pan_x: 40, pan_y: 40, zoom: 1 },
      outputs: {},
      nodes: [
        {
          id: "n0",
          x: 40,
          y: 80,
          name: "message",
          data: {
            type: "input",
            kind: "text",
            default: "Native Tauri execution",
            required: true,
          },
        },
        {
          id: "n1",
          x: 360,
          y: 80,
          name: "save-message",
          data: {
            type: "file",
            path: "native-smoke.txt",
            mode: "overwrite",
            unfence: false,
            bundle: false,
            review: true,
          },
        },
      ],
      edges: [{ from: "n0", to: "n1" }],
    };
    await request("save_graph", { doc: graph });
    const loaded = await request("load_graph", { name: graph.name });
    assert(
      loaded.nodes.length === 2 && loaded.view.zoom === 1,
      "Graph roundtrip failed",
    );
    const prepared = await request("prepare_run", { doc: graph });
    assert(prepared.fields[0].name === "message", "Run inputs missing");
    const { run_id } = await request("start_run", {
      doc: graph,
      inputs: prepared.inputs,
      fresh: true,
    });
    let state;
    for (let i = 0; i < 200; i++) {
      state = await request("runtime_state");
      if (state.review || !state.running) break;
      await pause();
    }
    assert(state.review && state.running, "Run did not pause for review");
    await request("review", {
      run_id,
      review_id: state.review.id,
      decision: "apply",
    });
    for (let i = 0; i < 200; i++) {
      state = await request("runtime_state");
      if (!state.running) break;
      await pause();
    }
    assert(
      state.run?.outcome.kind === "ok",
      JSON.stringify(state.error || state.run?.outcome || "Run did not finish"),
    );
    const history = await request("history", { graph: graph.name });
    assert(history[0]?.id === state.run.id, "History was not persisted");
    const output = await request("output", {
      graph: graph.name,
      run_id: state.run.id,
      node: "n1",
    });
    assert(
      output.text.includes("Native Tauri execution"),
      "Output hydration failed",
    );
    await invoke("smoke_result", { error: null });
  } catch (error) {
    await invoke("smoke_result", { error: String(error?.stack || error) });
  }
})();
