import { request } from "./api";
import { copyText, revealLabel } from "./clipboard";

/** Copy an output's text or show its saved file, with a short confirmation. */
export default function OutputActions({
  text,
  saved,
  note,
  onNote,
}: {
  text: string | null;
  saved: string | null;
  note: string | null;
  onNote: (text: string) => void;
}) {
  if (!text && !saved) return null;
  return (
    <div className="run-output-actions">
      {text && (
        <button
          type="button"
          onClick={() =>
            void copyText(text).then((copied) =>
              onNote(copied ? "Copied" : "Could not copy"),
            )
          }
        >
          Copy
        </button>
      )}
      {saved && (
        <button
          type="button"
          onClick={() =>
            void request("reveal", { path: saved })
              .then(() => onNote("Shown"))
              .catch((error: unknown) =>
                onNote(error instanceof Error ? error.message : String(error)),
              )
          }
        >
          {revealLabel()}
        </button>
      )}
      {note && (
        <span role="status" className="run-output-note">
          {note}
        </span>
      )}
    </div>
  );
}
