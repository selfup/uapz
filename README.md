# uapz

A desktop node-graph editor with a Tauri 2 shell, a React and React Flow canvas, and a
Rust execution core. Nodes are boxes you drag around and wire together; each one can
carry typed content: an **agent**,
a **database**, a **script**, a **data** file, a **directory**, a **file to write**, a
named **Run input** supplied each time the graph runs, or a **Workflow** calling a saved graph.
Graphs save as a Markdown file with a Mermaid diagram plus a JSON sidecar, and reusable
agents, prompts and databases save into their own directories so they show up in the
sidebar.

Nodes run from the inspector. A node runs after everything wired into it, text flows along
the wires, agent answers stream into the panel as they are generated, and every agent
result is saved under that agent with a timestamp. Outputs are cached between runs, so
running a downstream node again does not re-call the agents above it unless something they
depend on changed.

## Running

Use Node.js 24, npm, stable Rust and the macOS Xcode command-line tools. To build and
run the app with its frontend embedded:

```sh
./scripts/run-app.sh
```

The script installs frontend dependencies if `node_modules` is missing, checks Rust
formatting, builds the frontend, checks the Rust code, then builds and launches the
release executable. It uses this repository's workspace by default and works from any
current directory. Use `--debug` for faster builds, or select another workspace:

```sh
./scripts/run-app.sh --debug
./scripts/run-app.sh --workspace /absolute/path/to/workspace
```

`UAPZ_WORKSPACE_DIR` also overrides the script's default workspace; an explicit
`--workspace` takes precedence. Run `npm ci` again after dependency lockfile changes.
For development with frontend hot reload:

```sh
npm ci
cargo check --workspace --all-targets
npm run desktop
```

`npm run desktop` starts Vite and the Tauri desktop shell together. To build the
production application and macOS bundles:

```sh
cargo check --workspace --all-targets
npm run desktop:build
```

Development uses the repository root as its workspace by default. Packaged applications
use a `workspace` directory under Tauri's application-local data directory. Set
`UAPZ_WORKSPACE_DIR` or pass `--workspace PATH` to select an existing workspace; the command
line takes precedence. For example:

```sh
UAPZ_WORKSPACE_DIR=/absolute/path/to/workspace npm run desktop
```

The workspace holds graphs, presets, scripts, data, cache and history. Existing workspace
directories can be opened directly; graph and history formats remain compatible.

Tests cover the file formats, the Mermaid parser, every node config's validation, the SQL
checker, path confinement, the library's on-disk layout, the executor (scripts, SQLite,
chains, timeouts, the cache, cancellation, write-file nodes), streamed replies in every
provider's shape and the request shape for every provider. Frontend unit tests cover
graph edits, snapshot selection, output comparisons and the IPC wrapper. Browser tests
cover named-port wiring, labels and colors, keyboard edits, read-only children, saved
positions, run and review recovery, and delayed save, navigation and output responses:

```sh
cargo fmt --all --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm run check
npm run build
npm test
npx playwright install chromium
npm run test:ui
```

Playwright runs the frontend in Chromium with mocked Tauri IPC; it does not execute
fixture workflows or replace testing the native Tauri bridge. Browser failure artifacts
are written under `test-results/`. The shared fixture in `tests/support/workspace.ts`
can fail individual requests or hold them until explicitly released, so response-order
regressions do not depend on arbitrary delays. Unexpected browser errors and warnings
fail the tests.

The opt-in native smoke test starts the real Tauri webview and exercises IPC, graph
save/load, run inputs, file review, execution, output loading and history. It is available
in debug builds and requires an explicit disposable workspace because it creates a
fixture graph and output file. Run:

```sh
smoke_workspace="$(mktemp -d /private/tmp/uapz-smoke.XXXXXX)"
./scripts/run-app.sh --debug --workspace "$smoke_workspace" --smoke-test
```

The application prints the result and exits automatically. The temporary workspace keeps
the generated fixtures for inspection.

CI (`.github/workflows/ci.yml`) runs these Rust and frontend checks on a macOS runner
for every push and pull request. After installing dependencies and Chromium,
`scripts/run-tests.sh` runs the same validation locally. `scripts/run-tests.sh local`
also starts LM Studio, reloads its models and runs the live suite with this machine's defaults.

Live tests drive the executor against a real OpenAI-compatible server and a PostgreSQL
server; they are ignored by default. With a chat model and an embedding model loaded in LM
Studio (or any server named by `UAPZ_LIVE_ENDPOINT`) and a local PostgreSQL:

```sh
UAPZ_LIVE_MODEL=mistralai/ministral-3-3b \
UAPZ_LIVE_EMBED_MODEL=text-embedding-nomic-embed-text-v1.5 \
UAPZ_LIVE_PG_URL=postgres://localhost/postgres \
cargo test -p uapz live_ -- --ignored --nocapture
```

## Using it

The project header holds the workflow name, Save, New and Run. A searchable library
is on the left, the canvas is in the center, and the inspector is on the right. Drag
the dividers to resize either panel. Run status and review shortcuts stay visible in
the footer, including while inspecting a child workflow.

**Canvas**

- Drag a node to move it. Drag empty space to pan; scroll to zoom. Pan and zoom are
  saved with the graph.
- Drag from a node's right-hand port to another node's left-hand port to connect them.
  Self-connections and duplicates are ignored.
- Click a node or a wire to select it. Backspace or Delete removes it. Deleting a node
  removes its wires.
- Double-click empty space, press cmd-n or ctrl-n, or click **Add node** to add the type
  selected in the toolbar: **Blank**, one of the eight types, or **Database (write)**.
  Library rows add presets and saved kinds directly. A writable database starts with its
  read-only setting off.
- Escape clears the selection.
- Neutral node cards show their name, type, summary and run status, with a slim accent
  in the saved node color (or the type's default color). Titles use the node's own name,
  then its preset or kind label, then its type. Red type labels identify write-file
  nodes and writable databases.
  Workflow cards have a labeled port for each published output. Drag from the specific
  port to choose the value sent along that wire. Two ports can feed the same target.
- **Fit graph** fits all nodes in the canvas, adjusting pan and zoom. New graphs start empty.
- Double-click a Workflow card, or choose **Open child graph** in the inspector, to
  inspect its child on the canvas. Breadcrumbs return to any ancestor while preserving
  the parent's draft, selection, view and active run. Child previews are read-only.
  During execution they use prepared snapshots and show live status; from history they
  use the recorded graph, outputs and nested snapshots. **Return to run** and **Review
  changes** bring you back to the parent when it needs attention.
- The dots in the nodes' corners follow the latest run. Nodes taking part are marked
  pending, then amber while their own step runs, green when they produce output, blue
  when their output comes from the cache, and red when they fail. A node the run never
  reached loses its dot. Data, directory, database
  and write-file nodes always run, so they are always green; an agent or script that stays
  green on a rerun had something upstream change, and the log says which nodes were
  reused. Loading or clearing the graph removes the dots.

**Inspector**

- Select a node and its content appears as a form. Every edit writes straight back into
  the node; nothing is saved to disk until you save the graph or a preset.
- **Configure**, **Results** and **History** separate editing from execution. Configure
  holds input mappings, publication names and optional node-type controls. Results shows
  individual named outputs, child execution and an expandable execution log. History
  opens earlier runs. Run forms and pending file reviews appear above the current tab.
- **Name**, at the top of every node's form, labels the node on the canvas and lets an
  agent wired after it write `{{name}}` for its output. Names are letters, digits, `_`
  and `-`, start with a letter, and must be unique to be useful; the panel says when one
  is not.
- A blank node offers buttons to give it one of the eight content types, plus
  **Database (write)** with its read-only setting off.
  A typed node can change type
  or go back to blank under **Node options** in Configure.
- Problems are listed under the form as they happen (`model: model is empty`, `query:
  DELETE is not allowed in a read-only query`). A valid config says so.
- **Save agent** / **Save database** writes the config into the library under the name in
  the field next to it and labels the node with that name. Invalid configs are refused.
- In an agent, **Save prompt** writes the prompt text to `prompts/<name>.md`. Clicking a
  prompt in the sidebar loads it into the selected agent.
- An agent's **Message** is the user message. Empty sends the inputs joined with blank
  lines, as before. Otherwise `{{name}}` in it, or in the prompt, is replaced by that
  input's output, and the inputs nothing names follow the message. The **Inputs** list
  under the form shows each wired node, how to address it (`{{schema}}` or `n2`), where
  it ends up, and what it produced last time. A placeholder that names nothing is
  reported as a problem before you run.
- **Run node** executes the selected node after everything wired into it. While it runs, the
  log grows line by line and an agent's answer streams into a preview as it is generated;
  **Cancel** stops it. The result and the log appear in Results;
  the status line says where an agent's output was saved and how many nodes came from the
  cache. **Run fresh** ignores the cache. See *Running nodes*.
- Every node that took part in a run keeps its result. Select a node in the middle of the
  graph and open **Results** to see its saved file, a text preview or its failure, with a
  warning when the node has been edited since. **Previous output** compares the latest
  result with an earlier run, including configuration changes and named outputs; partial
  previews are identified rather than treated as complete matches. Open an earlier run from **History** to
  inspect its recorded outputs. Loading another graph clears the current result selection.
- **History** lists the graph's past runs
  (newest first, times in UTC). Open a run to bring back
  its outputs, dots and log without running anything; the status line says so. **Run workflow**
  (also cmd-enter and the header button) runs every end node of the graph in one pass.
  See *Running nodes*.
- **Run input** nodes turn a saved graph into a reusable tool. Run node and Run workflow first ask
  for the named inputs their targets need. Change the values in the Run form and press
  **Start run**; the graph's saved configuration and defaults stay unchanged.
- **Workflow** nodes call another saved graph. Use its published outputs or select one
  output node, then map named inputs to defaults, values or wired parent outputs.
  **Open child graph** opens the visual preview. From **Results**, open the child to
  inspect its recorded outputs, logs and failures.
- A write-file node with **Review changes before writing** enabled pauses with a diff
  for every proposed file. **Apply** approves the entire batch and continues downstream;
  **Reject batch** leaves that batch untouched; **Cancel run** stops the whole run.

**Sidebar**

Search by name, category or path. Matching sections expand while searching; their
headers show matching counts. Every section header also collapses on click.

- **Workflows**: saved graphs. Click one to load it; name, Save (cmd-s) and New
  (cmd-shift-n) are in the project header.
- **Nodes**: built-in node types, ready to add to the current workflow.
- **Custom nodes**: blank, colored node kinds. **Custom node kind** opens the name and color
  fields; **Save kind** adds the kind to the library. Click a row to add a node, or use
  its delete button to remove the kind. Type names are reserved.
- **Agents** and **Databases**: saved presets. Click one to add a node made from it;
  **×** deletes the preset (an agent's outputs are kept).
- **Prompts**: click loads the prompt into the selected agent node; **×** deletes it.
- **Scripts** and **Data**: the files under `scripts/` and `data/`. Click one to point the
  selected script/data node at it, or to add a node for it when no such node is selected.
  **↻** re-reads the directories.

## Node types

**Run input**

| Field | Notes |
| --- | --- |
| Name | Required, unique among the run's inputs; also the name an agent wired after it uses in `{{name}}` |
| Input type | Text, Number, Boolean, JSON, or File from `data/` |
| Default value | Used initially when the Run form has no previous value for this name; may be empty even for a required field |
| A value is required | On by default. Clearing a required value is an error; clearing an optional value sends empty text |

Run inputs are source nodes, so they have no incoming wires. Text preserves its spacing;
numbers must be finite JSON numbers; booleans are `true` or `false`; JSON is validated and
compacted. File inputs offer a selector over `data/` plus a relative-path field, and send
the selected file's contents (up to 64 MB), as text when valid UTF-8 and binary otherwise.
File paths cannot leave `data/`, including through symlinks.

For example, add a Text input named `question` and a File input named `source`, wire both
to an agent, and use `{{question}}` and `{{source}}` in its prompt or message. Wire the
agent to a write-file node and enable review there. **Run workflow** now asks for the question
and document, generates an answer, and pauses for file review before applying it.

Only inputs reachable from the requested targets appear in the form. All their values
are validated before any node executes. A run accepts up to 64 fields, 256 KB per value,
and 1 MB of values combined. Actual file contents have their separate 64 MB limit.

The latest run's values prefill the next form, including after reopening the graph.
Opening an older entry in **Runs** uses that entry's values for the next form. Values,
including effective defaults, are stored in the run record and shown below the run list;
credentials should continue to use the existing environment-variable fields. Input
values and file contents both participate in downstream cache keys, so changing either
recomputes affected agents and scripts. Saving a graph stores input definitions and
defaults, never the Run form's overrides.

To try both features without a model or API key, copy the included example into the
graph library before starting the app:

```sh
mkdir -p graphs
cp examples/reviewed-note.json examples/reviewed-note.md graphs/
```

Open **reviewed-note** and press **Run workflow**. Edit `note`, start the run, then inspect
and apply or reject the proposed change to `generated/note.md`.

**Workflow**

| Field | Notes |
| --- | --- |
| Saved workflow | A graph in `graphs/`; save the child before calling it |
| Outputs | **Published outputs** exposes the child's named ports. Selecting a single child node retains the original single-output behavior |
| Named input mappings | Inputs reachable from the selected outputs. Choose **Use default**, **Value**, or a directly wired parent output, labeled by node and port |
| Open child graph | Opens the saved child or the exact snapshot from the displayed run, with breadcrumbs back to the parent |
| Refresh | Re-reads the saved graph, output ports and available parent wires after edits |

For example, save a graph with a Run input named `question`, wire that input to an
agent, and select that agent as the return output of a Workflow node in another graph.
Wire the parent's source into the Workflow node and select it for `question`. You can
also enter a fixed value or use the child's default. Omitted mappings use defaults;
an explicitly empty value still fails a required input. The parent Run form only asks
for the parent's reachable Run inputs; child values come from these mappings.

To publish multiple outputs, select each result node in the child graph and enter a
name under **Configure → Published outputs**, then save the child. A comma-separated
list publishes the same result under multiple names. Names follow the same identifier
rules as Run inputs and must be unique across the graph. A graph can publish up to 64
outputs. In the parent Workflow node choose **Published outputs**; its card now exposes
one labeled port per name. All outputs share one child execution, so common upstream
agents and side effects are not repeated for each wire.

Agents can address a wired named output as `{{parts.heading}}` or `{{n2.heading}}`.
Workflow input mappings offer that same wired output. Each port keeps its own bytes and
text/binary metadata; binary outputs can flow to a write-file node. A multi-output call
requires a named wire, while a call with exactly one published output also accepts an
ordinary wire. Old graphs with a selected output node continue to work unchanged.
When publishing from a nested Workflow node, choose its source port before entering the
publication name. This forwards that port directly, including binary output.

Graph JSON stores publications as `"outputs": {"heading": "n0", "body": "n2.body"}`;
a named wire adds `"output": "heading"` to its usual `from`/`to` object. These fields
are optional, so older documents and history remain readable. Missing ports and invalid
or ambiguous publications fail preflight before upstream side effects. A stale port
stays visible on existing wires, but cannot start a new connection.

Values retain the Run input limits and type checks: up to 64 fields, 256 KB per value,
and 1 MB combined per invocation. File mappings supply a path relative to the shared
`data/` directory, not file bytes; select **Value** to enter a path, or wire a Text
input or other node that returns the path. A File Run input itself returns file bytes.
Binary outputs can be returned from a child, but cannot be mapped into a named input.

Before execution, the executor snapshots reachable called graphs and checks their
dependency graphs, mappings, defaults and literal values. Missing graphs or outputs,
invalid configurations, unwired mappings and recursive calls fail the calling target
before its upstream steps run. Recursion includes indirect calls; nesting is capped
at eight calls, with at most 128 calls and 4 MB of graph snapshots per run. Graph files
and the `graphs/` directory must be ordinary files/directories, not symlinks. Dynamic
mapped values are checked together after parent inputs finish and before any child step.
Independent parent targets retain the normal behavior of continuing after a failure.

Calls use saved snapshots for that entire run, even if a graph is edited while it runs.
The parent's history contains each child's snapshot, effective inputs, node results,
logs and nested runs. Prepared children that were never reached retain their snapshots
without being marked as executed. Reopening history displays those recorded results; a new run reads
the current saved graphs. All paths use the parent's working directory. Child node IDs
are qualified by their callers in progress, reviews and cache bookkeeping, for example
`n3/n1`; unassigned agent outputs use a corresponding folder such as `agents/n3-n1/`.

Workflow nodes always enter the child, so reads, databases and file writes still run.
Agents and scripts inside it can reuse cached outputs. Called graph contents (including
nested snapshots), mappings and effective input values enter cache keys; file inputs
also contribute their contents. Changing a called graph invalidates affected child
and downstream caches even when the returned bytes stay the same. **Run fresh** and
**Cancel** apply through the entire call tree. Child file reviews use the same Apply,
Reject and Cancel controls; a rejected child stops its calling node and dependents.

Try a complete offline parent/child example:

```sh
mkdir -p graphs
cp examples/reviewed-note.json examples/reviewed-note.md graphs/
cp examples/call-reviewed-note.json examples/call-reviewed-note.md graphs/
```

Open **call-reviewed-note**, press **Run workflow**, edit `message`, and start the run.
The child **reviewed-note** asks for review before writing `generated/note.md`.
Select the parent's **save-note** node and expand **Show child execution** in Results,
or double-click its Workflow card to inspect the child canvas.

For two named outputs and visual navigation, copy `examples/note-parts.json`,
`examples/note-parts.md`, `examples/use-note-parts.json` and `examples/use-note-parts.md`
into `graphs/`. Open **use-note-parts** and double-click **parts** to inspect the child;
return with the breadcrumb, then **Run**. The heading and body are reviewed separately
before being written to `generated/heading.md` and `generated/body.md`.

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
| File | Relative to the working directory; directories on the way are created. The path is yours, never the model's. `scripts/`, `agents/`, `prompts/`, `databases/`, `nodes/`, `graphs/` and `cache/` are refused, so a run can never plant a script or rewrite a preset. `data/` is allowed |
| If the file exists | **Create only** (the default) refuses to touch an existing file; **Overwrite** replaces it; **Append** adds to it |
| Split sections | Off: the whole text becomes the one file. On: the field names a directory (`.` for the working directory) and the text is split on `=== path ===` headers, one file per section under it. See below |
| Strip a code fence | On by default: a surrounding ```` ``` ```` fence is removed and the text ends with a newline, which is what a model's answer needs to become a source file. Off writes the bytes as they came |
| Review changes before writing | Off by default for compatibility. On pauses the run with a per-file diff before this node creates directories or writes anything |

A single binary input (an image from an agent) is written byte for byte. The node's output
is what it wrote, so a script can run on the file next.

**Several files from one answer.** The section format is the one a directory node emits, so
an agent that read the repository can answer in kind. Prompt it with something like
*"Answer with one `=== path ===` section per file, nothing else"* and turn **Split
sections** on:

```
=== src/add_test.rs ===
#[test]
fn adds() { assert_eq!(add(2, 2), 4); }

=== notes/README.md ===
# notes
```

Section paths are relative to the node's directory and go through the same checks as any
path (no `..`, no hidden names, no app directories, including after following symlinks).
Every path is checked and resolved before anything is written, and in **Create only**
mode one existing file stops the whole
write. Text before the first header is ignored, each section's own code fence is stripped,
aliases to the same file and file/directory conflicts are errors, and one answer may write
at most 500 files. Create only also refuses a file created by another writer after the
initial check. The output lists what was written.

**Review and apply.** A reviewed write shows the final bytes after code-fence removal and,
for Append, the resulting file with its original contents retained. Use **Previous file**
and **Next file** to inspect a batch, and the page controls for long diffs. Binary changes
show their byte counts and are explicitly identified as having no text diff.

Applying rechecks every destination against its captured contents and resolved path.
If a file was edited, deleted, created or redirected through a symlink during review,
the entire batch is refused before its first write; rerun to see the updated diff.
Rejection stops this node and its dependents; independent workflow targets can continue.
Cancellation stops a pending review. The footer provides a way back to the review while
inspecting a child workflow.
Earlier completed nodes and their saved agent outputs remain available.

Review captures at most 64 MB of original files and produces at most 1 MB of diff text.
Larger diffs are refused with a request to split the batch; changed lines are never
silently omitted. These are preflight checks, not a filesystem transaction: an I/O
failure during application can leave earlier files in the batch written. Reviewed nodes
require an interactive reviewer; an executor call without one fails instead of writing.

## Running nodes

Press **Run node** in the inspector. The executor checks the target's complete dependency graph
for cycles, missing nodes and invalid configurations before executing any of its steps.
It then runs upstream nodes first (each once, in a stable order), joins their text outputs
with blank lines, and hands that to the node. Combined text input is limited to 64 MB,
including separators. Traversal uses an explicit stack so long chains do not exhaust the
thread stack. Running the workflow also checks cyclic components with no terminal node, and continues
with independent targets after a failure:

| Node | What runs | Takes from the wire | Produces |
| --- | --- | --- | --- |
| Run input | Validates the supplied value or default; a File input reads its selected file | nothing | typed text, JSON, or file bytes |
| Data | Reads the file (up to 64 MB) | nothing | the file; text for CSV, TSV, JSON, Text and Markdown, binary otherwise |
| Script | `uv run --quiet <file>`, `sh`/`bash`/`zsh <file>` or `node <file>` with the arguments, in `scripts/` | stdin | stdout as text; non-zero exit or the timeout is an error |
| Database | `sqlite3 -json`, `duckdb -json`, `psql --csv` or `mysql --batch` with the query on stdin | the SQL to run, only when the node's own query is empty; data comes from the input file | rows as JSON (file engines) or CSV/TSV (server engines) |
| Agent | One request to the provider | the user message, or the `{{name}}` placeholders of its prompt and message | text, JSON, an embedding vector or an image, saved to `agents/<name>/outputs/` |
| Directory | Walks the directory (hidden entries and symlinks skipped, `.gitignore` honored when asked, 32 levels deep) | nothing | text: `Directory <path>: N files matched`, one line per file with its size, then `=== path ===` sections with contents until the budget is spent; binary files are named, not shown |
| Write file | Creates, replaces or appends to the file, after checking the path again and resolving symlinks | the content | the content it wrote, with the file's path in the status line |

**Named inputs.** Give an upstream node a name in the inspector and an agent after it can
write `{{name}}` in its prompt or its message; `{{n2}}` addresses a node by id. Each
placeholder is replaced by that input's text, and the inputs no placeholder names are
joined with blank lines and follow the message (or are the message when the message is
empty, which is what every other node gets). A prompt that places every input and leaves
no message becomes the user message, as a prompt with no input always has. A placeholder
that names nothing, or a name two inputs share, is an error before anything is sent; a
name that does not validate is ignored, and the node stays reachable by id. The log says
what went where: `n3: placed inputs: {{schema}} from n0 · {{question}} from n1 · n2
appended`. Names are part of an agent's cache key, so naming an input is an edit;
graphs without names or placeholders keep their cached entries. Templates also include
the upstream node ids in the key, so rewiring an input cannot reuse an answer with a
different or missing placeholder binding.

**Run workflow and the run history.** **Run workflow** picks every node with content that feeds no
other node with content, plus a representative of each otherwise uncovered cyclic
component. It runs these targets in id order in one pass: a node two of them share runs
once, a failed node is not tried again for the next target, and a failed target does not
stop the others. Cancellation stops the pass even after an earlier failure. Every
finished run is written to
`runs/<graph>/<timestamp>.json` with its log, outcome, targets, resolved run inputs, and for every node its
state, its content at the time, the label and a 4000-character preview of its output, and
the content key under which `cache/` holds the full bytes (outputs over 8 MB keep only the
preview). Two hundred records are kept per graph; unsaved graphs file under `untitled`.
The **Runs** list in the inspector reopens any of them without running, and **Previous**
under a node's last output compares the two runs.

**Streaming.** Generative agents are asked for a streamed reply (`stream: true`, or
Google's `streamGenerateContent`), the body is read as it arrives, and every piece of text
reaches the inspector while the model is still typing. JSON output is validated and
pretty-printed once the stream ends, so the preview shows the raw text and the result is
the finished document. A server that ignores the request and answers in one piece still
works. Multiline SSE events and final records without a newline are handled; malformed
stream JSON, invalid UTF-8 and trailing error events fail the run instead of saving a
partial answer. Reading stops when a reply exceeds 64 MB. Embeddings and images are not
streams.

**Cancel.** The run checks a cancel flag before every node, between the pieces of a
streamed reply, and inside the wait for a child process, which is killed with its process
group. A provider call that does not stream (embeddings, images) finishes or times out
first. A reply received after cancellation is not saved or cached, and the run reports
`Cancelled`. Outputs completed before cancellation and the run's history remain.

**Cache.** Every agent's and script's output is kept across runs, keyed on the node's
configuration, its inputs' outputs and (for scripts) the script file itself, so running a
downstream node again re-uses the agents above it. Data, directory, database and
write-file nodes always run: they read or change live state, and they are cheap. Every
node's output is hashed for the nodes after it, so editing a comment in a script does not
re-run the agent that reads its output, while changing a data file does. The log says
`n3: reused the cached Text · 2 KB` for each hit, the status line counts them, **Run
fresh** runs everything and refills the cache, and **Clear** under the run drops it.

The cache lives in `cache/` as `<key>.json` (label, type, saved path, time) plus
`<key>.bin` (the bytes), so it survives a restart and is shared by every graph: another
node asking the same question of the same inputs gets the same answer without a call. Keys
are content hashes (two FNV-1a passes, 128 bits), stable across builds. The directory is
pruned oldest-first past 1 GB, a broken entry is a miss, and a disk that refuses the write
costs persistence, not the run. Cache metadata is limited to 64 KB and payloads to 64 MB;
the stored length and checksum must match before bytes are reused. Entries made before
these integrity fields were added are treated as misses and refilled on the next run;
old history records can still show their saved previews. Add `cache/` to `.gitignore`, as
this repository does.

Agents send the prompt as the system message and the wire text as the user message; with
nothing on the wire the prompt is the user message. Prompt plus input must fit the context
size (estimated at four bytes per token, rounding up) or the run is refused before anything
is sent. Template expansion checks that shared budget before copying each substitution;
repeated placeholders and appended inputs count toward the same limit. JSON output adds
an instruction to the system prompt on every provider and is also
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
input file is supported for SQLite (CSV, imported into the `temp` schema with text columns, so
a read-only database stays untouched and `007` stays `007`) and DuckDB (CSV, TSV, JSON,
Parquet, into a temporary table).
Server engines read the connection URL from the named environment variable and pass it to
the tool through its own environment variables (`PGHOST`, `PGPASSWORD`, `MYSQL_PWD`, …),
never on the command line. Read-only is enforced by the engine too: `-readonly` for SQLite
and DuckDB, `default_transaction_read_only` for PostgreSQL, `SET SESSION TRANSACTION READ
ONLY` for MySQL.

Child processes get a clean environment (`PATH`, `HOME`, `TMPDIR`, `LANG`, `LC_ALL`, `TERM`
only, plus the one secret a database tool needs), stdin for their input, and are killed at
the timeout (the script's own, five minutes for database tools and HTTP). Timeout and
cancellation remain active after the direct child exits if its descendants still hold
the input or output pipes open.

## Security

Configs are checked while editing and again by the executor before running.

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
- **SQL is checked before it reaches an engine.** Exactly one statement, starting with a
  supported verb for the selected engine. Writable nodes allow read verbs and
  INSERT/UPDATE/DELETE/REPLACE/MERGE/CREATE/ALTER/DROP/TRUNCATE where supported; unlisted
  verbs such as SET, DO and CALL are refused. `VACUUM`, `PRAGMA`, IMPORT and EXPORT are
  refused in both modes.
  MySQL/MariaDB executable comments and nested comments are rejected; `--` must be
  followed by whitespace. A backslash immediately before a string's quote is ambiguous
  across engines and refused: use doubled quotes (`'it''s'`) instead. Quoted identifiers
  are scanned like bare ones (so `"pg_read_file"(...)` is caught). Read-only queries must
  start with SELECT, WITH, EXPLAIN, SHOW, DESCRIBE or VALUES
  and may not contain writing or session keywords anywhere, including inside CTEs. Every
  query, read-only or not, is refused if it names something that reaches outside the
  database: ATTACH, LOAD, COPY, GRANT, `INTO OUTFILE`, `pg_read_file`, `load_extension`,
  DuckDB's `read_csv`/`read_parquet`/`glob`, `xp_cmdshell` and friends. Files get into a
  database through the validated input field, not through the query.
- **Scripts are files you placed there.** The app never writes to `scripts/`; a node can
  only point at an existing file with a runtime-matching extension and pass bounded
  arguments. Write-file nodes refuse `scripts/` and every other directory the app manages,
  at edit time and again at run time, so a run can never create a script, an agent, a
  prompt, a database preset, a kind, a graph, a cache entry or a run record.
- **Write-file paths are the user's.** The path is a setting on the node; nothing on the
  wire can choose it unless Split sections is enabled, in which case section paths are
  relative to the configured directory. Paths are resolved with symlinks followed and
  must stay under the working directory, missing directories included. The resolved
  destination must also pass the hidden-name and managed-directory restrictions.
- **Saving is strict, drafting is not.** The graph file keeps whatever you typed, flagged
  in the inspector. The library refuses to save an invalid agent or database, and the
  canvas shows the first problem.
- **Saved state is bounded.** Graphs, kinds, presets and run records are limited to 16 MB
  per file; prompts keep their 256 KB limit. Reads reject symlink files and non-regular
  files, and the byte limit is enforced during reading as well as against metadata.
  Cache and history operations refuse symlinked storage directories. Replacement saves
  use exclusive temporary files and rename; new run records and agent outputs use
  exclusive creation so simultaneous saves cannot overwrite each other.
- **Provider endpoints are checked on every request path.** Helpers such as Write SQL
  validate endpoints and model ids too. HTTP redirects are refused for both streamed and
  whole replies, so prompts and provider-specific key headers stay at the configured
  endpoint. Configure the final API endpoint directly if a server returns a redirect.
- **The executor trusts nothing it didn't check itself.** Every config is validated again
  before it runs. Every file is opened through `resolve_inside`, after symlinks. Secrets are
  read from the environment at the last moment and go only into the request header or the
  child's environment: never argv, never the log, never a file. Child processes get the
  minimal environment above and a hard timeout. Model ids are checked before they are
  spliced into a URL path. Replies are size-capped. Read-only queries are enforced twice:
  by the SQL checker and by the engine.

## Files

All directories are created under the selected workspace on first use. They are plain
text, so they are easy to commit, diff or hand-edit. Development defaults to the repository
root; packaged applications default to their application-local data directory's
`workspace` folder. Override this with `--workspace PATH` or `UAPZ_WORKSPACE_DIR`.

| Path | Contents |
| --- | --- |
| `graphs/<name>.md` | Human-readable graph: a Mermaid flowchart |
| `graphs/<name>.json` | Full editor state: positions, pan/zoom, kinds, colors, typed node content and publications |
| `nodes/<name>.json` | A blank kind: `{ "name": "Fetch", "color": "#ff8800" }` |
| `agents/<name>/agent.json` | A saved agent config |
| `agents/<name>/outputs/<timestamp>.<ext>` | One file per run, `YYYYMMDDTHHMMSSmmmZ` UTC, `.txt`/`.json`/`.png` by output type |
| `prompts/<name>.md` | A reusable prompt |
| `databases/<name>.json` | A saved database config |
| `scripts/` | Script files for script nodes (`.py`, `.sh`, `.js`, …) |
| `data/` | Data files and database files for data and database nodes; write-file nodes may write here |
| `cache/<key>.json`, `cache/<key>.bin` | The executor's output cache, by content hash; safe to delete |
| `runs/<graph>/<timestamp>.json` | One record per run: log, outcome, every node's state, content and output preview; safe to delete |

### `graphs/<name>.md`

Node ids are stable, a node's label is its name (else the kind it was made from), labeled
nodes carry a `style` line with their color, and GitHub, VS Code and most Markdown tools
render the block.

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
made from), an optional `name` (the node's own, for the canvas and for `{{name}}`), a
`color` copied at save time, and `data`: `{}` for a blank node, or a typed object tagged
with `"type"`. An agent's `message` is written only when it is not empty.
`view.zoom` is optional and defaults to `1`, so existing graphs without a zoom value
retain their original scale. Named output publications and qualified edges keep their
existing representation.

```json
{
  "version": 1,
  "name": "pipeline",
  "view": { "pan_x": 0.0, "pan_y": 0.0, "zoom": 1.0 },
  "nodes": [
    {
      "id": "n0", "x": 160.0, "y": 220.0, "kind": "summarizer", "color": "#8b5cf6",
      "data": {
        "type": "agent",
        "provider": "anthropic",
        "model_type": "generative",
        "model": "claude-opus-5-5",
        "auth": { "kind": "env", "var": "ANTHROPIC_API_KEY" },
        "prompt": "Summarize {{notes}} in three bullets.",
        "context_size": 8192,
        "temperature": 0.3,
        "output": "text"
      }
    },
    { "id": "n1", "x": 460.0, "y": 330.0, "name": "notes", "data": {} }
  ],
  "edges": [{ "from": "n1", "to": "n0" }]
}
```

Loading prefers the JSON. If only the `.md` exists, the Mermaid block is parsed for node
ids and `-->` edges (node shapes and `|labels|` are tolerated) and laid out left to right.

## Shortcuts

| Keys | Action |
| --- | --- |
| cmd-s, ctrl-s | Save graph |
| cmd-shift-n, ctrl-shift-n | New graph |
| cmd-enter, ctrl-enter | Run workflow |
| cmd-n, ctrl-n | Add node of the selected type |
| Backspace, Delete | Delete selection |
| Escape | Clear canvas selection |
| cmd-q | Quit |

## Layout

| File | Role |
| --- | --- |
| `frontend/App.tsx` | Workspace header, searchable library, child navigation, run state and layout |
| `frontend/Canvas.tsx`, `frontend/graph.ts` | React Flow cards, ports, wires, view controls and pure graph edits |
| `frontend/Inspector.tsx` | Node configuration, validation, results, reviews and history |
| `frontend/api.ts`, `frontend/types.ts` | Typed Tauri IPC boundary and persisted document shapes |
| `frontend/*.test.ts`, `tests/` | Frontend unit tests and Playwright interaction tests with mocked IPC |
| `src-tauri/src/main.rs` | Tauri window, workspace selection and native IPC commands |
| `src-tauri/src/smoke.js` | Opt-in native webview/IPC/execution smoke test |
| `src-tauri/tauri.conf.json` | Vite integration, window configuration, content security policy and bundling |
| `src/lib.rs`, `src/app.rs` | UI-independent Rust core and application service for library, runs and reviews |
| `src/exec.rs` | The executor: upstream ordering, files, child processes, database tools, agent runs |
| `src/workflow.rs` | Run input definitions, reachable fields, typed values and validation |
| `src/calls.rs` | Workflow mappings, saved graph snapshots, recursion checks and call limits |
| `src/review.rs` | File snapshots, bounded diffs, review decisions and stale-file checks |
| `src/history.rs` | Run records, input values, output previews and history replay |
| `src/providers.rs` | Provider request builders and reply parsers; the HTTP client, plain and streamed |
| `.github/workflows/ci.yml`, `scripts/run-tests.sh` | Rust/frontend checks, builds and interaction tests |
| `src/node_data.rs` | The typed node content: configs, enums, summaries, validation |
| `src/validate.rs` | Sanitizers and validators: names, paths, env vars, identifiers, SQL |
| `src/library.rs` | The per-type directories: agents and outputs, prompts, databases, file listings |
| `src/doc.rs` | Graph file formats, Mermaid writer and parser, kind library |
| `src/storage.rs` | Bounded reads, managed directories, atomic replacements and exclusive new files |

The desktop shell uses Tauri 2. The frontend uses React, React Flow and Lucide icons,
with Vite, TypeScript, Vitest and Playwright for development and tests. The Rust core
uses `serde` and `serde_json`, `ureq` for HTTP, `url` for connection and endpoint URLs,
and `base64` for image replies.

The next pass will harden the TypeScript code, focusing on runtime validation at the IPC
boundary, asynchronous state handling and failure-path test coverage. Resumable batch
execution, per-item results, batch review and export remain deferred.
