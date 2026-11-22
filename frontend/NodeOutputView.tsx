import { useEffect, useRef, useState } from "react";
import { request } from "./api";
import OutputActions from "./OutputActions";
import type { FullOutput } from "./protocol";
import type { NodeRun } from "./types";
import { errorText, isImageOutput, outputMediaType } from "./outputMedia";

export type NodeOutputContext = {
  graph: string;
  runId: string;
  node: string;
  callPath?: string;
};

export const AUTOMATIC_OUTPUT_BYTES = 256 * 1024;

type Variant = "inspector" | "run";

const STYLES = {
  inspector: {
    root: "ins-result",
    heading: "ins-section-heading",
    state: (state: string) => `ins-result-state ins-state-${state}`,
    error: "ins-error",
    help: "ins-help",
    label: "ins-help",
    image: "ins-image-output",
    text: "ins-output",
    download: "ins-download",
    saved: "ins-saved",
  },
  run: {
    root: "run-output",
    heading: null,
    state: null,
    error: "run-output-error",
    help: "run-feed-note",
    label: "run-output-label",
    image: "run-output-image",
    text: "run-output-text",
    download: null,
    saved: "run-output-saved",
  },
} as const;

type Props = {
  result: NodeRun;
  context?: NodeOutputContext;
  outputName?: string;
  title?: string;
  variant: Variant;
  loadMode: "manual" | "auto";
};

export default function NodeOutputView({
  result,
  context,
  outputName,
  title = "Node result",
  variant,
  loadMode,
}: Props) {
  const css = STYLES[variant];
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
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<{ identity: string; text: string } | null>(
    null,
  );
  const retrieval = useRef<{
    identity: string;
    attempt: number;
    promise: Promise<FullOutput>;
  } | null>(null);
  const manualRequest = useRef(0);
  const ctxGraph = context?.graph;
  const ctxRunId = context?.runId;
  const ctxCallPath = context?.callPath;
  const ctxNode = context?.node;
  const identity = JSON.stringify([
    ctxGraph,
    ctxRunId,
    ctxCallPath,
    ctxNode,
    outputName,
    result.key,
    result.preview,
    result.label,
    result.error,
    result.complete,
  ]);
  const identityRef = useRef(identity);
  identityRef.current = identity;
  const needsFull =
    loadMode === "auto" &&
    !result.error &&
    !!ctxRunId &&
    (!result.complete || result.preview === undefined);
  const unlimited = fullChoice === identity;
  const requestIdentity = JSON.stringify([identity, unlimited]);
  const full = loaded?.identity === identity ? loaded.data : null;
  const error =
    loadMode === "auto"
      ? failure?.identity === requestIdentity
        ? failure.error
        : null
      : failure?.identity === identity
        ? failure.error
        : null;
  const loading =
    needsFull && loaded?.requestIdentity !== requestIdentity && !error;

  useEffect(() => {
    if (loadMode !== "manual") return;
    manualRequest.current += 1;
    setLoaded(null);
    setBusy(false);
    setFailure(null);
    setNote(null);
  }, [identity, loadMode]);

  useEffect(() => {
    if (loadMode !== "auto" || !needsFull || !ctxGraph || !ctxRunId || !ctxNode)
      return;
    let active = true;
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
          graph: ctxGraph,
          run_id: ctxRunId,
          node: ctxNode,
          ...(ctxCallPath ? { call_path: ctxCallPath } : {}),
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
            error: errorText(cause),
          });
      });
    return () => {
      active = false;
    };
  }, [
    requestIdentity,
    needsFull,
    attempt,
    loadMode,
    ctxGraph,
    ctxRunId,
    ctxNode,
    ctxCallPath,
    outputName,
    identity,
    unlimited,
  ]);

  async function loadManual() {
    if (!ctxGraph || !ctxRunId || !ctxNode) return;
    const requestId = ++manualRequest.current;
    const isCurrent = () =>
      manualRequest.current === requestId && identityRef.current === identity;
    setBusy(true);
    setFailure(null);
    try {
      const data = await request("output", {
        graph: ctxGraph,
        run_id: ctxRunId,
        node: ctxNode,
        ...(ctxCallPath ? { call_path: ctxCallPath } : {}),
        ...(outputName ? { output: outputName } : {}),
      });
      if (isCurrent())
        setLoaded({ identity, requestIdentity: identity, data });
    } catch (cause) {
      if (isCurrent()) setFailure({ identity, error: errorText(cause) });
    } finally {
      if (isCurrent()) setBusy(false);
    }
  }

  const text = full?.text ?? result.preview;
  const mediaType = outputMediaType(full);
  const image = isImageOutput(full, mediaType);
  const noteText = note?.identity === identity ? note.text : null;
  const actionText =
    typeof text === "string" && (variant === "run" ? text !== "" : !!text)
      ? text
      : null;
  const actionSaved =
    variant === "run"
      ? full?.saved || result.saved || null
      : result.saved || full?.saved || null;

  return (
    <section className={css.root}>
      {variant === "inspector" ? (
        <div className={css.heading!}>
          <h3>{title}</h3>
          <span className={css.state!(result.state)}>{result.state}</span>
        </div>
      ) : (
        outputName && <h4>{outputName}</h4>
      )}
      {result.error ? (
        variant === "run" ? (
          <>
            {result.preview !== undefined && (
              <pre className={css.text}>{result.preview}</pre>
            )}
            <p className={css.error}>{result.error}</p>
          </>
        ) : (
          <p className={css.error}>{result.error}</p>
        )
      ) : (
        <>
          {result.label && <p className={css.label}>{result.label}</p>}
          {image ? (
            <img
              className={css.image}
              src={`data:${mediaType};base64,${full?.base64}`}
              alt={`${outputName || title} ${variant === "run" ? "output" : "generated image"}`}
            />
          ) : variant === "run" ? (
            text !== undefined && text !== null ? (
              <pre className={css.text}>
                {text === "" ? "(Empty output)" : text}
              </pre>
            ) : (
              <p className={css.help}>
                {loading
                  ? "Loading output…"
                  : result.label
                    ? "Binary output"
                    : "No text output."}
              </p>
            )
          ) : text ? (
            <pre className={css.text}>{text}</pre>
          ) : (
            <p className={css.help}>
              {result.label ? "Binary output" : "No text output."}
            </p>
          )}
          {variant === "run" && loading && text !== undefined && (
            <p className={css.help}>
              {unlimited ? "Loading full output…" : "Loading output…"}
            </p>
          )}
          {variant === "run" &&
            !full &&
            !result.complete &&
            result.preview !== undefined && (
              <p className={css.help}>Preview truncated.</p>
            )}
          {variant === "inspector" && !full && text && !result.complete && (
            <p className={css.help}>Preview truncated.</p>
          )}
          {full?.limited && (
            <div className={css.help}>
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
            <p className={css.help}>
              {variant === "run"
                ? "Only the saved preview is available. The full output is no longer in the cache."
                : "Only a saved preview is available. The full output is no longer in the cache."}
            </p>
          )}
          {loadMode === "manual" && ctxNode && !result.complete && !full && (
            <button
              type="button"
              disabled={busy}
              onClick={() => void loadManual()}
            >
              {busy ? "Loading…" : "View full output"}
            </button>
          )}
          {error &&
            (variant === "run" ? (
              <div className={css.error} role="alert">
                <p>Could not load full output: {error}</p>
                <button
                  type="button"
                  onClick={() => setAttempt((value) => value + 1)}
                >
                  Retry output
                </button>
              </div>
            ) : (
              <p className={css.error} role="alert">
                {error}
              </p>
            ))}
          {full?.base64 && !image && (
            <a
              className={css.download || undefined}
              download={
                (full.saved || result.saved)?.split("/").pop() || "output.bin"
              }
              href={`data:${full.media_type || "application/octet-stream"};base64,${full.base64}`}
            >
              Download output
            </a>
          )}
          {variant === "inspector" && result.saved && (
            <p className={css.saved}>
              <span>Saved to</span>
              <code>{result.saved}</code>
            </p>
          )}
          {variant === "run" && (full?.saved || result.saved) && (
            <p className={css.saved}>
              Saved to <code>{full?.saved || result.saved}</code>
            </p>
          )}
          <OutputActions
            text={actionText}
            saved={actionSaved}
            note={noteText}
            onNote={(value) => setNote({ identity, text: value })}
          />
        </>
      )}
      {Object.entries(result.named_outputs || {}).map(([name, output]) => (
        <NodeOutputView
          key={name}
          title={name}
          outputName={name}
          result={output}
          context={context}
          variant={variant}
          loadMode={loadMode}
        />
      ))}
    </section>
  );
}
