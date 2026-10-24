# uapz

A small native node-graph editor built with [gpui-kit](https://gpui-kit.com). Nodes are
boxes you drag around and wire together; each one can carry typed content: an **agent**,
a **database**, a **script** or a **data** file. Graphs save as a Markdown file with a
Mermaid diagram plus a JSON sidecar, and reusable agents, prompts and databases save into
their own directories so they show up in the sidebar.

Nodes run from the inspector. A node runs after everything wired into it, text flows along
the wires, and every agent result is saved under that agent with a timestamp.

## Running

```sh
cargo check   # keep it warning-free
cargo run
```

Tests cover the file formats, the Mermaid parser, every node config's validation, the SQL
checker, path confinement, the library's on-disk layout, the executor (scripts, SQLite,
chains, timeouts) and the request shape for every provider:

```sh
cargo test
```

## Using it

The window has a sidebar on the left, a toolbar above the canvas, the canvas itself, and
an inspector on the right.

**Canvas**

- Drag a node to move it. Drag empty space to pan.
- Drag from a node's right-hand port and release on another node to connect them. The
  wire is dashed until it snaps to a target. Self-connections and duplicates are ignored.
- Click a node or a wire to select it. Backspace or Delete removes it. Deleting a node
  removes its wires.
- Double-click empty space, press cmd-n, or click **Add node** to add a node of whatever
  the toolbar dropdown has selected: **Blank**, one of the four types, or a saved kind.
- Escape cancels a drag and clears the selection.
- A typed node shows its label (its type, or the preset it came from) and a one-line
  summary such as `Ollama · llama3.1` or `SQLite · sales.sqlite`.

**Inspector**

- Select a node and its content appears as a form. Every edit writes straight back into
  the node; nothing is saved to disk until you save the graph or a preset.
- A blank node offers four buttons to give it content. A typed node can change type or go
  back to blank at the bottom of the panel.
- Problems are listed under the form as they happen (`model: model is empty`, `query:
  DELETE is not allowed in a read-only query`). A valid config says so.
- **Save agent** / **Save database** writes the config into the library under the name in
  the field next to it and labels the node with that name. Invalid configs are refused.
- In an agent, **Save prompt** writes the prompt text to `prompts/<name>.md`. Clicking a
  prompt in the sidebar loads it into the selected agent.
- **Run** executes the selected node after everything wired into it. The result, the log
  and a text preview appear under the form; the status line says where an agent's output
  was saved. See *Running nodes*.

**Sidebar**

Every section header collapses on click. Sections, top to bottom:

- **Graphs**: name, Save (Enter, cmd-s), New (cmd-shift-n), and the saved graphs. Click
  one to load it.
- **Kinds**: blank, colored node kinds. Name, color, **Save kind**. Click a row to make it
  current and load it into the creator; **+** adds one; **×** deletes it. Type names are
  reserved.
- **Agents** and **Databases**: saved presets. Click one to add a node made from it;
  **×** deletes the preset (an agent's outputs are kept).
- **Prompts**: click loads the prompt into the selected agent node; **×** deletes it.
- **Scripts** and **Data**: the files under `scripts/` and `data/`. Click one to point the
  selected script/data node at it, or to add a node for it when no such node is selected.
  **↻** re-reads the directories.

## Node types

**Agent**

| Field | Notes |
| --- | --- |
| Provider | Ollama, LM Studio, llama.cpp (local, no key); OpenRouter, Anthropic, OpenAI, Google, Mistral; or Custom, any OpenAI-compatible server |
| Endpoint | Base URL of the API. Empty uses the provider's default (`http://localhost:11434`, `http://localhost:1234/v1`, `http://localhost:8080/v1`, …); required for Custom. Plain `http://` only for localhost |
| Model type | Generative, Embedding or Image; decides which outputs are allowed |
| Model | Provider's model id, e.g. `llama3.1`, `claude-opus-5-5` |
| API key env var | Remote providers only. The *name* of the variable holding the key. The key itself is never written to disk. |
| Prompt | System prompt; save and reuse via `prompts/` |
| Context size | Tokens, 1 to 2,000,000 |
| Temperature | 0.0 to 2.0. Not sent to Anthropic, whose API version 2026-10-21 has no sampling parameters |
| Output | Text, JSON, Embedding or Image; must match the model type |

**Database**

| Field | Notes |
| --- | --- |
| Engine | SQLite, DuckDB (file-based), PostgreSQL, MySQL (server) |
| Database file | File-based engines: a path inside `data/` with a matching extension |
| Connection URL env var | Server engines: the *name* of the variable holding the URL, so credentials stay in the environment |
| Input data | Optional `.csv`, `.tsv`, `.json`, `.ndjson`, `.jsonl` or `.parquet` file inside `data/`, loaded into the input table before the query |
| Input table | Table name for the input; a plain identifier |
| Query | One statement. Read-only by default, see below |

**Script**

| Field | Notes |
| --- | --- |
| Runtime | Python (run through `uv`), Shell, Node; picked from the file extension |
| Script file | A path inside `scripts/` whose extension matches the runtime |
| Arguments | Space separated; at most 64, each printable and under 4096 bytes |
| Timeout | Seconds, 1 to 86,400 |

**Data**

| Field | Notes |
| --- | --- |
| File | A path inside `data/` |
| Format | CSV, TSV, JSON, Text, Markdown, Image, PDF, Parquet or Binary; picked from the extension and checked against it |

## Running nodes

Press **Run** in the inspector. The executor runs the selected node's upstream nodes first
(each once, in a stable order, refusing cycles), joins their text outputs with blank lines,
and hands that to the node:

| Node | What runs | Takes from the wire | Produces |
| --- | --- | --- | --- |
| Data | Reads the file (up to 64 MB) | nothing | the file; text for CSV, TSV, JSON, Text and Markdown, binary otherwise |
| Script | `uv run --quiet <file>`, `sh`/`bash`/`zsh <file>` or `node <file>` with the arguments, in `scripts/` | stdin | stdout as text; non-zero exit or the timeout is an error |
| Database | `sqlite3 -json`, `duckdb -json`, `psql --csv` or `mysql --batch` with the query on stdin | nothing (use the input file) | rows as JSON (file engines) or CSV/TSV (server engines) |
| Agent | One request to the provider | the user message | text, JSON, an embedding vector or an image, saved to `agents/<name>/outputs/` |

Agents send the prompt as the system message and the wire text as the user message; with
nothing on the wire the prompt is the user message. Prompt plus input must fit the context
size (estimated at four characters per token) or the run is refused before anything is
sent. JSON output is requested natively where the API has a flag (OpenAI-style
`response_format`, Ollama `format`, Google `responseMimeType`), by instruction on
Anthropic, and always validated and pretty-printed; fenced or chatty replies are unwrapped.

Provider notes: Ollama is called on its native API so the context size reaches the model
as `num_ctx`. Anthropic is called on API version 2026-10-21 with server-side refusal
fallbacks enabled for the models that support them (Claude Opus 5.5, Opus 5, Sonnet 5.5,
Fable 5.1). Google takes the key in a header, never in the URL. Embeddings work on every
provider but Anthropic; image generation only on OpenAI (and Custom).

Outputs are named `agents/<preset name or node id>/outputs/<UTC timestamp>.<txt|json|png>`.

Database command-line tools must be installed (`sqlite3` ships with macOS). Loading an
input file is supported for SQLite (CSV, imported into the `temp` schema so a read-only
database stays untouched) and DuckDB (CSV, TSV, JSON, Parquet, into a temporary table).
Server engines read the connection URL from the named environment variable and pass it to
the tool through its own environment variables (`PGHOST`, `PGPASSWORD`, `MYSQL_PWD`, …),
never on the command line. Read-only is enforced by the engine too: `-readonly` for SQLite
and DuckDB, `default_transaction_read_only` for PostgreSQL, `SET SESSION TRANSACTION READ
ONLY` for MySQL.

Child processes get a clean environment (`PATH`, `HOME`, `TMPDIR`, `LANG`, `LC_ALL`, `TERM`
only, plus the one secret a database tool needs), stdin for their input, and are killed at
the timeout (the script's own, five minutes for database tools and HTTP).

## Security

The configs only describe runs, but a future executor will act on them, so the rules are
enforced now, at edit time, and are meant to be re-checked at run time.

- **Names become file stems.** Anything typed as a name is reduced to letters, digits,
  `-`, `_` and spaces, so a graph, kind, agent, prompt or database name can never leave
  its directory or become a hidden file.
- **Paths are confined.** Every file a node refers to is a relative path inside one fixed
  directory (`data/` or `scripts/`). Absolute paths, `..`, `.`, hidden components,
  backslashes, drive prefixes and control characters are rejected. Sidebar listings skip
  hidden files and symlinks. `validate::resolve_inside` additionally follows symlinks and
  proves the real file is still under the real directory; executors call it before opening
  anything.
- **No secrets on disk.** Agents and server databases store the *name* of an environment
  variable, never a key or URL. Names must look like `[A-Za-z_][A-Za-z0-9_]*`.
- **SQL is checked before it reaches an engine.** Exactly one statement; MySQL `/*!`
  executable comments rejected; quoted identifiers scanned like bare ones (so
  `"pg_read_file"(...)` is caught); block comments end at the first `*/`, the conservative
  reading. Read-only queries must start with SELECT, WITH, EXPLAIN, SHOW, DESCRIBE or VALUES
  and may not contain writing or session keywords anywhere, including inside CTEs. Every
  query, read-only or not, is refused if it names something that reaches outside the
  database: ATTACH, LOAD, COPY, GRANT, `INTO OUTFILE`, `pg_read_file`, `load_extension`,
  DuckDB's `read_csv`/`read_parquet`/`glob`, `xp_cmdshell` and friends. Files get into a
  database through the validated input field, not through the query.
- **Scripts are files you placed there.** The app never writes to `scripts/`; a node can
  only point at an existing file with a runtime-matching extension and pass bounded
  arguments.
- **Saving is strict, drafting is not.** The graph file keeps whatever you typed, flagged
  in the inspector. The library refuses to save an invalid agent or database, and the
  canvas shows the first problem.
- **The executor trusts nothing it didn't check itself.** Every config is validated again
  before it runs. Every file is opened through `resolve_inside`, after symlinks. Secrets are
  read from the environment at the last moment and go only into the request header or the
  child's environment: never argv, never the log, never a file. Child processes get the
  minimal environment above and a hard timeout. Model ids are checked before they are
  spliced into a URL path. Replies are size-capped. Read-only queries are enforced twice:
  by the SQL checker and by the engine.

## Files

All directories are created next to the executable's working directory on first use. They
are plain text, so they are easy to commit, diff or hand-edit.

| Path | Contents |
| --- | --- |
| `graphs/<name>.md` | Human-readable graph: a Mermaid flowchart |
| `graphs/<name>.json` | Full editor state: positions, kinds, colors, typed node content |
| `nodes/<name>.json` | A blank kind: `{ "name": "Fetch", "color": "#ff8800" }` |
| `agents/<name>/agent.json` | A saved agent config |
| `agents/<name>/outputs/<timestamp>.<ext>` | One file per run, `YYYYMMDDTHHMMSSmmmZ` UTC, `.txt`/`.json`/`.png` by output type |
| `prompts/<name>.md` | A reusable prompt |
| `databases/<name>.json` | A saved database config |
| `scripts/` | Script files for script nodes (`.py`, `.sh`, `.js`, …) |
| `data/` | Data files and database files for data and database nodes |

### `graphs/<name>.md`

Node ids are stable, labeled nodes carry a `style` line with their color, and GitHub,
VS Code and most Markdown tools render the block.

````markdown
# pipeline

Layout and node data live in `pipeline.json`.

```mermaid
flowchart LR
    n0["summarizer"]
    n1[" "]
    n0 --> n1
    style n0 fill:#8b5cf6,stroke:#6845b9,color:#ffffff
```
````

### `graphs/<name>.json`

One object per node with its position, an optional `kind` (the saved kind or preset it was
made from), a `color` copied at save time, and `data`: `{}` for a blank node, or a typed
object tagged with `"type"`.

```json
{
  "version": 1,
  "name": "pipeline",
  "view": { "pan_x": 0.0, "pan_y": 0.0 },
  "nodes": [
    {
      "id": "n0", "x": 160.0, "y": 220.0, "kind": "summarizer", "color": "#8b5cf6",
      "data": {
        "type": "agent",
        "provider": "anthropic",
        "model_type": "generative",
        "model": "claude-opus-5-5",
        "auth": { "kind": "env", "var": "ANTHROPIC_API_KEY" },
        "prompt": "Summarize the input in three bullets.",
        "context_size": 8192,
        "temperature": 0.3,
        "output": "text"
      }
    },
    { "id": "n1", "x": 460.0, "y": 330.0, "data": {} }
  ],
  "edges": [{ "from": "n0", "to": "n1" }]
}
```

Loading prefers the JSON. If only the `.md` exists, the Mermaid block is parsed for node
ids and `-->` edges (node shapes and `|labels|` are tolerated) and laid out left to right.

## Shortcuts

| Keys | Action |
| --- | --- |
| cmd-s, ctrl-s | Save graph |
| cmd-shift-n | New graph |
| cmd-n, ctrl-n | Add node of the selected kind or type |
| Backspace, Delete | Delete selection |
| Escape | Cancel drag, clear selection |
| cmd-q | Quit |

## Layout

| File | Role |
| --- | --- |
| `src/main.rs` | App setup, key bindings, window |
| `src/sidebar.rs` | Workspace: sidebar sections, toolbar, layout |
| `src/inspector.rs` | The node content editor on the right, with Run |
| `src/exec.rs` | The executor: upstream ordering, files, child processes, database tools, agent runs |
| `src/providers.rs` | Provider request builders and reply parsers; the HTTP client |
| `src/graph.rs` | The editor view: nodes, wires, dragging, selection |
| `src/node_data.rs` | The typed node content: configs, enums, summaries, validation |
| `src/validate.rs` | Sanitizers and validators: names, paths, env vars, identifiers, SQL |
| `src/library.rs` | The per-type directories: agents and outputs, prompts, databases, file listings |
| `src/doc.rs` | Graph file formats, Mermaid writer and parser, kind library |

Dependencies are `gpui-kit` (GPUI plus the component library), `serde` and `serde_json`,
`ureq` for HTTP, `url` for connection and endpoint URLs, and `base64` for image replies.
