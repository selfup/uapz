import { X } from "lucide-react";
import ComboInput from "./ComboInput";
import type { RunForm } from "./types";

export default function RunInputForm({
  form,
  busy,
  dataChoices,
  onClose,
  onChange,
  onSubmit,
}: {
  form: RunForm;
  busy: boolean;
  dataChoices: string[];
  onClose: () => void;
  onChange: (form: RunForm) => void;
  onSubmit: (form: RunForm) => void;
}) {
  return (
    <section className="run-form" aria-label="Run inputs">
      <div className="section-title">
        <strong>Run inputs</strong>
        <button
          className="icon-button"
          aria-label="Close run inputs"
          onClick={onClose}
        >
          <X size={15} />
        </button>
      </div>
      <p>Values for this execution</p>
      <form
        onSubmit={(event) => {
          event.preventDefault();
          onSubmit(form);
        }}
      >
        {form.fields.map((field) => (
          <label key={field.name}>
            {field.name}
            <small>
              {field.config.kind}
              {field.config.required ? " · required" : " · optional"}
            </small>
            {field.config.kind === "boolean" ? (
              <select
                aria-label={`Run input ${field.name}`}
                value={form.inputs[field.name] || ""}
                onChange={(event) =>
                  onChange({
                    ...form,
                    inputs: {
                      ...form.inputs,
                      [field.name]: event.target.value,
                    },
                  })
                }
              >
                <option value="">Choose…</option>
                <option value="true">true</option>
                <option value="false">false</option>
              </select>
            ) : field.config.kind === "file" ? (
              <ComboInput
                id={`run-input-${field.name}`}
                label={`Run input ${field.name}`}
                value={form.inputs[field.name] || ""}
                choices={dataChoices}
                placeholder="Choose or type a path inside data/"
                onChange={(value) =>
                  onChange({
                    ...form,
                    inputs: { ...form.inputs, [field.name]: value },
                  })
                }
              />
            ) : (
              <textarea
                aria-label={`Run input ${field.name}`}
                value={form.inputs[field.name] || ""}
                rows={
                  field.config.kind === "text" || field.config.kind === "json"
                    ? 3
                    : 1
                }
                placeholder={field.config.default}
                onChange={(event) =>
                  onChange({
                    ...form,
                    inputs: {
                      ...form.inputs,
                      [field.name]: event.target.value,
                    },
                  })
                }
              />
            )}
          </label>
        ))}
        {form.error && (
          <p className="field-error" role="alert">
            {form.error}
          </p>
        )}
        <button className="primary" type="submit" disabled={busy}>
          {busy ? "Starting…" : "Start run"}
        </button>
      </form>
    </section>
  );
}
