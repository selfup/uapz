# State machines in uapz

This file names every state machine the app relies on, the invariants each one keeps, and
the transitions that move it. Some are explicit Rust enums; most are implicit in a handful
of flags, refs and `Option`s. Where the code could reach a state the machine should not
allow, the finding is recorded in section 5 with the evidence and the fix.

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
| 3.1 | Document | `App.tsx` state `graph`, `dirty`, refs `documentEpoch`, `savedKey`, undo stacks, `children` | React state and refs | renderer |
| 3.2 | Run (client view) | `App.tsx` state `runtime`, `attachedRun`, `pendingRunId`, `form`, `shownRun`, `historical`; refs `preparingRun`, `startingRun`, `pendingRun`, `decidingReview` | React state and refs | renderer |
| 3.3 | Runtime polling | `readRuntime` and `poll` (`frontend/App.tsx`) | monotonic request counters | renderer |
| 3.4 | Output retrieval | `RunOutput` (`frontend/RunPanel.tsx`); `Output` (`frontend/Inspector.tsx`) | `loaded`, `failure`, `attempt` | renderer |
| 3.5 | Quit | `closeRequested` (`frontend/App.tsx`) | `closePrompt` ref plus the dialog | renderer |
| 3.6 | Confirmation dialog | `confirmation` state (`frontend/App.tsx`) | single slot | renderer |

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
  and `cancel` (see finding K).
- R2. `cancelling` implies `running`, by construction of the derived value.
- R3. Once `running` is false, no mark is `pending` or `running`. Kept by `finish` on both
  the recorded and the crashed path (before this change, only on the recorded path; see
  finding C).
- R4. `run.is_some()` implies `!running`.
- R5. `error.is_some() && run.is_none() && !running` identifies a crashed worker. The
  renderer treats it as terminal (see 3.2 and finding C).
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
  emitted, and `failed` is `failures[0]` (`Runner::record_failure`). Before this change the
  report carried only `failed`; see finding B.
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
- H3. `Outcome::Cancelled` may carry `Failed` nodes: an earlier target's own failure is kept
  (see finding B). Nodes stopped by the cancel itself are absent.
- H4. The `graph` field of a listed record equals the directory it was listed from
  (`history::list`). The stored field can be stale after a rename; see finding A.
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
| any | `deleteGraph` of the open graph | not `occupied`; no save in flight | Dirty | `savedKey = null`; `history`, `shownRun`, `attachedRun`, `historical` and `remembered` cleared (finding J) |
| any | `renameGraph` of the open graph | not `occupied`; no save in flight | *Self* | name replaced; `savedKey` follows only when clean; the shown run, the history list and the remembered per-node runs are re-keyed to the new name (finding A) |
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

Derived values in `App`: `running` is `runtime.running || pendingRunId`, `occupied` is
`running || form || busy`, `runMatches` is `runtime.run_id === attachedRun`, `currentRun`
is `shownRun`, else the attached runtime's record.

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
| Completed or Historical | Clear history | not `occupied`; same epoch | Idle | `attachedRun`, `shownRun`, `historical`, `history`, remembered outputs cleared (finding I) |

Invariants:

- F1. One `start_run` per user intent: `preparingRun`, `startingRun` and `pendingRun` each
  block a second entry, and a failed first status read keeps `pendingRun` so the same run
  cannot be resubmitted. A lost `start_run` reply still attaches via the live runtime or
  the next poll so the run is not left unowned.
- F2. One decision per review: `decidingReview` blocks the second click (finding D).
- F3. Navigation, rename, delete, duplicate, Clear cache and Clear history are disabled
  while `occupied` (the service's `idle()` guard protects `clear_cache`, `clear_history`,
  `delete_graph` and `rename_graph`; see finding I). Rename, delete and duplicate are also
  disabled while a save is in flight so a landing write cannot resurrect the old name;
  opening another graph during that save stays allowed. Save is allowed during a run on
  purpose: the run uses the document captured at start.
- F4. Completion is detected once per attached run, whether or not a record exists
  (finding C).

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
| First result of the session shows nothing running | the last opened graph is restored from local storage |

### 3.4 Output retrieval

`RunOutput` fetches the full output when the record's preview is incomplete or missing.
Its identity covers the context (graph, run id, node path), the output name, the key, the
preview and the completeness; a change in any resets the loaded state. States: idle,
loading, loaded, failed; **Retry output** increments `attempt` and re-issues the request;
**Load full output** drops the 256 KB limit. The inspector's `Output` is the manual
counterpart: **View full output** loads on request and a changed identity discards the
result.

Invariant O1: a response is kept only when its request identity still matches, so a result
for another run or node is never shown.

### 3.5 Quit

`closeRequested` answers the shell's `close-requested` event. Here "run active" covers
`runtime.running || pendingRun || startingRun` (finding L).

| `dirty` | run active | Behaviour |
| --- | --- | --- |
| false | false | `quit` at once |
| false | true | ask: `A run is in progress. Quitting cancels it.` with **Quit and cancel the run** |
| true | false | ask: unsaved changes, with **Save and quit** unless read only |
| true | true | ask: unsaved changes and a run in progress, with **Save and quit** unless read only |

If **Save and quit** is chosen while `saveInFlight` is already true, `closeRequested`
refuses to quit early and shows `Wait for the current save to finish before quitting.`
(finding L). `closePrompt` ignores further close requests while the question is open.
Before finding E, the clean-and-running row quit silently.

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
| X4 | A record is reachable by the graph name it reports. | H4 and finding J |
| X5 | Nothing is written to disk by a reviewed file node (`review: true`) without a decision for exactly that batch and an unchanged-file check after it. | V1 and V2 |
| X6 | A cancel reaches every blocking point within 50 ms (review wait) or 25 ms (process poll), and in-flight provider streams at the next chunk. | 2.3, 2.7, 2.8 |
| X7 | Quitting never cancels a run without the person agreeing. | 3.5 and 2.9 |

## 5. Findings

Each finding names the invalid state or transition, how the code reached it, the evidence,
and the fix. Issues are ordered by impact. All below are fixed and pinned by the tests
named.

### A. Renamed graph: records unreachable by the name they carry (high)

- **Invalid state**: a listed `Run` whose `graph` field names a directory that no longer
  exists (violates H4, X4).
- **Reached by**: `rename_graph` moves `runs/<old>/*.json` to `runs/<new>/` without
  rewriting the files. `history::list` returned the stored `graph`. The renderer uses that
  field for every `output` request (`RunPanel.tsx`, `Inspector.tsx` history tab), and
  `AppService::output` looks the run up under it, so after a rename every truncated output
  of every earlier run answered `the run or completed live output is no longer stored`.
  Once a later run replaced the live session the error was permanent.
- **Fix**: `history::list` sets `run.graph` to the directory it listed. Existing
  workspaces are repaired on read; `rename` needs no change. The renderer keeps its own
  copies of runs (`shownRun`, `history`, `remembered.origins`), all belonging to the open
  document, and re-keys them when that document is renamed (`renameGraph`); otherwise the
  inspector would keep asking under the old name once a later run had replaced the live
  session.
- **Tests**: `history::tests::renamed_records_report_the_directory_they_are_listed_from`,
  `app::tests::history_outputs_stay_reachable_under_a_renamed_graph`,
  `tests/run-lifecycle.spec.ts` "renaming the open workflow re-keys its remembered runs".
  The Rust tests fail without the fix.

### B. Record under-reports failures (medium)

- **Invalid state**: a node shown as `failed` on the canvas during the run that has no
  entry in the record afterwards (violates X3, H5).
- **Reached by**: two paths. Run all with two independent failing targets: `Report.failed`
  held only the first, so `history::record` wrote one `Failed` node; the second node's
  live `failed` mark disappeared when the run ended. And a cancel after a target had
  failed: the outcome became `Cancelled` and the failed node was dropped altogether
  (`exec::tests::independent_failures_each_get_a_failed_state` already proved both
  `Failed` events were emitted).
- **Fix**: `Report.failures` records every own-step failure with its error
  (`Runner::record_failure`); `history::record` writes each as a `Failed` node regardless
  of outcome and stores `Report.failed` as `Run.failed`, so the headline and `Run::report`
  name the node that failed first rather than the first by id (`history::record` and
  `Run::first_failed`). Records written before `failed` existed fall back to the first
  failed node by id.
- **Tests**: `exec::tests::independent_failures_each_get_a_failed_state` and
  `exec::tests::cancellation_takes_precedence_over_an_earlier_target_failure` now assert
  the record; the history `report()` helper derives `failures` the way the executor does.

### C. Crashed worker leaves the run half finished (medium)

- **Invalid states**: service side, `running == false` with marks still `pending` or
  `running` (violates R3). Renderer side, the attached run never reaches Completed: the
  status bar kept `Workflow is running`, the error was never shown, history was not
  refreshed (violates F4, X1).
- **Reached by**: the worker's `catch_unwind` path set `running = false` and `error` but
  skipped the clean-up that lived in the success closure; the renderer's completion check
  required `state.run`, which a crashed worker never produces.
- **Fix**: `Session::finish(Option<..>)` is the single exit for both paths; the poll treats
  `!running && (run || error)` as terminal (`poll` completion check) and reports
  `Run failed` with the error. The error is shown even while a history run is on display:
  it belongs to the run that ended, not to the one shown.
- **Tests**: `app::tests::a_crashed_worker_leaves_no_pending_or_running_marks`;
  `tests/run-lifecycle.spec.ts`, "a run whose worker stopped without a record".

### D. A review decision could be sent twice (medium)

- **Invalid transition**: Awaiting review -> Deciding -> Deciding again for the same review.
- **Reached by**: the panel stayed enabled until the status read after the first decision
  returned. A second click in that interval sent a second `review` request, which the
  service refused with `there is no pending review`, and the renderer showed that as an
  error although the decision had been applied. The interval grows with the size of the
  status payload the poll is serialising at the time.
- **Fix**: `decidingReview` and the `deciding` state disable Apply and Reject while a
  decision is in flight (`reviewDecision`); Cancel run stays available.
- **Tests**: `tests/run-lifecycle.spec.ts`, "a review decision is sent once however often its button is pressed while it is in flight"; the
  service's own refusal of a duplicate was already pinned by
  `app::tests::pending_review_blocks_second_run_and_rejects_stale_decisions`.

### E. Quit during a run with a clean document cancelled it silently (medium)

- **Invalid transition**: Open -> Exited with a run in progress and no confirmation
  (violates X7). The existing dialog text already said quitting cancels the run, but the
  question was only asked when the document was dirty.
- **Reached by**: Cmd+Q or the close button after saving while an agent run was still going.
- **Fix**: the prompt now covers the clean-and-running case (`closeRequested`).
- **Tests**: `tests/run-lifecycle.spec.ts`, "a saved, idle workflow quits at once" and
  "quitting while a run is in progress asks first, even with nothing unsaved". The Playwright fixture now emulates
  the shell's event listener registration and the `quit` command
  (`tests/support/workspace.ts`), so `__testState.emit("close-requested")` reaches the
  renderer.

### F. Save in flight could resurrect a renamed or deleted graph (medium)

- **Invalid transition**: Dirty -> save submitted -> rename/delete/duplicate of that name ->
  save lands and writes the old name again (violates the library's one-file-per-name
  expectation).
- **Reached by**: rename, delete and duplicate stayed enabled while `saveInFlight` was
  true. Opening another graph during a save is intentional and must stay available; a
  blanket library lock would have removed that.
- **Fix**: rename, delete and duplicate take the same `saveInFlight` guard as their
  buttons (`mutateDisabled` while `saving`); open stays enabled. Invariant F3.
- **Tests**: `tests/async-workspace.spec.ts`, "rename, delete and duplicate wait for an in-flight
  save, while open stays available".

### G. Spawn failure dropped the previous finished session (medium)

- **Invalid state**: after `start_run` fails to spawn a worker, the slot is Empty even
  though a finished session with live outputs was present (violates the 2.1 rule that a
  finished session stays until the next run replaces it).
- **Reached by**: `AppService::start` wrote the new session into the slot before spawn, then
  cleared the slot to `None` on spawn failure.
- **Fix**: keep the previous slot value and restore it when spawn fails; the never-started
  session is discarded.
- **Tests**: `app::tests::a_spawn_failure_keeps_the_previous_finished_session`.

### H. Lost `start_run` reply left the service session unattached (medium)

- **Invalid state**: service `Occupied, running` while the renderer has no `attachedRun`
  for that id: Cancel and the review panel still work from `runtime_state`, but the run
  panel stays empty and completion is never announced (violates F1's ownership and X1).
- **Reached by**: the service accepted `start_run` and spawned the worker, then the IPC
  reply failed or never arrived, so `startRun` never set `attachedRun` / `pendingRun`.
- **Fix**: when `start_run` errors but `runtime_state` already shows this graph running,
  `startRun` attaches to that session; the poll also attaches any orphan running session
  whose graph matches the open document.
- **Tests**: `tests/async-actions.spec.ts`, "a lost start_run reply still attaches to the service
  session that accepted it".

### I. Clear history ignored `occupied`, left `historical` set and lacked an epoch guard (medium)

- **Invalid states**: Clear history and Clear cache stayed enabled while `form` or `busy`
  made `occupied` true (violates F3); clearing history from `Historical` left
  `historical == true` with `shownRun == null` instead of reaching `Idle`; navigating while
  `clear_history` was in flight could wipe the newly opened graph's state (violates D1).
- **Reached by**: opening the run inputs form while history existed, or inspecting an
  earlier run and clicking Clear history.
- **Fix**: both buttons guard on `occupied`; Clear history captures `documentEpoch`, skips
  state updates if the epoch changed, and resets `historical` and `historicalRef`.
- **Tests**: `tests/run-lifecycle.spec.ts`, "clearing history waits while occupied and exits historical view cleanly".

### J. Deleting the open workflow left stale history and remembered outputs (medium)

- **Invalid state**: the Inspector still listed deleted runs and remembered outputs whose
  `output` requests fail after `delete_graph` removed their files on disk (violates X4 for
  retained client copies).
- **Reached by**: deleting the currently open workflow. `deleteGraph` kept `graph.name`
  unchanged, so the history-refresh effect did not re-run.
- **Fix**: when `deleteGraph` deletes the open graph, it clears `history`, `shownRun`,
  `attachedRun`, `historical` and `remembered`.
- **Tests**: `tests/run-lifecycle.spec.ts`, "deleting the open workflow clears its history and remembered outputs".

### K. Cancelling during `Batch::prepare` could briefly publish a review (low)

- **Invalid state**: `Session` with `running == true`, `cancel == true` and `review == Some`
  (outside the 2.2 state table, violates R1).
- **Reached by**: `cancel_run` arriving after `review_writes`'s initial `check_cancel` and
  before `review::wait` published `Pending`. `review::wait` published before checking
  `cancel`, and the publish callback wrote `state.review` unconditionally.
- **Fix**: `review::wait` checks `cancel` before publishing, and `Session::publish_review`
  checks `running && !cancel` under the session mutex.
- **Tests**: `review::tests::cancelled_wait_does_not_publish_a_pending_review`,
  `app::tests::a_cancelled_session_never_publishes_a_review`.

### L. Quit during `start_run` or an in-flight save bypassed confirmation or quit mid-save (medium)

- **Invalid transitions**: quitting with a clean document while `start_run` was in flight
  cancelled the starting run without prompting (violates X7); clicking **Save and quit**
  while another save was already in flight returned immediately from `save()` and quit
  before the save finished; an in-flight `installExamples` could replace an open quit dialog
  and leave `closePrompt` stuck `true`.
- **Reached by**: Cmd+Q while `start_run` or `save_graph` was pending, or while
  `install_examples` was completing.
- **Fix**: `closeRequested` treats `startingRun` as active and refuses to quit early if
  `saveInFlight` is true when **Save and quit** is chosen; `installExamples` sets `busy` and
  checks `documentEpoch`, `rootNavigation` and `closePrompt` before opening a graph.
- **Tests**: `tests/run-lifecycle.spec.ts`, "quitting while start_run is in flight asks before cancelling",
  "save and quit refuses to quit while another save is already in flight".

### 5.1 Reachable but left as is

These are real but low impact, or the alternative would cost an intended behaviour. Each
is listed so a later change does not mistake them for oversights.

| Situation | What happens | Why unchanged |
| --- | --- | --- |
| Live outputs over 4 MB in total while a run is still going | `output` answers `no longer stored` for the evicted node until the record exists; **Retry output** then works. | Bound is deliberate; the record recovers the output. |
| A history run is inspected in the gap between a review decision and the completion poll | The completion notice is suppressed to keep the chosen run on screen; results and history are still recorded, and a service error is still shown. | Intended, pinned by `run-history.spec.ts`. |
| Undo after renaming the open graph | The old name comes back; saving recreates the old file. | Undo restores documents, and the name is part of the document. |
| Two targets fail; the record is reopened | `Run.failed` names the first to fail; the order of the others is not stored. | Only the first matters to the outcome's error and the headline. |

### 5.2 Checked and sound

Paths that looked suspicious and held up, so they need not be re-derived:

- Cancel racing a decision: `Session::cancel` sets the flag before answering `Reject`;
  `wait` rechecks the flag after receipt, so cancel always wins.
- Stale status reads: a poll result numbered below the last applied read is dropped, so
  a finished run never shows as awaiting review again (`async-actions.spec.ts`, "a late
  runtime poll").
- Lock order: slot before session everywhere; the cache lock is never nested inside a
  session lock, and the worker takes only the session lock.
- `Create` mode write: `create_new` at open time, so a file created between review and
  write is never truncated.
- Progress ids for children are prefixed at every depth, so marks and outputs of nested
  calls cannot collide with the root's.

## 6. Test map

| Transition or invariant | Test |
| --- | --- |
| S1 second start refused; V3 stale ids; V1 duplicate decision | `app::tests::pending_review_blocks_second_run_and_rejects_stale_decisions` |
| Cancel wakes a review; `Cancelled` record | `app::tests::cancellation_wakes_review_and_retains_cancelled_history`, `review::tests::cancellation_wakes_a_waiting_review_and_wins_over_approval` |
| R1 cancelled session never publishes review | `review::tests::cancelled_wait_does_not_publish_a_pending_review`, `app::tests::a_cancelled_session_never_publishes_a_review` |
| R3 and R5 on the crashed path | `app::tests::a_crashed_worker_leaves_no_pending_or_running_marks` |
| H4, X4 | `history::tests::renamed_records_report_the_directory_they_are_listed_from`, `app::tests::history_outputs_stay_reachable_under_a_renamed_graph`, `run-lifecycle.spec.ts` "renaming the open workflow re-keys its remembered runs so outputs are requested under the new name", "deleting the open workflow clears its history and remembered outputs" |
| N3, H3, H5 | `exec::tests::independent_failures_each_get_a_failed_state`, `exec::tests::cancellation_takes_precedence_over_an_earlier_target_failure` |
| N4 | `exec::tests::cancellation_after_a_whole_reply_prevents_saving_and_caching`, `exec::tests::agents_stream_text_into_progress_and_stop_on_cancel` |
| P1 | `exec::tests::process_timeout_covers_pipes_inherited_by_grandchildren`, `exec::tests::cancellation_covers_pipes_inherited_by_grandchildren` |
| V2 | `exec::tests::a_stale_file_stops_an_approved_batch_before_its_first_write` |
| R6, R7 feed bounds | `app::tests::feed_preserves_order_coalesces_text_and_replaces_completed_streams`, `app::tests::feed_retention_is_bounded_and_flags_utf8_truncation_and_real_evictions` |
| F1 | `async-actions.spec.ts` "preparing a run blocks repeated execution and root navigation until its inputs are ready", "an accepted run cannot be resubmitted when its first status response is ...", "a lost start_run reply still attaches to the service session that accepted it" |
| F2 | `run-lifecycle.spec.ts` "a review decision is sent once however often its button is pressed while it is in flight" |
| F3 save-vs-mutate and clear history | `async-workspace.spec.ts` "rename, delete and duplicate wait for an in-flight save, while open stays available"; `run-lifecycle.spec.ts` "clearing history waits while occupied and exits historical view cleanly" |
| F4, X1 | `run-lifecycle.spec.ts` "a run whose worker stopped without a record is reported as failed and releases the editor" |
| 2.1 spawn failure restores previous | `app::tests::a_spawn_failure_keeps_the_previous_finished_session` |
| 3.3 stale poll | `async-actions.spec.ts` "a late runtime poll ... cannot override a newer completed status" |
| 3.5, X7 | `run-lifecycle.spec.ts` "a saved, idle workflow quits at once", "quitting while a run is in progress asks first, even with nothing unsaved", "quitting while start_run is in flight asks before cancelling", "save and quit refuses to quit while another save is already in flight"; `session.spec.ts` "leaving unsaved changes offers to save them first" |
| D1 | `async-workspace.spec.ts`, `async-actions.spec.ts` prompt and save tests |
| 3.4 | `run-panel.spec.ts` "historical runs expose all results and failed full-output loads can be retried" |

## 7. Keeping this file current

When a change adds a flag, an `Option`, a ref that gates an action, or a new error string
on an IPC request, it has changed one of these machines. Add the row, state the invariant
it protects, and point the regression test at it here. Cite symbols by name, not by line.
A fixed finding stays recorded in section 5 with a pointer to its test and its fix
reflected in the tables above; a finding that is accepted goes into 5.1 with the reason.
