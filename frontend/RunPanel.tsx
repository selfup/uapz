import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { request } from "./api";
import OutputActions from "./OutputActions";
import { feedNodeName, runFeed } from "./runFeed";
import type { GraphDoc, NodeRun, Run, RuntimeState } from "./types";
import type { FullOutput } from "./protocol";
import "./runPanel.css";

type Props = {
  graph: GraphDoc;
  runtime: RuntimeState | null;
  run: Run | null;
  /** Called with a node path such as `n1` or `n1/n4` to show it on the canvas. */
  onSelectNode?: (path: string) => void;
};
type OutputContext = { graph: string; runId: string; id: string };
const AUTOMATIC_OUTPUT_BYTES = 256 * 1024;

export default function RunPanel(props: Props) {
  const identity = props.runtime?.run_id || props.run?.id || "empty";
  return <RunFeedPanel key={identity} {...props} />;
}

function RunFeedPanel({
  graph,
  runtime,
  run: selectedRun,
  onSelectNode,
}: Props) {
  const run = runtime?.run || selectedRun;
  const root = runtime?.graph || graph;
  const entries = runFeed(runtime, run);
  const viewport = useRef<HTMLDivElement>(null);
  const content = useRef<HTMLDivElement>(null);
  const followRef = useRef(true);
  const userScrollUntil = useRef(0);
  const pointerGestures = useRef(new Set<number>());
  const touchGesture = useRef(false);
  const [following, setFollowing] = useState(true);
  const follow = () => {
    const element = viewport.current;
    if (followRef.current && element) element.scrollTop = element.scrollHeight;
  };
  useLayoutEffect(follow);
  useEffect(() => {
    const observer = new ResizeObserver(follow);
    if (viewport.current) observer.observe(viewport.current);
    if (content.current) observer.observe(content.current);
    return () => observer.disconnect();
  }, []);
  useEffect(() => {
    const endPointer = (event: PointerEvent) => {
      if (pointerGestures.current.delete(event.pointerId))
        userScrollUntil.current = Date.now() + 1000;
    };
    const endTouch = (event: TouchEvent) => {
      if (touchGesture.current) {
        touchGesture.current = event.touches.length > 0;
        userScrollUntil.current = Date.now() + 1000;
      }
    };
    window.addEventListener("pointerup", endPointer);
    window.addEventListener("pointercancel", endPointer);
    window.addEventListener("touchend", endTouch);
    window.addEventListener("touchcancel", endTouch);
    return () => {
      window.removeEventListener("pointerup", endPointer);
      window.removeEventListener("pointercancel", endPointer);
      window.removeEventListener("touchend", endTouch);
      window.removeEventListener("touchcancel", endTouch);
    };
  }, []);
  const status = runtime?.running
    ? runtime.review
      ? "Awaiting review"
      : runtime.cancelling
        ? "Cancelling"
        : "Running"
    : run?.outcome.kind === "ok"
      ? "Completed"
      : run?.outcome.kind === "failed" || runtime?.error
        ? "Failed"
        : run?.outcome.kind === "cancelled"
          ? "Cancelled"
          : "Ready";
  const error =
    runtime?.error ||
    (run?.outcome.kind === "failed" ? run.outcome.error : null);
  const workflows = { ...run?.workflows, ...runtime?.workflows };
  const context = {
    graph: runtime?.graph.name || run?.graph || graph.name,
    runId: runtime?.run_id || run?.id || "",
  };
  const markUserScroll = () => {
    userScrollUntil.current = Date.now() + 1000;
  };
  return (
    <section className="run-panel" aria-label="Workflow run output">
      <header className="run-panel-header">
        <div>
          <span className="run-panel-eyebrow">WORKFLOW OUTPUT</span>
          <h2>{root.name}</h2>
        </div>
        <span
          className={`run-status run-status-${status.toLowerCase().replaceAll(" ", "-")}`}
        >
          {status}
        </span>
      </header>
      <div className="run-panel-toolbar">
        <span>
          {runtime?.running
            ? "Live output from every node"
            : run
              ? `${run.elapsed_ms.toLocaleString()} ms · ${run.id}`
              : "All node output appears here"}
        </span>
        {!following && (
          <button
            type="button"
            onClick={() => {
              followRef.current = true;
              userScrollUntil.current = 0;
              setFollowing(true);
              follow();
            }}
          >
            Follow output
          </button>
        )}
      </div>
      <div
        ref={viewport}
        className="run-feed"
        aria-label="Run output feed"
        tabIndex={0}
        onWheel={markUserScroll}
        onTouchStart={() => {
          touchGesture.current = true;
          markUserScroll();
        }}
        onTouchMove={markUserScroll}
        onPointerDown={(event) => {
          pointerGestures.current.add(event.pointerId);
          markUserScroll();
        }}
        onPointerMove={(event) => {
          if (pointerGestures.current.has(event.pointerId)) markUserScroll();
        }}
        onKeyDown={(event) => {
          if (
            [
              "ArrowUp",
              "ArrowDown",
              "PageUp",
              "PageDown",
              "Home",
              "End",
              " ",
            ].includes(event.key)
          )
            markUserScroll();
        }}
        onScroll={() => {
          const element = viewport.current;
          if (
            !element ||
            (!pointerGestures.current.size &&
              !touchGesture.current &&
              Date.now() > userScrollUntil.current)
          )
            return;
          markUserScroll();
          const atBottom =
            element.scrollHeight - element.scrollTop - element.clientHeight <
            40;
          followRef.current = atBottom;
          setFollowing(atBottom);
        }}
      >
        <div ref={content} className="run-feed-content">
          {!!runtime?.events_dropped && (
            <p className="run-feed-note">
              {runtime.events_dropped.toLocaleString()} earlier feed entries
              were omitted. Completed outputs remain below when available.
            </p>
          )}
          {!runtime && !run ? (
            <div className="run-panel-empty">
              <h3>No run yet</h3>
              <p>
                Run this workflow to see progress, streamed text, and completed
                outputs together.
              </p>
            </div>
          ) : (
            <>
              {!entries.length && (
                <p className="run-feed-note">
                  {runtime?.running
                    ? "Waiting for workflow output…"
                    : "This run has no recorded node output."}
                </p>
              )}
              {entries.map((entry) =>
                entry.kind === "log" ? (
                  <pre className="run-log" key={entry.key}>
                    {entry.truncated &&
                      "[Earlier text in this log entry was omitted]\n"}
                    {entry.text}
                  </pre>
                ) : (
                  <article
                    className={`run-entry run-entry-${entry.kind}`}
                    key={entry.key}
                    data-node-path={entry.id}
                  >
                    <header>
                      <h3>
                        {onSelectNode ? (
                          <button
                            type="button"
                            className="run-entry-link"
                            title="Show this node on the canvas"
                            onClick={() => onSelectNode(entry.id)}
                          >
                            {feedNodeName(root, entry.id, workflows, run)}
                          </button>
                        ) : (
                          feedNodeName(root, entry.id, workflows, run)
                        )}
                      </h3>
                      <span>
                        {entry.kind === "node"
                          ? entry.state
                          : entry.kind === "text"
                            ? runtime?.running
                              ? "streaming"
                              : "partial output"
                            : entry.result.state}
                      </span>
                    </header>
                    {entry.kind === "text" && (
                      <>
                        {entry.truncated && (
                          <p className="run-feed-note">
                            Earlier streamed text was omitted.
                          </p>
                        )}
                        <pre className="run-output-text">{entry.text}</pre>
                      </>
                    )}
                    {entry.kind === "output" && (
                      <RunOutput
                        result={entry.result}
                        context={{ ...context, id: entry.id }}
                      />
                    )}
                  </article>
                ),
              )}
              {runtime?.review && (
                <p className="run-feed-note">
                  Execution is paused for file review. Review the proposed
                  changes to continue.
                </p>
              )}
              {error && (
                <p className="run-output-error" role="alert">
                  {error}
                </p>
              )}
              {run?.outcome.kind === "cancelled" && (
                <p className="run-feed-note">
                  Run cancelled. Output produced before cancellation is retained
                  above.
                </p>
              )}
            </>
          )}
        </div>
      </div>
    </section>
  );
}

function RunOutput({
  result,
  context,
  outputName,
}: {
  result: NodeRun;
  context: OutputContext;
  outputName?: string;
}) {
  const [attempt, setAttempt] = useState(0);
  const [loaded, setLoaded] = useState<{
    identity: string;
    requestIdentity: string;
    data: FullOutput;
  } | null>(null);
  const [fullChoice, setFullChoice] = useState<string | null>(null);
  const [failure, setFailure] = useState<{
    identity: string;
    error: string;
  } | null>(null);
  const [note, setNote] = useState<{ identity: string; text: string } | null>(
    null,
  );
  const retrieval = useRef<{
    identity: string;
    attempt: number;
    promise: Promise<FullOutput>;
  } | null>(null);
  const identity = JSON.stringify([
    context,
    outputName,
    result.key,
    result.preview,
    result.complete,
    result.error,
  ]);
  const needsFull =
    !result.error &&
    !!context.runId &&
    (!result.complete || result.preview === undefined);
  const unlimited = fullChoice === identity;
  const requestIdentity = JSON.stringify([identity, unlimited]);
  const full = loaded?.identity === identity ? loaded.data : null;
  const error = failure?.identity === requestIdentity ? failure.error : null;
  const loading =
    needsFull && loaded?.requestIdentity !== requestIdentity && !error;
  useEffect(() => {
    if (!needsFull) return;
    let active = true;
    const parts = context.id.split("/");
    const node = parts.pop() || "";
    setFailure(null);
    // Strict Mode replays effects; both subscribers share one retrieval.
    if (
      retrieval.current?.identity !== requestIdentity ||
      retrieval.current.attempt !== attempt
    )
      retrieval.current = {
        identity: requestIdentity,
        attempt,
        promise: request("output", {
          graph: context.graph,
          run_id: context.runId,
          node,
          ...(parts.length ? { call_path: parts.join("/") } : {}),
          ...(outputName ? { output: outputName } : {}),
          ...(!unlimited ? { max_bytes: AUTOMATIC_OUTPUT_BYTES } : {}),
        }),
      };
    retrieval.current.promise
      .then((data) => {
        if (active) setLoaded({ identity, requestIdentity, data });
      })
      .catch((cause: unknown) => {
        if (active)
          setFailure({
            identity: requestIdentity,
            error: cause instanceof Error ? cause.message : String(cause),
          });
      });
    return () => {
      active = false;
    };
  }, [requestIdentity, needsFull, attempt]);
  const text = full?.text ?? result.preview;
  const mediaType =
    full?.media_type ||
    (/jpe?g/i.test(full?.label || "")
      ? "image/jpeg"
      : /webp/i.test(full?.label || "")
        ? "image/webp"
        : "image/png");
  const image =
    full?.base64 &&
    mediaType.startsWith("image/") &&
    (full.media_type || /image|png|jpe?g|webp/i.test(full.label));
  return (
    <div className="run-output">
      {outputName && <h4>{outputName}</h4>}
      {result.error ? (
        <>
          {result.preview !== undefined && (
            <pre className="run-output-text">{result.preview}</pre>
          )}
          <p className="run-output-error">{result.error}</p>
        </>
      ) : (
        <>
          {result.label && <p className="run-output-label">{result.label}</p>}
          {image ? (
            <img
              className="run-output-image"
              src={`data:${mediaType};base64,${full?.base64}`}
              alt={`${outputName || "Node"} output`}
            />
          ) : text !== undefined && text !== null ? (
            <pre className="run-output-text">
              {text === "" ? "(Empty output)" : text}
            </pre>
          ) : (
            <p className="run-feed-note">
              {loading
                ? "Loading output…"
                : result.label
                  ? "Binary output"
                  : "No text output."}
            </p>
          )}
          {loading && text !== undefined && (
            <p className="run-feed-note">
              {unlimited ? "Loading full output…" : "Loading output…"}
            </p>
          )}
          {!full && !result.complete && result.preview !== undefined && (
            <p className="run-feed-note">Preview truncated.</p>
          )}
          {full?.limited && (
            <div className="run-feed-note">
              <p>
                {full.text !== null
                  ? `Showing first 256 KB of ${(full.bytes || 0).toLocaleString()} bytes.`
                  : `${(full.bytes || 0).toLocaleString()} bytes of binary output.`}
              </p>
              {!unlimited && (
                <button type="button" onClick={() => setFullChoice(identity)}>
                  Load full output
                </button>
              )}
            </div>
          )}
          {full && !full.complete && !full.limited && (
            <p className="run-feed-note">
              Only the saved preview is available. The full output is no longer
              in the cache.
            </p>
          )}
          {error && (
            <div className="run-output-error" role="alert">
              <p>Could not load full output: {error}</p>
              <button
                type="button"
                onClick={() => setAttempt((value) => value + 1)}
              >
                Retry output
              </button>
            </div>
          )}
          {full?.base64 && !image && (
            <a
              download={
                (full.saved || result.saved)?.split("/").pop() || "output.bin"
              }
              href={`data:${full.media_type || "application/octet-stream"};base64,${full.base64}`}
            >
              Download output
            </a>
          )}
          {(full?.saved || result.saved) && (
            <p className="run-output-saved">
              Saved to <code>{full?.saved || result.saved}</code>
            </p>
          )}
          <OutputActions
            text={typeof text === "string" && text !== "" ? text : null}
            saved={full?.saved || result.saved || null}
            note={note?.identity === identity ? note.text : null}
            onNote={(text) => setNote({ identity, text })}
          />
        </>
      )}
      {Object.entries(result.named_outputs || {}).map(([name, output]) => (
        <RunOutput
          key={name}
          outputName={name}
          result={output}
          context={context}
        />
      ))}
    </div>
  );
}
