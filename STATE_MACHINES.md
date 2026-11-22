# State machines in uapz

This file names every state machine the app relies on, the invariants each one keeps, and
the transitions that move it. Some are explicit Rust enums; most are implicit in a handful
of flags, refs and `Option`s. Unfixed gaps live in section 5 until resolved; accepted edge
cases live in section 5.1.

References name types, functions and methods in the source (for example `AppService::start`,
`Session::finish`, `prepareRun`). When a cited symbol is renamed or removed, update the
reference here in the same change.

Legend for the tables:

- **Guard**: the condition checked before the transition; a failed guard is an error or a
  no-op, as noted.
- **Effect**: what else changes, beyond the state itself.
- *Self* in the "To" column means the state does not change.

## 1. Map

| # | Machine | Lives in | Form | Thread or owner |
| --- | --- | --- | --- | --- |
| 2.1 | Service session slot | `AppService.current` (`src/app.rs`) | `Option<Arc<Mutex<Session>>>` | any IPC request |
| 2.2 | Session (one run) | `Session` (`src/app.rs`) | flags `running`, `cancel`, `review`, `run`, `error` | worker thread + IPC requests |
| 2.3 | Review decision | `review::Pending` and `review::wait` (`src/review.rs`) | one-shot channel | worker waits, IPC decides |
| 2.4 | Executor node | `Runner` (`src/exec.rs`) | `done` and `errors` maps, `failed`, `failures`, `reused` fields plus `NodeState` events | worker thread |
| 2.5 | Run record | `history::Run` (`src/history.rs`) | `Outcome` and per-node `State` enums | written by the worker, read by anyone |
| 2.6 | Output cache entry | `exec::Cache` (`src/exec.rs`) | in-memory map plus `.json`/`.bin` pair | behind one mutex |
| 2.7 | Child process | `run_process` (`src/exec.rs`) | local variables `exited`, `timed_out`, `cancelled` | worker thread |
| 2.8 | Provider stream | `generate_streaming` (`src/providers.rs`) | `Stream { stopped, error, text }` | worker thread |
| 2.9 | Desktop shell exit | `on_window_event` / `ExitRequested` (`src-tauri/src/main.rs`) | Tauri events | main thread |
| 3.1 | Document | `frontend/App.tsx` state `graph`, `dirty`, refs `documentEpoch`, `savedKey`, undo stacks, `children` | React state and refs | renderer |
| 3.2 | Run (client view) | `frontend/App.tsx` state `runtime`, `attachedRun`, `pendingRunId`, `form`, `shownRun`, `historical`; refs `preparingRun`, `startingRun`, `pendingRun`, `decidingReview`; `deriveRunView` (`frontend/runView.ts`); `RunInputForm` (`frontend/RunInputForm.tsx`); `ReviewPanel` (`frontend/ReviewPanel.tsx`) | React state, refs, and derived view | renderer |
| 3.3 | Runtime polling | `readRuntime` and `poll` (`frontend/App.tsx`) | monotonic request counters | renderer |
| 3.4 | Output retrieval | `NodeOutputView` (`frontend/NodeOutputView.tsx`), used in `auto` mode by `RunPanel` (`frontend/RunPanel.tsx`) and `manual` mode by `Inspector` (`frontend/Inspector.tsx`) and `PreviousOutput` (`frontend/inspector/PreviousOutput.tsx`) | `loaded`, `failure`, `attempt`, `fullChoice`, `busy`, `retrieval`, `identityRef`, `manualRequest` | renderer |
| 3.5 | Quit | `closeRequested` (`frontend/App.tsx`) | `closePrompt` ref plus `ConfirmDialog` (`frontend/dialogs.tsx`) | renderer |
| 3.6 | Confirmation dialog | `confirmation` state (`frontend/App.tsx`) and `ConfirmDialog` (`frontend/dialogs.tsx`) | single slot | renderer |

## 2. Backend

### 2.1 Service session slot

The service keeps at most one session: the run in progress, or the last one that finished.
A finished session stays in the slot so the renderer can read its live outputs and record
until the next run replaces it.

States:

| State | Meaning |
| --- | --- |
| Empty | No run has started since the process began, or the first spawn failed while the slot was Empty. |
| Occupied, running | The slot holds a session whose worker thread is alive. |
| Occupied, finished | The slot holds a session whose worker has called `finish`. |

Transitions:

| From | Trigger | Guard | To | Effect |
| --- | --- | --- | --- | --- |
| any | `start_run` (`AppService::start`) | no session is running (`AppService::start` running check), otherwise `a run is already active` | Occupied, running | New `Session` with every member node marked `pending`; worker thread spawned; the previous session is dropped, which sets its (already finished) cancel flag. |
| Occupied, running | spawn fails (`AppService::start`) | | previous slot (Occupied, finished or Empty) | The new session is discarded; the previous finished session is restored so its live outputs stay reachable. |
| Occupied, running | worker ends (`Session::finish`) | | Occupied, finished | See 2.2. |
| Occupied, finished or Empty | `clear_cache`, `clear_history`, `delete_graph`, `rename_graph` | `AppService::idle` holds the slot lock for the whole operation, so no run can start meanwhile | *Self* | |
| Occupied, running | the same four requests | | *Self* | Error `wait for the active run to finish`. |
| any | `runtime_state` | | *Self* | Serialises the session, or `null` when Empty. |
| any | the last `AppService` clone drops (`ServiceLifetime::drop`) | | *Self* | `cancel()` on the current session. |

Invariants:

- S1. At most one worker thread is alive per service. Enforced by the running check under
  the slot lock in `start`.
- S2. Lock order is always slot, then session (`request`, `idle`, `session()`,
  `cancel_active`). The worker and the progress callback take only the session lock.
  Nothing takes the cache lock while holding a session lock.
- S3. An `output` request never holds a session lock while reading the cache
  (`AppService::output` clones what it needs first).

### 2.2 Session

A session is the live view of one run. Its lifecycle is driven by the worker thread
(`progress`, `finish`) and by IPC requests (`cancel_run`, `review`).

States, as derived from the fields:

| State | `running` | `cancel` | `review` | `run` | `error` |
| --- | --- | --- | --- | --- | --- |
| Running | true | false | None | None | None |
| Awaiting review | true | false | Some | None | None |
| Cancelling | true | true | None | None | None |
| Finished, recorded | false | any | None | Some | None or the save error |
| Finished, crashed | false | any | None | None | `the execution worker stopped unexpectedly` |

Transitions:

| From | Trigger | Guard | To | Effect |
| --- | --- | --- | --- | --- |
| Running | file node needs review (`Runner::review_writes`, review callback in `AppService::start`) | running and not cancelling (`Session::publish_review`) | Awaiting review | `review_seq += 1`; `review = Some((id, pending))`. |
| Awaiting review | `review` request (`AppService::request` `"review"`) | running and not cancelling; id matches; channel accepts | Running | `review = None`; worker resumes with the decision. Errors: `this run is no longer awaiting review`, `this review is stale`, `this review has already been answered`, `there is no pending review`. |
| Awaiting review | worker returns from `wait` (review callback in `AppService::start`) | | Running or Cancelling | `review = None` (the request handler usually did this already). |
| Running or Awaiting review | `cancel_run` (`AppService::request` `"cancel_run"`) or `cancel_active` or `Session`/`ServiceLifetime` drop | `running`, otherwise `this run has already finished` | Cancelling | `cancel` flag set; a pending review is taken and answered `Reject` so the waiter wakes (`Session::cancel`). |
| Cancelling | `review` request | | *Self* | Error `this run is no longer awaiting review`. |
| any running state | worker ends normally | | Finished, recorded | `Session::finish(Some)`: marks lose `pending` and `running`; `log`, `workflows`, `run`, `error = save error`; `review = None`; `stream = None`. |
| any running state | worker panics | | Finished, crashed | `Session::finish(None)`: the same clean-up with `run = None` and the fixed error. |
| any finished state | `cancel_run` | | *Self* | Error `this run has already finished`. |
| any | `Progress::Node/Log/Text/Output/Workflow` (`Session::progress`) | only the worker sends these, so only while running | *Self* | Marks, bounded feed events, bounded log, stream tail, bounded live outputs. |

Derived value `cancelling` is `running && cancel` (`Session::value`), so it is false in both
finished states even though the flag stays set.

Invariants:

- R1. `review.is_some()` implies `running && !cancel`. Kept by `publish_review`, `finish`
  and `cancel`.
- R2. `cancelling` implies `running`, by construction of the derived value.
- R3. Once `running` is false, no mark is `pending` or `running`. Kept by `finish` on both
  the recorded and the crashed path.
- R4. `run.is_some()` implies `!running`.
- R5. `error.is_some() && run.is_none() && !running` identifies a crashed worker. The
  renderer treats it as terminal (see 3.2).
- R6. Feed event `seq` is strictly increasing; the renderer rejects anything else
  (`frontend/responses.ts`, the `runtime` schema).
- R7. A completed output replaces every earlier `text` event for the same node and clears
  `stream` when it was that node's (`Session::completed_output`).

### 2.3 Review decision

`review::wait` checks the cancel flag before publishing, then publishes a `Pending` holding
the sender of a `sync_channel(1)` and polls the receiver every 50 ms while watching the
cancel flag.

| From | Trigger | To | Result for the worker |
| --- | --- | --- | --- |
| Published | `Pending::decide(Apply)` succeeds and cancel is clear on receipt | Decided | `Ok(Apply)`; then `Batch::verify` must find every file unchanged, or the batch fails before the first write. |
| Published | `Pending::decide(Reject)` | Decided | `Ok(Reject)`, turned into `review::REJECTED`. |
| Published | cancel flag set (by `Session::cancel`, which also sends `Reject`) | Cancelled | `Err(CANCELLED)`, checked before and after receipt, so cancel wins over a decision already in the channel. |
| Published | every `Pending` clone dropped without a decision | Closed | `Err("the file review was closed")`. |
| Decided | a second `decide` | *Self* | `try_send` fails on the full channel: `false`, surfaced as `this review has already been answered`. |

Invariants:

- V1. Exactly one decision is ever delivered for one review; the channel capacity is one and
  the sender never blocks.
- V2. Nothing is written before `verify` passes, and `verify` runs after the decision, so a
  file changed during the wait stops the whole batch (`Runner::review_writes`).
- V3. Review ids are unique within a session (`{run_id}-review-{seq}`), so a decision for
  an earlier review cannot be applied to a later one.

### 2.4 Executor node

Per run, each node moves through the states below. `NodeState` is what the progress sink
sees; the `Runner` maps are the source of truth.

| State | In the runner | Progress event |
| --- | --- | --- |
| Unvisited | in none of the maps | none |
| Ordered | appears in a target's `Runner::execution_order` after `validate_node` | none |
| Reused | `done[id]`, `reused` contains id | `Reused` then `Output` |
| Running | own step started | `Running` |
| Done | `done[id]` | `Done` then `Output` |
| Failed | `errors[id]`, `failures` contains `(id, error)`, `failed` is the first such id | `Failed` |
| Blocked | `errors[id]` with the error of the upstream node or `CANCELLED` | none |

Transitions:

| From | Trigger | Guard | To |
| --- | --- | --- | --- |
| Unvisited | a target's walk reaches it | `validate_node` passes; `check_cancel` | Ordered |
| Unvisited | `validate_node` fails or the walk finds a cycle | | Failed (for the node the error names) and Blocked (for the target) |
| Ordered | cache has the key and `fresh` is false (`Runner::run_fresh`) | agent or script node only; other kinds always run | Reused |
| Ordered | cache-key setup or upstream lookup fails before step starts (`Runner::run_fresh`) | e.g. missing/oversized script file | Failed |
| Ordered | otherwise | `check_cancel` | Running |
| Running | step returns `Ok` | | Done; the output is stored in the cache when it has a key |
| Running | step returns `Err(e)`, `e != CANCELLED` | first failure of this node | Failed |
| Running | step returns `Err(CANCELLED)` | | Blocked (no `Failed` event; `Runner::record_failure`) |
| any | a later target asks for the node | `done` hit returns the earlier result; `errors` hit returns the earlier error | *Self* |
| Ordered or Unvisited | an earlier step in the same order failed | | Blocked (the target inherits the error) |

Run-level outcome (`run_prepared`): the result is the last target's output unless any
target failed, in which case it is the first error; a `CANCELLED` error stops the loop and
becomes the result (`run_prepared` cancel break).

Invariants:

- N1. A node's own step runs at most once per run (`done` and `errors` are consulted before
  every step).
- N2. `Failed` is emitted at most once per node and never for `CANCELLED`.
- N3. The set of ids in `Report.failures` equals the set of nodes for which `Failed` was
  emitted, and `failed` is `failures[0]` (`Runner::record_failure`).
- N4. Nothing is cached or saved after cancellation is observed: `run_agent` checks the
  flag after the reply and before `write_agent_output`; `run_process` returns `CANCELLED`
  before returning any bytes.
- N5. Workflow children inherit the parent's `Control`, so cancel, cache and `fresh` apply
  at every depth, and their progress ids are prefixed with the call path.

### 2.5 Run record

`history::record` turns a `Report` into a `Run`.

| Report | Record |
| --- | --- |
| `result: Ok` | `Outcome::Ok` |
| `result: Err(CANCELLED)` | `Outcome::Cancelled` |
| `result: Err(other)` | `Outcome::Failed { error }` |
| `outputs[id]`, id in `reused` | `nodes[id].state = Reused` |
| `outputs[id]`, otherwise | `nodes[id].state = Ran` |
| `(id, error)` in `failures` | `nodes[id].state = Failed`, `error` kept (`history::record` failures loop) |
| `failed` | `Run.failed`: the node that failed first, named by the headline and leading `Run::report().failures` |

File lifecycle: written once by the worker as `runs/<graph>/<id>.json` with a unique
suffix on collision; the oldest beyond 200 per graph are removed on each save (and after a
merging rename, on the next save); moved by `history::rename`; removed by `clear` and by
`delete_graph`.

Invariants:

- H1. `Outcome::Ok` implies no `Failed` node: any own-step failure makes the run result an
  error.
- H2. `Outcome::Failed` implies at least one `Failed` node once execution reaches a node
  step. Pre-execution failures (`Report::failure` on `workflow::fields`/`workflow::resolve`,
  empty targets, or a child workflow's dynamic input mapping error) carry no `Failed` nodes.
- H3. `Outcome::Cancelled` may carry `Failed` nodes: an earlier target's own failure is
  kept. Nodes stopped by the cancel itself are absent.
- H4. The `graph` field of a listed record equals the directory it was listed from
  (`history::list` overrides the stored field, which can be stale after a rename).
- H5. A record's node states match the session's final marks: `Ran`, `Reused` and `Failed`
  carry over one to one, and nodes without a final state are absent from both (R3).
- H6. `Run.failed` is the node that failed first, matching the outcome's error when
  `outcome` is `Outcome::Failed` (a `Cancelled` run keeps the first earlier failure in
  `Run.failed` too); the order of any further failures is not stored. Records from before
  the field existed fall back to the first failed node by id.

### 2.6 Output cache entry

| State | Meaning |
| --- | --- |
| Absent | no entry under the key |
| In memory | `entries[cache_id] = (key, output)` for this process |
| On disk | `<key>.json` plus `<key>.bin` under `cache/` |

| From | Trigger | To |
| --- | --- | --- |
| Absent | `Cache::store` after a step | In memory and On disk (disk failure costs persistence only) |
| Absent | `Cache::keep` from `record_node` for outputs up to 8 MB | On disk |
| On disk | `keep` again | *Self*, modification time touched so pruning treats it as recent |
| On disk | `Cache::lookup` with a matching key | In memory and On disk |
| On disk | `Cache::prune` while the directory exceeds 1 GB, oldest first | Absent on disk; memory entries stay valid |
| any | `Cache::clear` | Absent |
| On disk | a read finds a size or checksum mismatch | treated as Absent |

Invariants:

- C1. Bytes never reach a downstream node without matching the stored checksum and length.
- C2. The cache directory is refused when it is a symlink (`storage::directory`).
- C3. A node is reused only when its key, which covers its configuration and every
  upstream output, matches; data, directory, database, file, input and workflow nodes have
  no key.

### 2.7 Child process

`run_process` owns one child and three pipe threads.

| From | Trigger | To | Result |
| --- | --- | --- | --- |
| Spawned | `spawn` fails | | `Err` naming the program |
| Running | child exits and all three pipe threads finish | Finished | `Ok(Finished { timed_out: false })` |
| Running or Exited with open pipes | cancel flag set | Killed | process group killed; `Err(CANCELLED)` (`run_process` cancel path) |
| Running or Exited with open pipes | deadline passes | Killed | process group killed; `Ok(Finished { timed_out: true })`, which the caller turns into a timeout error |
| any | a stream passes 64 MB | Finished | `Err` naming the cap |

Invariant P1: the deadline and the cancel flag stay active until every pipe end is closed,
so a grandchild holding the pipe cannot keep the run alive.

### 2.8 Provider stream

| From | Trigger | To | Result |
| --- | --- | --- | --- |
| Streaming | `on_text` returns false (`Stream::feed`) | Stopped | `Err(STOPPED)`; `run_agent` maps it to `CANCELLED` because the callback returns false only when the cancel flag is set |
| Streaming | an event carries an error | Error | `Err(message)` |
| Streaming | the reply ends with no streamed text or with status 400 or above | Fallback | parsed as one whole reply |
| Streaming | the reply ends | Finished | text validated for the output type |

### 2.9 Desktop shell exit

| From | Trigger | To | Effect |
| --- | --- | --- | --- |
| Open | window close button (`on_window_event` `CloseRequested`) | Close requested | `prevent_close`; `close-requested` emitted to the renderer |
| Open | quit from the menu or dock, `code == None` (`ExitRequested`) | Close requested | `prevent_exit`; `close-requested` emitted |
| Close requested | renderer calls `quit` | Exiting | `exit(0)` carries a code, so the handler lets it through |
| Exiting | `ExitRequested` with a code | Exited | `cancel_active`; wait up to 2 s for `running` to clear so child processes die |

In smoke mode the window closes without asking.

## 3. Frontend

### 3.1 Document

| Field | Role |
| --- | --- |
| `documentEpoch` ref | Incremented by `newGraph` and `openGraph`; every async result compares its captured epoch before touching state. |
| `graph`, `graphRef` | The open document; the ref is updated synchronously by `updateGraph`, `restore`, `renameGraph` and on every render. |
| `dirty`, `savedKey` | `savedKey` is the serialised document minus `view` at the last save; `dirty` is cleared by a save whose document is still current. |
| `undoStack`, `redoStack` | Snapshots; edits to one node within 800 ms coalesce. |
| `children` | Read-only views of child graphs (saved or run snapshots), at most 8 deep. |

| From | Trigger | Guard | To | Effect |
| --- | --- | --- | --- | --- |
| any | `updateGraph` / `updateNode` | not read only; same epoch | Dirty | undo entry pushed; redo cleared |
| any | `save` | not read only; `saveInFlight` false | Clean if no edit landed during the request, else Dirty | `savedKey` set only when the document is unchanged; library refreshed |
| any | `newGraph` / `openGraph` | not `occupied`; no run is being prepared, started, pending or running; `discardDraft` agreed; navigation counter unchanged | Clean | epoch incremented; history, selection, children, shown run, `historical` and attached run reset |
| any | `deleteGraph` of the open graph | not `occupied`; no save in flight | Dirty | `savedKey = null`; `history`, `shownRun`, `attachedRun`, `historical` and `remembered` cleared |
| any | `renameGraph` of the open graph | not `occupied`; no save in flight | *Self* | name replaced; `savedKey` follows only when clean; `shownRun`, `history` and `remembered.origins` are re-keyed to the new name |
| any | `duplicateGraph` | not `occupied`; no save in flight | *Self* | copy written under the new name |
| any | undo / redo | not read only | Dirty or Clean by comparison with `savedKey` | view kept |
| any | `openChild` | depth under 8; navigation counters unchanged | child pushed | snapshot when a run is shown, saved graph otherwise |

Invariants:

- D1. A response from a previous epoch never changes the current document (checked in
  `action`, `refreshHistory`, `save`, `prepareRun`, `openChild`, `usePrompt`, `clear_history`,
  `installExamples`, and in `openGraph` via `rootNavigation`, which subsumes epoch changes).
- D2. `dirty` is true whenever the document differs from `savedKey`; `restore` recomputes it.
- D3. Read-only children never reach `updateGraph` or `updateNode`.

### 3.2 Run, as the renderer sees it

Derived values: in `App`, `running` is `runtime.running || pendingRunId` and `occupied` is
`running || form || busy`; `deriveRunView` (`frontend/runView.ts`) derives `runMatches`
(`runtime.run_id === attachedRun`), `currentRun` (`shownRun`, else the attached runtime's
record), `snapshotContext`, `captured`, `childRun`, `outputRun`, `marks`, `liveLog`,
`liveStream` and `outputContext`. `RunInputForm` (`frontend/RunInputForm.tsx`) renders
`form`, and `ReviewPanel` (`frontend/ReviewPanel.tsx`) renders `runtime.review` (resetting
its `fileIndex` whenever `review.id` changes).

| State | Signals |
| --- | --- |
| Idle | no form, not running, `preparingRun` and `startingRun` false |
| Preparing | `preparingRun` true, `busy` true |
| Form | `form` set |
| Starting | `startingRun` true, `busy` true |
| Pending | `pendingRun` holds a run id the poll has not yet confirmed |
| Running | `runtime.running` and `runMatches` |
| Awaiting review | Running and `runtime.review` |
| Deciding | Awaiting review and `decidingReview` true |
| Cancelling | Running and `runtime.cancelling` |
| Completed | `lastCompleted === attachedRun`; `shownRun` holds the record |
| Historical | `historical` true; `shownRun` is a record chosen from the list |

| From | Trigger | Guard | To | Effect |
| --- | --- | --- | --- | --- |
| Idle | Run button, Cmd+Enter, node Run (`prepareRun`) | not read only, not `occupied`, none of the three run refs set | Preparing | root navigation counter bumped so an in-flight open cannot land |
| Preparing | `prepare_run` returns | same epoch; document unchanged, else error `The workflow changed while preparing` | Form when fields exist, else Starting | last run's inputs prefilled by field name |
| Form | Close run inputs (`setForm(null)`) | | Idle | form dismissed |
| Form | Start run (`startRun`) | `startingRun`, `pendingRun`, `runtime.running` all clear | Starting | |
| Starting | `start_run` fails | | Form with error when fields exist, else Idle with the error | |
| Starting | `start_run` fails but `runtime_state` already shows this graph running | | Pending / Running | attach to that session (lost reply); form cleared |
| Starting | `start_run` returns an id | | Pending | `attachedRun`, panel, notice set; form cleared; one status read follows |
| Idle, Completed, Form or Historical | poll sees a running session for the open graph (`poll` orphan attach) | not already attached; no `pendingRun` or `startingRun` | Running | `attachedRun` set; form and `historical` cleared; run panel shown |
| Pending | a status read returns that id (`readRuntime`) | | Running | `pendingRun` cleared |
| Running | poll sees `review` | | Awaiting review | |
| Awaiting review | Apply or Reject (`reviewDecision`) | `decidingReview` false | Deciding | buttons disabled until the service's answer is read |
| Deciding | request settles | | Running (or Completed when the read shows it) | `decidingReview` cleared |
| Pending, Running, Awaiting review or Deciding | Cancel run | | Cancelling | notice `Cancelling...` |
| Running, Awaiting review, Cancelling, Pending | poll sees `!running` with a record or an error (`poll` completion check) | `run_id === attachedRun` and not yet completed | Completed | record remembered per node; history refreshed; notice shown unless Historical; a service error is shown whichever run is on display |
| Completed, Idle or Historical | Inspect this run (`onOpenHistory`) | not `occupied` | Historical | run panel shows the chosen record |
| Historical | a new run starts, or `newGraph` / `openGraph` | | Idle then Pending, or Idle | `historical` cleared |
| Completed or Historical | Clear history | not `occupied`; same epoch | Idle | `attachedRun`, `shownRun`, `historical`, `history`, remembered outputs cleared |

Invariants:

- F1. One `start_run` per user intent: `preparingRun`, `startingRun` and `pendingRun` each
  block a second entry, and a failed first status read keeps `pendingRun` so the same run
  cannot be resubmitted. A lost `start_run` reply still attaches via the live runtime or
  the next poll so the run is not left unowned.
- F2. One decision per review: `decidingReview` blocks the second click while a decision
  is in flight.
- F3. Navigation, rename, delete, duplicate, Clear cache and Clear history are disabled
  while `occupied` (the service's `idle()` guard protects `clear_cache`, `clear_history`,
  `delete_graph` and `rename_graph`). Rename, delete and duplicate are also disabled while
  a save is in flight (`mutateDisabled` on `LibraryRow` in `frontend/LibraryPanel.tsx`) so a
  landing write cannot resurrect the old name; opening another graph during that save stays
  allowed. Save is allowed during a run on purpose: the run uses the document captured at
  start.
- F4. Completion is detected once per attached run, whether or not a record exists.

### 3.3 Runtime polling

`readRuntime` numbers every status read. A result is applied only when its number is not
below the last applied one and the caller's `isCurrent` still holds; an error from an
older read is swallowed. The poll runs every 150 ms while running, every second otherwise,
and every 150 ms after a result it could not apply.

| Situation | Outcome |
| --- | --- |
| Poll result arrives after a review decision's own read | discarded, so a finished run cannot look like it is awaiting review again |
| Poll fails before any read applied | error shown, poll continues |
| Poll fails after a later read applied | ignored |
| First result of the session shows a run in progress with no document opened | the run's graph is loaded as a dirty document and the run attached |
| First result of the session shows nothing running | the last opened graph is restored from local storage (`readLayout` in `frontend/layout.ts`) |

### 3.4 Output retrieval

`NodeOutputView` (`frontend/NodeOutputView.tsx`) handles output retrieval for both the run
panel (`RunPanel` in `frontend/RunPanel.tsx`, `loadMode="auto"`) and the inspector
(`Inspector` in `frontend/Inspector.tsx` and `PreviousOutput` in
`frontend/inspector/PreviousOutput.tsx`, `loadMode="manual"`). Its `identity` covers
`context` (`graph`, `runId`, `callPath`, `node`), `outputName`, `result.key`,
`result.preview`, `result.label`, `result.error` and `result.complete`.

- `auto` mode fetches the output when `!result.error && context.runId` and the preview is
  incomplete or missing, capped at `AUTOMATIC_OUTPUT_BYTES` (256 KB); `retrieval` shares
  one in-flight promise across Strict Mode effect replays. States: idle, loading, loaded,
  failed; **Retry output** increments `attempt` and re-issues the request; **Load full
  output** sets `fullChoice` to `identity` and drops the 256 KB limit.
- `manual` mode loads on request via **View full output** (`loadManual`, without a byte
  cap) while tracking `busy`; an identity change clears `loaded`, `busy`, `failure` and
  `note`.

Invariant O1: a response is kept only when its request identity still matches (effect
cleanup `active` flag plus `loaded`/`failure` identity matching in `auto` mode;
`manualRequest` counter and `identityRef` check in `manual` mode), so a result for another
run, node, or superseded request is never shown.

### 3.5 Quit

`closeRequested` answers the shell's `close-requested` event. Here "run active" covers
`runtime.running || pendingRun || startingRun`.

| `dirty` | run active | Behaviour |
| --- | --- | --- |
| false | false | `quit` at once |
| false | true | ask: `A run is in progress. Quitting cancels it.` with **Quit and cancel the run** |
| true | false | ask: unsaved changes, with **Save and quit** unless read only |
| true | true | ask: unsaved changes and a run in progress, with **Save and quit** unless read only |

If **Save and quit** is chosen while `saveInFlight` is already true, `closeRequested`
refuses to quit early and shows `Wait for the current save to finish before quitting.`.
`closePrompt` ignores further close requests while the question is open.

### 3.6 Confirmation dialog

One slot. A second `choose` while a dialog is open replaces it; the first promise then
never settles. Other callers have already bumped their navigation counter or hold no state,
so a dangling promise is harmless, except `closeRequested`, which holds `closePrompt` until
its dialog settles; `installExamples` sets `busy` and checks `closePrompt` before calling
`openGraph` so an async completion cannot replace an open quit dialog. Keep callers
side-effect free until the dialog answers.

## 4. Cross-machine invariants

| # | Invariant | Where enforced |
| --- | --- | --- |
| X1 | The renderer's `running` is never true for a run the service has finished for longer than one poll interval. | 2.2 R4 and 3.2 F4 |
| X2 | A review panel is shown only for a review the service still holds (up to the post-decision status read, while decision buttons stay disabled). | R1 plus the status read after every decision |
| X3 | Live marks and the finished record agree on every node's final state. | N3, H5 and R3 |
| X4 | A record is reachable by the graph name it reports. | H4, `renameGraph` and `deleteGraph` |
| X5 | Nothing is written to disk by a reviewed file node (`review: true`) without a decision for exactly that batch and an unchanged-file check after it. | V1 and V2 |
| X6 | A cancel reaches every blocking point within 50 ms (review wait) or 25 ms (process poll), and in-flight provider streams at the next chunk. | 2.3, 2.7, 2.8 |
| X7 | Quitting never cancels a run without the person agreeing. | 3.5 and 2.9 |

## 5. Open findings

None. Resolved issues are reflected in the tables and invariants above and pinned by
regression tests in section 6.

### 5.1 Reachable but left as is

These are real but low impact, or the alternative would cost an intended behaviour. Each
is listed so a later change does not mistake them for oversights.

| Situation | What happens | Why unchanged |
| --- | --- | --- |
| Live outputs over 4 MB in total while a run is still going | `output` answers `no longer stored` for the evicted node until the record exists; **Retry output** then works. | Bound is deliberate; the record recovers the output. |
| A history run is inspected in the gap between a review decision and the completion poll | The completion notice is suppressed to keep the chosen run on screen; results and history are still recorded, and a service error is still shown. | Intended, pinned by `tests/run-history.spec.ts`. |
| Undo after renaming the open graph | The old name comes back; saving recreates the old file. | Undo restores documents, and the name is part of the document. |
| Two targets fail; the record is reopened | `Run.failed` names the first to fail; the order of the others is not stored. | Only the first matters to the outcome's error and the headline. |
| Finished `Session` still in `AppService.current` after `clear_history`, `delete_graph` or `rename_graph` | A direct `output` request for that last run id under the old name still hits memory until the next run replaces the slot; after `rename_graph`, if `history::save` had failed so no file exists on disk, lookups under the new name miss the in-memory session. | The renderer clears or re-keys its own state; disk records stay reachable under the new name via `history::list`. |
| Lost `start_run` reply for a run that finishes before `runtime_state` is read | `startRun` only attaches when `runtime_state` still shows `running: true`; a sub-millisecond run that already finished falls through to the `start_run` error. | Without the returned `run_id`, the renderer cannot distinguish a newly finished run from an earlier finished session. |
| `updateNode` called in the same tick as `save` or `prepareRun` resolving | `updateNode` relies on the next render to update `graphRef`, unlike `updateGraph`, `restore` and `renameGraph`. | Single-microtask window before React commits; normal edits commit before IPC settles. |
| Replacing one text-input `ConfirmDialog` (`frontend/dialogs.tsx`) directly with another | `ConfirmDialog` has no `key`, so its local input `value` would not reset to the new `input.initial`. | Modal `<dialog>` prevents opening a second text-input dialog while one is open; only `closeRequested` (no input) can replace it. |

### 5.2 Checked and sound

Paths that looked suspicious and held up, so they need not be re-derived:

- Cancel racing a decision: `Session::cancel` sets the flag before answering `Reject`;
  `wait` rechecks the flag after receipt, so cancel always wins.
- Stale status reads: a poll result numbered below the last applied read is dropped, so
  a finished run never shows as awaiting review again (`tests/async-actions.spec.ts`,
  "a late runtime poll snapshot/failure cannot override a newer completed status").
- Lock order: slot before session everywhere; the cache lock is never nested inside a
  session lock, and the worker takes only the session lock.
- `Create` mode write: `create_new` at open time, so a file created between review and
  write is never truncated.
- Progress ids for children are prefixed at every depth, so marks and outputs of nested
  calls cannot collide with the root's.

## 6. Test map

| Transition or invariant | Test |
| --- | --- |
| S1 second start refused; V3 stale ids; V1 duplicate decision | `app::tests::pending_review_blocks_second_run_and_rejects_stale_decisions`, `app::tests::sequential_reviews_enforce_unique_ids_and_all_review_guards`, `review::tests::second_decision_returns_false_and_first_decision_wins` |
| 2.1 idle-guarded mutations (`clear_cache`, `clear_history`, `delete_graph`, `rename_graph`) | `app::tests::active_run_blocks_all_idle_mutations_until_finished` |
| 2.1 spawn failure restores previous or Empty | `app::tests::a_spawn_failure_keeps_the_previous_finished_session`, `app::tests::spawn_failure_on_empty_slot_leaves_service_empty` |
| Cancel wakes a review; `Cancelled` record | `app::tests::cancellation_wakes_review_and_retains_cancelled_history`, `review::tests::cancellation_wakes_a_waiting_review_and_wins_over_approval` |
| R1 cancelled session never publishes review | `review::tests::cancelled_wait_does_not_publish_a_pending_review`, `app::tests::a_cancelled_session_never_publishes_a_review` |
| R2 cancelling state, R3 recorded marks cleanup, H5, X3 | `app::tests::cancelling_state_rejects_reviews_and_recorded_finish_clears_pending_marks`, `tests/run-lifecycle.spec.ts` "cancelling a running workflow shows Cancelling state until the worker finishes" |
| R3 and R5 on the crashed path | `app::tests::a_crashed_worker_leaves_no_pending_or_running_marks` |
| 2.2 finished with save error | `app::tests::save_error_records_run_and_surfaces_error_in_finished_session`, `tests/run-lifecycle.spec.ts` "a finished run with a save error surfaces the error even while inspecting history" |
| H2, H3, H6 `Run.failed`, legacy fallback, merging rename prune | `history::tests::first_failed_leads_report_headline_and_legacy_records_fall_back`, `history::tests::merging_rename_prunes_beyond_max_runs_on_next_save` |
| H4, X4 | `history::tests::renamed_records_report_the_directory_they_are_listed_from`, `app::tests::history_outputs_stay_reachable_under_a_renamed_graph`, `tests/run-lifecycle.spec.ts` "renaming the open workflow re-keys its remembered runs so outputs are requested under the new name", "deleting the open workflow clears its history and remembered outputs" |
| N1, N2, N3, H3, H5 | `exec::tests::independent_failures_each_get_a_failed_state`, `exec::tests::cancellation_takes_precedence_over_an_earlier_target_failure`, `exec::tests::shared_failing_upstream_runs_once_and_emits_failed_once`, `exec::tests::cache_key_setup_failure_records_failed_state_before_running` |
| N4 | `exec::tests::cancellation_after_a_whole_reply_prevents_saving_and_caching`, `exec::tests::agents_stream_text_into_progress_and_stop_on_cancel` |
| 2.6 cache `keep`, `prune`, C1, C2, C3 | `exec::tests::cache_keep_enforces_size_cap_touches_mtime_and_prunes_oldest_keeping_memory`, `exec::tests::non_cacheable_nodes_always_run_even_with_warm_cache`, `exec::tests::corrupt_cache_bytes_are_misses_and_history_can_repair_them`, `exec::tests::cache_never_reads_writes_or_clears_a_symlinked_directory` |
| 2.7 spawn error, P1 | `exec::tests::run_process_reports_missing_program`, `exec::tests::process_timeout_covers_pipes_inherited_by_grandchildren`, `exec::tests::cancellation_covers_pipes_inherited_by_grandchildren` |
| V2, `Create` mode | `exec::tests::a_stale_file_stops_an_approved_batch_before_its_first_write`, `review::tests::create_mode_rejects_existing_file_during_prepare` |
| R6, R7 feed bounds and multi-node stream preservation | `app::tests::feed_preserves_order_coalesces_text_and_replaces_completed_streams`, `app::tests::feed_retention_is_bounded_and_flags_utf8_truncation_and_real_evictions`, `app::tests::completed_output_preserves_other_nodes_stream_and_text_events` |
| 3.1 rename dirty vs clean, 8-level child depth | `tests/async-workspace.spec.ts` "renaming a dirty open workflow keeps it unsaved while renaming a clean one stays saved", "nested workflow navigation stops at 8 levels" |
| 3.2 derived run view (`deriveRunView`) | `frontend/runView.test.ts` |
| F1, no-fields start, orphan attach | `tests/async-actions.spec.ts` "preparing a run blocks repeated execution and root navigation until its inputs are ready", "an accepted run cannot be resubmitted when its first status response is error/null", "a lost start_run reply still attaches to the service session that accepted it"; `tests/run-lifecycle.spec.ts` "workflows without input fields start immediately and return to idle on start failure", "polling attaches to an unowned active run for the open graph" |
| F2 | `tests/run-lifecycle.spec.ts` "a review decision is sent once however often its button is pressed while it is in flight" |
| F3 save-during-run, save-vs-mutate and clear history | `tests/async-workspace.spec.ts` "rename, delete and duplicate wait for an in-flight save, while open stays available"; `tests/run-lifecycle.spec.ts` "save stays available during a run while navigation and destructive actions wait", "clearing history waits while occupied and exits historical view cleanly" |
| F4, X1 | `tests/run-lifecycle.spec.ts` "a run whose worker stopped without a record is reported as failed and releases the editor" |
| 3.3 stale poll, active session on launch | `tests/async-actions.spec.ts` "a late runtime poll snapshot/failure cannot override a newer completed status"; `tests/run-lifecycle.spec.ts` "launching with an active service session loads its graph as dirty and attaches" |
| 3.5, 3.6, X7 | `tests/run-lifecycle.spec.ts` "a saved, idle workflow quits at once", "quitting while a run is in progress asks first, even with nothing unsaved", "quitting while start_run is in flight asks before cancelling", "quitting while an accepted run is still pending asks before cancelling", "quitting handles unsaved idle, unsaved active, read-only child, and save failures", "save and quit refuses to quit while another save is already in flight", "an in-flight install_examples completion does not replace an open quit dialog"; `tests/session.spec.ts` "leaving unsaved changes offers to save them first" |
| D1 | `tests/async-workspace.spec.ts`, `tests/async-actions.spec.ts` prompt and save tests |
| 3.4, O1 | `tests/run-panel.spec.ts` "historical runs expose all results and failed full-output loads can be retried", "switching historical runs discards a late RunOutput response from the previous run", "live runtime polls keep a failed output alert and Retry button mounted without flicker"; `tests/outputs.spec.ts` "late output responses cannot appear under the next selected node", "returning to a node discards an earlier in-flight manual output request" |

## 7. Keeping this file current

When a change adds a flag, an `Option`, a ref that gates an action, or a new error string
on an IPC request, it has changed one of these machines. Add the row, state the invariant
it protects, and point the regression test at it here. Cite symbols by name, not by line.
A finding that is fixed moves out of section 5 into the tables above with its regression
test in section 6; a finding that is accepted goes into 5.1 with the reason.
