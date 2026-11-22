import { useEffect, useRef, useState } from "react";

export type Choice = "cancel" | "confirm" | "alternative";

export const SHORTCUTS: [string, string][] = [
  ["⌘S", "Save the workflow"],
  ["⌘↵", "Run the workflow"],
  ["⌘⇧↵", "Run the workflow without cached outputs"],
  ["⌘N", "Add a node of the selected type"],
  ["⌘⇧N", "New workflow"],
  ["⌘Z · ⌘⇧Z", "Undo · redo"],
  ["⌘C · ⌘V · ⌘D", "Copy · paste · duplicate the selected nodes"],
  ["⌫", "Delete the selected nodes or wire"],
  ["Esc", "Clear the selection"],
  ["⇧ drag", "Select every node in a box"],
  ["⌘ click", "Add a node to the selection"],
  ["Double-click", "Add a node on the canvas · open a child workflow"],
  ["Right-click", "Actions for a node, wire or the canvas"],
  ["⌘B · ⌘J", "Show or hide the library · the inspector"],
  ["?", "This list"],
];

export function ConfirmDialog({
  title,
  message,
  confirm,
  alternative,
  input,
  onDecision,
}: {
  title: string;
  message: string;
  confirm: string;
  alternative?: string;
  input?: { label: string; initial: string };
  onDecision: (choice: Choice, value: string) => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [value, setValue] = useState(input?.initial ?? "");
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  return (
    <dialog
      ref={dialog}
      className="confirm-dialog"
      aria-labelledby="confirm-title"
      aria-describedby="confirm-message"
      onCancel={(event) => {
        event.preventDefault();
        onDecision("cancel", value);
      }}
    >
      <form
        onSubmit={(event) => {
          event.preventDefault();
          onDecision("confirm", value);
        }}
      >
        <h2 id="confirm-title">{title}</h2>
        <p id="confirm-message">{message}</p>
        {input && (
          <input
            aria-label={input.label}
            value={value}
            autoFocus
            onChange={(event) => setValue(event.target.value)}
          />
        )}
        <div>
          <button
            type="button"
            autoFocus={!input}
            onClick={() => onDecision("cancel", value)}
          >
            Cancel
          </button>
          <button
            type="submit"
            className={alternative ? "danger-button" : "primary"}
            disabled={!!input && !value.trim()}
          >
            {confirm}
          </button>
          {alternative && (
            <button
              type="button"
              className="primary"
              onClick={() => onDecision("alternative", value)}
            >
              {alternative}
            </button>
          )}
        </div>
      </form>
    </dialog>
  );
}

export function ShortcutsDialog({ onClose }: { onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  return (
    <dialog
      ref={dialog}
      className="confirm-dialog shortcuts-dialog"
      aria-labelledby="shortcuts-title"
      onCancel={(event) => {
        event.preventDefault();
        onClose();
      }}
    >
      <h2 id="shortcuts-title">Keyboard shortcuts</h2>
      <dl>
        {SHORTCUTS.map(([keys, what]) => (
          <div key={keys}>
            <dt>
              <kbd>{keys}</kbd>
            </dt>
            <dd>{what}</dd>
          </div>
        ))}
      </dl>
      <div>
        <button className="primary" autoFocus onClick={onClose}>
          Close
        </button>
      </div>
    </dialog>
  );
}
