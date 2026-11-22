import { useEffect, useState } from "react";
import type { RuntimeState } from "./types";

export default function ReviewPanel({
  runtime,
  deciding,
  onDecision,
  onCancel,
}: {
  runtime: RuntimeState;
  /** A decision is on its way to the service; the buttons wait for its answer. */
  deciding: boolean;
  onDecision: (decision: "apply" | "reject") => void;
  onCancel: () => void;
}) {
  const [fileIndex, setFileIndex] = useState(0);
  const review = runtime.review!;
  useEffect(() => setFileIndex(0), [review.id]);
  const index = Math.max(0, Math.min(fileIndex, review.files.length - 1));
  const file = review.files[index];
  return (
    <section className="review-panel" aria-label="Review file changes">
      <div className="section-title">
        <strong>Review changes</strong>
        <span>
          {review.files.length} file{review.files.length === 1 ? "" : "s"}
        </span>
      </div>
      <p>{review.node}</p>
      {file && (
        <>
          <div className="review-files">
            <button
              type="button"
              aria-label="Previous file"
              disabled={index === 0}
              onClick={() => setFileIndex(index - 1)}
            >
              ‹
            </button>
            <select
              aria-label="Review file"
              value={index}
              onChange={(event) => setFileIndex(Number(event.target.value))}
            >
              {review.files.map((entry, position) => (
                <option value={position} key={entry.path}>
                  {entry.path}
                </option>
              ))}
            </select>
            <button
              type="button"
              aria-label="Next file"
              disabled={index >= review.files.length - 1}
              onClick={() => setFileIndex(index + 1)}
            >
              ›
            </button>
          </div>
          <p>
            File {index + 1} of {review.files.length} ·{" "}
            {file.before_bytes ?? "New file"} → {file.after_bytes} bytes
          </p>
          <pre className="review-diff">
            {file.diff.split("\n").map((line, lineIndex) => (
              <span
                key={lineIndex}
                className={
                  line.startsWith("+")
                    ? "diff-add"
                    : line.startsWith("-")
                      ? "diff-remove"
                      : ""
                }
              >
                {line}
                {"\n"}
              </span>
            ))}
          </pre>
        </>
      )}
      <div className="review-actions">
        <button
          className="primary"
          disabled={runtime.cancelling || deciding}
          onClick={() => onDecision("apply")}
        >
          Apply changes
        </button>
        <button
          disabled={runtime.cancelling || deciding}
          onClick={() => onDecision("reject")}
        >
          Reject
        </button>
        <button disabled={runtime.cancelling} onClick={onCancel}>
          Cancel run
        </button>
      </div>
    </section>
  );
}
