# Canonical import publication responsiveness

## Status

M14 retained the synchronous final canonical commit. The cancellable preparation
phase is already a worker; the final journal-backed publication is still applied
on the GUI/TUI thread. This document records the exact current boundary and the
design required for a safe worker-based commit. It is deliberately a design
record, not a claim that the redesign has shipped.

## Current lifecycle

| Phase | Thread | Cancellable | Progress source | Persistence effect |
|---|---|---|---|---|
| Discovery and metadata inference | import worker | Yes | `ImportProgress` stages | None |
| Fingerprinting / duplicate detection | import worker | Yes | progress items | None |
| Artwork/lyrics preparation and portable copy | import worker | Yes | progress and item state | Prepared copies only; cancelled copies are removed |
| Review edits | UI thread | N/A | none | None |
| Canonical staging and copy verification | UI thread | **No** | `WritingCatalogue` | Transaction staging and journal |
| Catalogue publication and controller refresh | UI thread | **No** | `WritingCatalogue` | Atomic catalogue generation |
| Queue/history/UI rebind | UI thread | N/A | completion report | Queue/history stores |

The GUI's `apply_import_review` and the TUI's `poll_import_worker` both call
`Controller::apply_import_plan` synchronously. A large import can therefore
freeze the event loop while files are staged, verified, renamed and committed.
The commit is journal-backed and never exposes a partial catalogue.

M14 fixed one UI-level race: a cancel request arriving after copy preparation but
before the commit message is processed now aborts before publication and removes
that preparation's unpublished copies. It also labels the final commit as
non-cancellable. It does **not** make the final commit asynchronous.

## Required design for a worker-based commit

The safe unit is a **canonical commit job**, not the whole `Controller` and not a
partially published store.

### Job construction (UI thread)

1. Re-check the active root, `library_session`, plan generation and reviewed
   target snapshot.
2. Reject if a scan, another commit, recovery or library switch is active.
3. Build the canonical `CanonicalImportTrackRequest` and
   `CanonicalTrackMetadataUpdate` batches from the Controller snapshot, without
   writing.
4. Capture a report template for candidates (including skips and pre-commit
   validation failures).
5. Record the expected root, session, generation and a unique job token.
6. Mark the Controller `commit_in_progress` and disable all catalogue-writing
   actions in the UI.

### Worker execution

The worker receives only:

- the canonical library root;
- the expected generation/session;
- owned request/update batches;
- the job token.

It opens a fresh `CanonicalLibrary` on the worker, verifies the catalogue
generation, and calls `import_review_batch` exactly once. It returns the opened
store, returned IDs and the canonical error/result. It never mutates Controller
state.

Exclusive transaction ownership is preserved by Controller state: no second
commit, scan, metadata mutation, deletion, transfer, library switch or recovery
can start while `commit_in_progress` is set. Playback and queue-only operations
may continue because they do not write the canonical catalogue; the UI should
still disable metadata controls.

### Completion and rebinding

On success the UI:

1. verifies the job token and that the library session/root are still active;
2. installs the returned store, replacing the stale in-memory store;
3. refreshes the catalogue/artist/album/track views;
4. reconciles the default queue and persists queue/history state;
5. publishes `LibraryChanged`;
6. clears `commit_in_progress` and reports imported/updated/skipped/failed rows.

If the session/root changed while the worker ran, the returned store is dropped
and the current library is reopened/refreshed; the result is rejected as stale,
never installed over another library.

On a worker error the UI reopens the canonical root to run journal recovery and
then refreshes. It must distinguish:

- failure before the catalogue commit point: previous catalogue remains active;
- committed catalogue with cleanup warning: refresh authoritative committed state
  and report the cleanup warning;
- malformed/unrecoverable journal: enter the existing recovery-required state;
- panic/disconnect: treat as an uncertain commit and reopen/recover.

### Cancellation

- Preparation remains cancellable with immediate item-level checks.
- Once a job is handed to the worker, cancellation is **not offered**. The commit
  can publish atomically or fail/recover; pretending it can be cancelled after
  journal creation would be false.
- The UI must show a `Committing` state with no Cancel button and explicit text:
  “Publishing the catalogue; this step cannot be cancelled.”
- A close request during `Committing` should be deferred until the worker result
  is processed. If the process is killed, the existing journal recovery is the
  authority.

### Failure, panic and process termination

- Worker panic or channel disconnect leaves a transaction directory. The next
  open runs the existing journal recovery; it must not be deleted heuristically.
- A process killed during staging, publication or cleanup relies on the same
  journal and catalogue generation rules already used by synchronous commits.
- No worker may delete a journal or catalogue it did not create.
- Worker logs and error text must not contain credentials or user paths beyond
  the existing user-facing diagnostics.

### Shutdown ordering

Shutdown must:

1. refuse to start if a commit is already active, or wait for its result;
2. restore/persist queue and history only after the final catalogue state is
   known;
3. join the commit worker with a bounded wait;
4. if the wait expires, detach only after preserving the journal (never delete
   it), and document that the process may need recovery on next open;
5. drop playback and engine handles after the commit decision.

The existing bounded scan and audio-recovery shutdown behavior should be reused;
an unbounded filesystem/join wait is not acceptable.

### Required UI states

- `Preparing` / `Importing`: cancellable worker copy phase.
- `Committing`: non-cancellable final commit; progress text but no false percent.
- `Completed`: report counts and committed generation.
- `Failed`: pre-commit failure, with library unchanged.
- `Recovery required`: journal/catalogue repair required; no normal writes.
- `Cancelled`: only before publication started.

### Why M14 defers the implementation

A second-store worker is feasible, but it requires:

- a Controller-wide write guard for every canonical mutation path;
- a new job/outcome type spanning `flux-app` and both front-ends;
- close/shutdown coordination with the GUI event loop and TUI loop;
- adapting many deterministic GUI/TUI tests that currently assume synchronous
  publication;
- preserving the existing report ordering and error classification.

That is a transaction-ownership and lifecycle redesign. Doing it without those
controls would risk a release-candidate regression or a stale catalogue being
installed after a switch. M14 therefore retains the proven synchronous commit,
fixes the cancel race and labels the phase honestly. The design above is the
implementation contract for a dedicated follow-up milestone.

## Acceptance tests for the future worker

1. A large import leaves the GUI/TUI event loop able to process a paint/status
   tick while the worker stages and commits.
2. A stale job after a library switch is rejected and the new library remains
   active.
3. Two imports cannot start a second commit concurrently.
4. Cancellation during preparation removes only unpublished copies.
5. Cancellation is unavailable during `Committing`.
6. Worker panic/error leads to journal recovery without exposing partial state.
7. Shutdown during commit either waits for a bounded result or leaves a
   recoverable journal; it never deletes the journal.
8. Queue, history, ratings, artwork and lyrics survive a successful commit with
   stable IDs.
9. The same tests run on Linux, Windows and macOS native targets with the null
   backend and, where available, a hardware audio endpoint.
