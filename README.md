# uapz

A small native node-graph editor built with [gpui-kit](https://gpui-kit.com). Nodes are
boxes you drag around and wire together; each one can carry typed content: an **agent**,
a **database**, a **script**, a **data** file, a **directory** or a **file to write**.
Graphs save as a Markdown file with a Mermaid diagram plus a JSON sidecar, and reusable
agents, prompts and databases save into their own directories so they show up in the
sidebar.

Nodes run from the inspector. A node runs after everything wired into it, text flows along
the wires, agent answers stream into the panel as they are generated, and every agent
result is saved under that agent with a timestamp. Outputs are cached between runs, so
running a downstream node again does not re-call the agents above it unless something they
depend on changed.

## Running

```sh
cargo check   # keep it warning-free
cargo run
```

Tests cover the file formats, the Mermaid parser, every node config's validation, the SQL
checker, path confinement, the library's on-disk layout, the executor (scripts, SQLite,
chains, timeouts, the cache, cancellation, write-file nodes), streamed replies in every
provider's shape and the request shape for every provider:

```sh
cargo test
```

Live tests drive the executor against a real OpenAI-compatible server and a PostgreSQL
server; they are ignored by default. With a chat model and an embedding model loaded in LM
Studio (or any server named by `UAPZ_LIVE_ENDPOINT`) and a local PostgreSQL:

```sh
UAPZ_LIVE_MODEL=mistralai/ministral-3-3b \
UAPZ_LIVE_EMBED_MODEL=text-embedding-nomic-embed-text-v1.5 \
UAPZ_LIVE_PG_URL=postgres://localhost/postgres \
cargo test live_ -- --ignored --nocapture
```

## Using it

The window has a sidebar on the left, a toolbar above the canvas, the canvas itself, and
an inspector on the right. Drag the divider beside either side panel to resize it.

**Canvas**

- Drag a node to move it. Drag empty space to pan.
- Drag from a node's right-hand port and release on another node to connect them. The
  wire is dashed until it snaps to a target. Self-connections and duplicates are ignored.
- Click a node or a wire to select it. Backspace or Delete removes it. Deleting a node
  removes its wires.
- Double-click empty space, press cmd-n, or click **Add node** to add a node of whatever
  the toolbar dropdown has selected: **Blank**, one of the six types, **Database
  (write)**, or a saved kind.
- Escape cancels a drag and clears the selection.
- A typed node shows its label (its type, or the preset it came from) and a one-line
  summary such as `Ollama · llama3.1` or `SQLite · sales.sqlite`. Red nodes write: a
  database node whose read-only switch is off (its summary says `· write`) and a **Write
  file** node.

**Inspector**

- Select a node and its content appears as a form. Every edit writes straight back into
  the node; nothing is saved to disk until you save the graph or a preset.
- A blank node offers buttons to give it content: the six types plus **Database (write)**,
  a database node that starts with its read-only switch off. A typed node can change type
  or go back to blank at the bottom of the panel.
- Problems are listed under the form as they happen (`model: model is empty`, `query:
  DELETE is not allowed in a read-only query`). A valid config says so.
- **Save agent** / **Save database** writes the config into the library under the name in
  the field next to it and labels the node with that name. Invalid configs are refused.
- In an agent, **Save prompt** writes the prompt text to `prompts/<name>.md`. Clicking a
  prompt in the sidebar loads it into the selected agent.
- **Run** executes the selected node after everything wired into it. While it runs, the
  log grows line by line and an agent's answer streams into a preview as it is generated;
  **Cancel** stops it. The result, the log and a text preview appear under the form; the
  status line says where an agent's output was saved and how many nodes came from the
  cache. **Run fresh** ignores the cache. See *Running nodes*.

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
| Query | One statement. Read-only by default, see below. Empty means the SQL arrives on the wire, typically from an agent |
| Write the query with an agent | Pick a saved generative agent, describe what the query should do, press **Write query**. The agent gets the engine, the schema (`.schema` for SQLite and DuckDB, `information_schema` for the servers, plus the input file's columns) and your request; its answer lands in the query field, where the usual checks run on it |

**Script**

| Field | Notes |
| --- | --- |
| Runtime | Python (run through `uv`), Shell, Node; picked from the file extension |
| Script file | A path inside `scripts/` whose extension matches the runtime |
| Arguments | Split like a shell: spaces separate, `"double"` or `'single'` quotes keep words together, a backslash escapes the next character. At most 64, each printable and under 4096 bytes |
| Timeout | Seconds, 1 to 86,400 |

**Data**

| Field | Notes |
| --- | --- |
| File | A path inside `data/` |
| Format | CSV, TSV, JSON, Text, Markdown, Image, PDF, Parquet or Binary; picked from the extension and checked against it |

**Directory**

Reads a directory as one text, so an agent can work on a whole repository: a listing of
the files that pass the filters, then their contents up to a budget. Point it at `.` and
the graph reads the repository the app runs in.

| Field | Notes |
| --- | --- |
| Directory | Relative to the working directory; `.` or empty is the working directory itself. Must be inside it |
| Include patterns | Space-separated globs relative to the directory, e.g. `src/**/*.rs *.md`. `*` crosses `/`, so `*.rs` takes Rust files at any depth. Empty takes every file |
| Exclude patterns | Globs to skip. Hidden entries and symlinks are always skipped |
| Respect .gitignore | On by default, so `target/` and the like stay out of a repository scan |
| Include file contents | Off gives the listing only |
| Max files | 1 to 10,000; the listing says when more matched |
| Content budget | Bytes of contents after the listing; files are cut when it runs out. The agent downstream must still fit everything in its context size |

**Write file**

Writes the text arriving on its wire to a file in the working directory: the way an
agent's answer becomes a test file, a document or a data file in the repository. Wire
**Directory → Agent → Write file** and a run reads the code, drafts the tests and lands
them on disk. The node is red because it changes files outside the app's own folders.

| Field | Notes |
| --- | --- |
| File | Relative to the working directory; directories on the way are created. The path is yours, never the model's. `scripts/`, `agents/`, `prompts/`, `databases/`, `nodes/` and `graphs/` are refused, so a run can never plant a script or rewrite a preset. `data/` is allowed |
| If the file exists | **Create only** (the default) refuses to touch an existing file; **Overwrite** replaces it; **Append** adds to it |
| Strip a code fence | On by default: a surrounding ```` ``` ```` fence is removed and the text ends with a newline, which is what a model's answer needs to become a source file. Off writes the bytes as they came |

A single binary input (an image from an agent) is written byte for byte. The node's output
is what it wrote, so a script can run on the file next.

## Running nodes

Press **Run** in the inspector. The executor runs the selected node's upstream nodes first
(each once, in a stable order, refusing cycles), joins their text outputs with blank lines,
and hands that to the node:

| Node | What runs | Takes from the wire | Produces |
| --- | --- | --- | --- |
| Data | Reads the file (up to 64 MB) | nothing | the file; text for CSV, TSV, JSON, Text and Markdown, binary otherwise |
| Script | `uv run --quiet <file>`, `sh`/`bash`/`zsh <file>` or `node <file>` with the arguments, in `scripts/` | stdin | stdout as text; non-zero exit or the timeout is an error |
| Database | `sqlite3 -json`, `duckdb -json`, `psql --csv` or `mysql --batch` with the query on stdin | the SQL to run, only when the node's own query is empty; data comes from the input file | rows as JSON (file engines) or CSV/TSV (server engines) |
| Agent | One request to the provider | the user message | text, JSON, an embedding vector or an image, saved to `agents/<name>/outputs/` |
| Directory | Walks the directory (hidden entries and symlinks skipped, `.gitignore` honored when asked, 32 levels deep) | nothing | text: `Directory <path>: N files matched`, one line per file with its size, then `=== path ===` sections with contents until the budget is spent; binary files are named, not shown |
| Write file | Creates, replaces or appends to the file, after checking the path again and resolving symlinks | the content | the content it wrote, with the file's path in the status line |

**Streaming.** Generative agents are asked for a streamed reply (`stream: true`, or
Google's `streamGenerateContent`), the body is read as it arrives, and every piece of text
reaches the inspector while the model is still typing. JSON output is validated and
pretty-printed once the stream ends, so the preview shows the raw text and the result is
the finished document. A server that ignores the request and answers in one piece still
works. Embeddings and images are not streams.

**Cancel.** The run checks a cancel flag before every node, between the pieces of a
streamed reply, and inside the wait for a child process, which is killed with its process
group. A provider call that does not stream (embeddings, images) finishes or times out
first. A cancelled run saves nothing and reports `Cancelled`.

**Cache.** The inspector keeps every agent's and script's output across runs, keyed on the
node's configuration, its inputs' outputs and (for scripts) the script file itself, so
running a downstream node again re-uses the agents above it. Data, directory, database and
write-file nodes always run: they read or change live state, and they are cheap. Every
node's output is hashed for the nodes after it, so editing a comment in a script does not
re-run the agent that reads its output, while changing a data file does. The log says
`n3: reused the cached Text · 2 KB` for each hit, the status line counts them, **Run
fresh** runs everything and refills the cache, and **Clear** under the run drops it. The
cache lives in memory for the session; agent outputs are also on disk under `agents/`.

Agents send the prompt as the system message and the wire text as the user message; with
nothing on the wire the prompt is the user message. Prompt plus input must fit the context
size (estimated at four characters per token) or the run is refused before anything is
sent. JSON output adds an instruction to the system prompt on every provider and is also
requested natively where the API has a flag (OpenAI-style `response_format`, which LM
Studio only accepts in its `json_schema` form; Ollama `format`; Google
`responseMimeType`). It is always validated and pretty-printed; fenced or chatty replies
are unwrapped.

A database node with an empty query runs the text arriving on the wire as its statement,
with a surrounding code fence stripped. That SQL goes through the same checker as a typed
query, with the node's read-only flag, so an agent wired into a read-only node can only
read. Wire a generative agent into a **Database (write)** node and the agent's SQL can
change the database: that is what the red color warns about. A writable SQLite or DuckDB
node may name a file that does not exist yet, in an existing directory under `data/`; the
engine creates it. Read-only nodes need an existing file.

Provider notes: when a reply names the model that answered and it is not the one asked for
(LM Studio answers an unknown model id with whatever is loaded), the log says so:
`⚠ answered by mistralai/ministral-3-3b, not my-typo: the server substituted a model`.
Snapshot and tag suffixes (`gpt-4o-2024-08-06`, `llama3.1:latest`) count as the same model.
Ollama is called on its native API so the context size reaches the model as `num_ctx`. Anthropic is called on API version 2026-10-21 with server-side refusal
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
  arguments. Write-file nodes refuse `scripts/` and every other directory the app manages,
  at edit time and again at run time, so a run can never create a script, an agent, a
  prompt, a database preset, a kind or a graph.
- **Write-file paths are the user's.** The path is a setting on the node; nothing on the
  wire can choose it. It is resolved with symlinks followed and must stay under the working
  directory, missing directories included.
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
| `data/` | Data files and database files for data and database nodes; write-file nodes may write here |

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
