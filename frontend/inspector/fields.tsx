import { useEffect, useId, useState, type ReactNode } from "react";
import { request } from "../api";
import ComboInput from "../ComboInput";

export function FieldControl({
  label,
  help,
  children,
}: {
  label: string;
  help?: string;
  children: (id: string) => ReactNode;
}) {
  const id = useId();
  return (
    <div className="ins-field">
      <label htmlFor={id}>{label}</label>
      {children(id)}
      {help && <p className="ins-help">{help}</p>}
    </div>
  );
}
export function Text({
  label,
  value,
  onChange,
  help,
  placeholder,
  multiline = false,
  mono = false,
  rows = 4,
  choices,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  help?: string;
  placeholder?: string;
  multiline?: boolean;
  mono?: boolean;
  rows?: number;
  choices?: string[];
}) {
  return (
    <FieldControl label={label} help={help}>
      {(id) =>
        multiline ? (
          <textarea
            id={id}
            className={mono ? "ins-mono" : ""}
            value={value}
            onChange={(event) => onChange(event.target.value)}
            placeholder={placeholder}
            rows={rows}
            spellCheck={!mono}
          />
        ) : choices?.length ? (
          <ComboInput
            id={id}
            value={value}
            onChange={onChange}
            choices={choices}
            placeholder={placeholder}
            mono={mono}
          />
        ) : (
          <input
            id={id}
            className={mono ? "ins-mono" : ""}
            value={value}
            onChange={(event) => onChange(event.target.value)}
            placeholder={placeholder}
            autoComplete="off"
            spellCheck={!mono}
          />
        )
      }
    </FieldControl>
  );
}
export function Select<T extends string>({
  label,
  value,
  onChange,
  options,
  help,
}: {
  label: string;
  value: NoInfer<T>;
  onChange: (value: T) => void;
  options: readonly (readonly [T, string])[];
  help?: string;
}) {
  return (
    <FieldControl label={label} help={help}>
      {(id) => (
        <select
          id={id}
          value={value}
          onChange={(event) => {
            const selected = options.find(
              ([option]) => option === event.currentTarget.value,
            );
            if (selected) onChange(selected[0]);
          }}
        >
          {!options.some((option) => option[0] === value) && (
            <option value={value}>{value || "Choose…"}</option>
          )}
          {options.map(([key, text]) => (
            <option key={key} value={key}>
              {text}
            </option>
          ))}
        </select>
      )}
    </FieldControl>
  );
}
export function Toggle({
  label,
  value,
  onChange,
  help,
}: {
  label: string;
  value: boolean;
  onChange: (value: boolean) => void;
  help?: string;
}) {
  return (
    <div className="ins-toggle-field">
      <label className="ins-toggle">
        <input
          type="checkbox"
          checked={value}
          onChange={(event) => onChange(event.target.checked)}
        />
        <span>{label}</span>
      </label>
      {help && <p className="ins-help">{help}</p>}
    </div>
  );
}
export function NumberField({
  label,
  value,
  onChange,
  min = 0,
  step = 1,
  help,
}: {
  label: string;
  value: number;
  onChange: (value: number) => void;
  min?: number;
  step?: number;
  help?: string;
}) {
  const [draft, setDraft] = useState(String(value));
  useEffect(() => setDraft(String(value)), [value]);
  return (
    <FieldControl label={label} help={help}>
      {(id) => (
        <input
          id={id}
          type="number"
          value={draft}
          min={min}
          step={step}
          onBlur={() => setDraft(String(value))}
          onChange={(event) => {
            const text = event.target.value;
            setDraft(text);
            if (text !== "" && Number.isFinite(Number(text)))
              onChange(Number(text));
          }}
        />
      )}
    </FieldControl>
  );
}
export function Lines({
  label,
  value,
  onChange,
  help,
}: {
  label: string;
  value: string[];
  onChange: (value: string[]) => void;
  help?: string;
}) {
  const [draft, setDraft] = useState(value.join("\n"));
  const encoded = JSON.stringify(value);
  useEffect(() => {
    setDraft((previous) =>
      JSON.stringify(previous.split("\n").filter(Boolean)) === encoded
        ? previous
        : value.join("\n"),
    );
  }, [encoded]); // Keep an unfinished last line while typing.
  return (
    <Text
      label={label}
      multiline
      rows={3}
      mono
      value={draft}
      help={help || "One item per line."}
      onChange={(text) => {
        setDraft(text);
        onChange(text.split("\n").filter(Boolean));
      }}
    />
  );
}

/** Says whether an environment variable is set for this process, never its value. */
export function EnvStatus({ name }: { name: string }) {
  const [state, setState] = useState<{ name: string; set: boolean } | null>(
    null,
  );
  const trimmed = name.trim();
  useEffect(() => {
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(trimmed)) {
      setState(null);
      return;
    }
    let active = true;
    const timer = window.setTimeout(() => {
      request("env_var_set", { name: trimmed })
        .then((set) => {
          if (active) setState({ name: trimmed, set });
        })
        .catch(() => {
          if (active) setState(null);
        });
    }, 250);
    return () => {
      active = false;
      window.clearTimeout(timer);
    };
  }, [trimmed]);
  if (!state || state.name !== trimmed) return null;
  return (
    <p
      className={`ins-help ins-env ${state.set ? "is-set" : "is-unset"}`}
      role="status"
    >
      {state.set
        ? `${state.name} is set in uapz's environment.`
        : `${state.name} is not set in uapz's environment. Start uapz from a shell that exports it, or export it in your login profile.`}
    </p>
  );
}

