import type { ReactNode } from "react";
import {
  ArrowUpRight,
  FolderOpen,
  Plus,
  RefreshCw,
  Search,
  Trash2,
  X,
} from "lucide-react";
import { LIBRARY_DRAG_TYPE } from "./Canvas";
import type { LibraryItem } from "./library";

export type LibraryRowAction = {
  label: string;
  icon: ReactNode;
  run: () => void;
};

export type LibraryRowProps = {
  label: string;
  onClick: () => void;
  color?: string;
  onDelete?: () => void;
  disabled?: boolean;
  trailing?: "add" | "open";
  drag?: LibraryItem;
  actions?: LibraryRowAction[];
  // Workflow open stays available during an in-flight save; rename/delete/
  // duplicate must wait so the landing write cannot resurrect the old name.
  mutateDisabled?: boolean;
};

export function LibraryRow({
  label,
  onClick,
  color,
  onDelete,
  disabled = false,
  trailing = "add",
  drag,
  actions,
  mutateDisabled = disabled,
}: LibraryRowProps) {
  return (
    <div className="library-row">
      <button
        className="library-item"
        onClick={onClick}
        disabled={disabled}
        title={drag ? `${label} · drag onto the canvas` : label}
        draggable={!!drag && !disabled}
        onDragStart={(event) => {
          if (!drag) return;
          event.dataTransfer.setData(LIBRARY_DRAG_TYPE, JSON.stringify(drag));
          event.dataTransfer.effectAllowed = "copy";
        }}
      >
        {color ? <i style={{ background: color }} /> : <FolderOpen size={15} />}
        <span>{label}</span>
        {trailing === "open" ? (
          <ArrowUpRight size={12} className="row-action" />
        ) : (
          <Plus size={12} className="row-action" />
        )}
      </button>
      {(onDelete || actions?.length) && (
        <span className="row-actions">
          {actions?.map((item) => (
            <button
              key={item.label}
              className="icon-button"
              aria-label={item.label}
              title={item.label}
              onClick={item.run}
              disabled={mutateDisabled}
            >
              {item.icon}
            </button>
          ))}
          {onDelete && (
            <button
              className="icon-button delete-library"
              aria-label={`Delete ${label}`}
              title={`Delete ${label}`}
              onClick={onDelete}
              disabled={mutateDisabled}
            >
              <Trash2 size={13} />
            </button>
          )}
        </span>
      )}
    </div>
  );
}

export type LibraryGroup = {
  label: string;
  items: ReactNode[];
  open?: boolean;
};

export default function LibraryPanel({
  hidden,
  query,
  onQueryChange,
  readOnly,
  connecting,
  groups,
  workspace,
  kindEditor,
  kindName,
  kindColor,
  onRefresh,
  onInstallExamples,
  onToggleKindEditor,
  onKindNameChange,
  onKindColorChange,
  onSaveKind,
}: {
  hidden: boolean;
  query: string;
  onQueryChange: (query: string) => void;
  readOnly: boolean;
  connecting: boolean;
  groups: LibraryGroup[];
  workspace?: string;
  kindEditor: boolean;
  kindName: string;
  kindColor: string;
  onRefresh: () => void;
  onInstallExamples: () => void;
  onToggleKindEditor: () => void;
  onKindNameChange: (name: string) => void;
  onKindColorChange: (color: string) => void;
  onSaveKind: () => void;
}) {
  return (
    <aside className="library" hidden={hidden}>
      <div className="library-heading">
        <strong>Library</strong>
        <button
          className="icon-button"
          aria-label="Refresh library"
          onClick={onRefresh}
        >
          <RefreshCw size={15} />
        </button>
      </div>
      <label className="search">
        <Search size={15} />
        <input
          aria-label="Search library"
          placeholder="Search library…"
          value={query}
          onChange={(event) => onQueryChange(event.target.value)}
        />
        {query && (
          <button aria-label="Clear search" onClick={() => onQueryChange("")}>
            <X size={13} />
          </button>
        )}
      </label>
      {readOnly && (
        <p className="library-hint">Return to the parent to add nodes.</p>
      )}
      <div className="library-sections">
        {connecting && <p className="muted">Connecting to your workspace…</p>}
        {groups
          .filter((group) => !query || group.items.length)
          .map((group) => (
            <details
              key={`${group.label}:${!!query}`}
              open={!!query || group.open}
            >
              <summary>
                {group.label}
                <span>{group.items.length}</span>
              </summary>
              {group.items.length ? (
                group.items
              ) : (
                <div className="library-empty">
                  <p>No {group.label.toLowerCase()} saved yet.</p>
                  {group.label === "Workflows" && !readOnly && (
                    <button type="button" onClick={onInstallExamples}>
                      Install example workflows
                    </button>
                  )}
                </div>
              )}
            </details>
          ))}
        {query && groups.every((group) => !group.items.length) && (
          <p className="library-empty">No matching library items.</p>
        )}
      </div>
      <button
        className="kind-button"
        onClick={onToggleKindEditor}
        disabled={readOnly}
      >
        <Plus size={14} />
        Custom node kind
      </button>
      {kindEditor && (
        <form
          className="kind-form"
          onSubmit={(event) => {
            event.preventDefault();
            onSaveKind();
          }}
        >
          <input
            aria-label="Kind name"
            placeholder="Kind name"
            value={kindName}
            onChange={(event) => onKindNameChange(event.target.value)}
            required
          />
          <input
            aria-label="Kind color"
            type="color"
            value={kindColor}
            onChange={(event) => onKindColorChange(event.target.value)}
          />
          <button type="submit">Save kind</button>
        </form>
      )}
      <div className="workspace-path" title={workspace}>
        <FolderOpen size={13} />
        <span>{workspace || "Local workspace"}</span>
      </div>
    </aside>
  );
}
