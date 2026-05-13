# PLAN: Fix stale-deletion bug + keep index fresh

**Status:** Draft — revised after codebase review
**Date:** 2026-05-13
**Owner:** TBD
**Target branch:** `fix/incremental-staleness-and-freshness`

> Repo note: current checkout was reviewed on `master` with a dirty working tree.
> Before implementation, create/switch to the target branch and preserve unrelated
> user changes.

---

## 1. Background

While dogfooding `sess` we noticed it was only returning sessions older than
~38 days. Investigation surfaced **two distinct defects**:

### Bug A — Incremental indexer silently destroys data (P0, data-loss)

`Indexer::incremental_index` (`src/indexer.rs:134`) builds `all_source_paths`
**only** from conversations returned by the time-filtered scan, then passes
that set to `Storage::delete_stale` (`src/storage/sqlite.rs:373`).

`delete_stale` deletes any DB row whose `source_path` is **not** in the set.
Because the per-connector `scan()` honors `since_ts`, every connector whose
files were not modified since the last scan returns an **empty** vector — so
every row for that agent gets deleted.

Reproduction (today, on this machine):

```text
$ sess stats                    # before
opencode: 2817 conversations / 110_128 messages

$ sess index                    # incremental
… Loaded 0 OpenCode sessions
… Deleted 4688 stale conversations

$ sess stats                    # after
opencode: 0 conversations / 0 messages
```

Side-effect: Tantivy is *not* told about the deletion (only `commit()` is
called, but no `delete_term` runs), so search still returns the wiped rows.
The result is a split-brain DB ↔ index state where `sess stats` and
`sess search` disagree.

This bug is already listed in `ROADMAP.md` under **P0 #1** and called out as
the **#1 known limitation** in `ARCHITECTURE.md §7`.

### Bug B — Index never refreshes on its own (P1, UX)

`auto_index` in `src/main.rs:48,76` only runs the indexer when the DB is empty
(`Indexer::needs_initial_index` returns `true` iff `total_conversations == 0`).
There is no timer, daemon, watcher, or git/agent hook. Result: the index drifts
arbitrarily far behind the agents' real session directories — in our case 38
days and ~9,400 new sessions invisible.

---

## 2. Goals & Non-goals

### Goals

- **G1** — `sess index` (incremental) MUST NOT delete rows that simply weren't
  rescanned this run.
- **G2** — Successful stale-deletion paths MUST update SQLite and Tantivy
  together. No stale-deletion split-brain states.
- **G3** — Fresh sessions appear in `sess search` without the user having to
  remember to run `sess index --full`.
- **G4** — All correctness gains are covered by new tests so we don't regress.
- **G5** — Filesystem uncertainty MUST be fail-safe: permission errors,
  transient mount errors, and metadata failures keep rows rather than deleting
  them.

### Non-goals

- Changing the on-disk schema, the `Conversation` model, or the connector trait.
- Real-time file watching (`inotify`). The freshness story can be solved by a
  cheap timer + a fast incremental scan.
- Fixing the unrelated semantic-only RRF hole (ROADMAP P0 #2). Tracked
  separately.
- Guaranteeing all SQLite/Tantivy writes are atomic across arbitrary crashes.
  This plan fixes deletion consistency and adds recovery-oriented tests; full
  cross-store transactions remain out of scope.

---

## 3. Design

### 3.1 Correct stale-detection semantics

The current code conflates **"file not seen this scan"** with **"file no
longer exists"**. The latter is what stale deletion should mean. Fix by making
stale detection **existence-based, not scan-based**, and fail-safe on ambiguous
filesystem state.

Two viable implementations were considered:

**Option A (recommended) — Existence check at delete time.**
Iterate DB rows and check each `source_path`. A row is stale iff its owning
connector is currently detected and the filesystem confirms the path is missing.
Pros: localized, robust to any scan mode. Cons: one existence check per indexed
conversation per stale-check (~15k stats on this machine = negligible).

**Option B — Two-phase scan in incremental mode.**
Walk every file (ignore `since_ts`) just to build an alive-set, but only *parse*
files modified since `since_ts`. Pros: no I/O semantics change inside storage.
Cons: requires every connector to expose an inventory-only mode, expanding the
connector surface area.

**Decision:** Go with **Option A**, but do **not** use `Path::exists()`.
`exists()` collapses permission/transient metadata errors into `false`, which is
unsafe. Use `Path::try_exists()` or equivalent `metadata()` handling:

- `Ok(true)` → keep
- `Ok(false)` → candidate for deletion
- `Err(error)` → keep and emit warning/summary entry

#### New storage API

```rust
// src/storage/sqlite.rs
pub struct MissingSource {
    pub id: i64,
    pub agent: Agent,
    pub source_path: PathBuf,
}

pub struct StaleDeletionSummary {
    pub deleted_ids: Vec<i64>,
    pub deleted_paths: Vec<PathBuf>,
    pub uncertain_paths: Vec<(i64, PathBuf, String)>,
}

pub fn find_missing_sources(
    &self,
    detected_agents: &HashSet<Agent>,
) -> Result<Vec<MissingSource>>;

pub fn delete_missing_sources(
    &mut self,
    detected_agents: &HashSet<Agent>,
) -> Result<StaleDeletionSummary>;
```

Behavior:

- Query `id, agent, source_path` from `conversations`.
- Only evaluate rows whose `agent` is in `detected_agents`.
- If `source_path.try_exists()? == true`, keep.
- If `source_path.try_exists()? == false`, delete the `conversations` row
  (cascade deletes `messages`) and include the ID/path in the summary.
- If existence check errors, keep the row and include it in `uncertain_paths`.
- Rows whose agent is not currently detected are always kept. This avoids
  deleting an entire agent when an env var/root temporarily disappears.

Both `full_index` and `incremental_index` switch to this API. The old
`delete_stale(HashSet<PathBuf>)` is removed unless compile fallout shows a
need for a short-lived `#[deprecated]` internal shim.

#### Tantivy side

After computing deleted DB IDs, call:

```rust
pub fn delete_conversations(&mut self, ids: &[i64]) -> Result<()>;
```

This is a batching wrapper around the existing DB-ID delete primitive:
`Term::from_field_u64(self.field_conv_db_id, db_id as u64)`. The Tantivy schema
field is `conv_db_id`, not `id`.

The call order for stale deletion becomes:

1. Find/delete missing SQLite rows and collect DB IDs.
2. Issue Tantivy `delete_term` for every deleted ID.
3. Commit Tantivy once.
4. Log/print deleted and uncertain counts.

This closes the stale-deletion split-brain hole. It does not claim arbitrary
cross-store atomicity if the process crashes between SQLite and Tantivy; that is
handled by existing `sess index --rebuild` and may be hardened later.

#### Edge case: conversation moved file

`source_path` is the identity key. If an agent renames a session file, the old
path 404s and the new path is upserted as a *new* row. That's current behavior
and out of scope to change here.

### 3.2 Auto-freshness

Add auto-refresh in two user-facing paths and an optional timer for users who do
not keep the TUI open.

#### 3.2.1 Refresh age API

Add:

```rust
pub fn last_scan_age(&self) -> Result<Option<Duration>>;
pub fn should_refresh(&self, max_age: Duration) -> Result<bool>;
```

Rules:

- Missing `meta.last_scan_ts` → refresh needed.
- Age greater than `max_age` → refresh needed.
- Future `last_scan_ts` (clock skew) → do not refresh solely because of age,
  clamp age to zero, and warn once.
- Default `max_age` = 15 minutes.

Initial empty DB behavior remains explicit:

- If DB is empty and auto-index is enabled, run `full_index()`.
- If DB is non-empty but stale, run `incremental_index()`.

#### 3.2.2 `sess search` startup refresh

Before executing the search:

1. Construct `Indexer`.
2. If neither `--no-auto-index` nor `--no-refresh` is set:
   - empty DB → `full_index()`
   - stale DB → synchronous `incremental_index()`
3. Then execute search.

This keeps `sess search foo` fresh with predictable behavior. `--no-refresh`
suppresses age-based refresh, while `--no-auto-index` suppresses all automatic
indexing behavior including initial and age-based refresh.

#### 3.2.3 TUI startup + periodic refresh

The TUI design must match current ownership:

- `src/main.rs` passes `data_dir`, semantic setting, refresh config, and the
  shared `Arc<TantivyIndex>` into `tui::run_app`.
- A new `RefreshThread` owns no borrowed `Storage`. It owns:
  - `data_dir: PathBuf`
  - `enable_semantic: bool`
  - `max_age: Duration`
  - `interval: Duration`
- On each refresh cycle, it creates a fresh `Indexer::new(&data_dir, ...)` so
  writes are isolated from the TUI's borrowed read-side `Storage`.
- It sends events over `mpsc`:
  - `RefreshEvent::Started`
  - `RefreshEvent::Finished { stats, deleted, uncertain }`
  - `RefreshEvent::SkippedFresh`
  - `RefreshEvent::BusySkipped`
  - `RefreshEvent::Failed(String)`
- The TUI `App` gains:
  - `indexing: bool`
  - `last_index_status: Option<String>`
- On successful refresh, the TUI loop:
  1. reloads the shared Tantivy reader via a new `TantivyIndex::reload_reader(&self)` helper,
  2. increments `search_generation`,
  3. triggers a new search.

The refresh thread uses `std::thread::spawn` + `std::sync::mpsc::recv_timeout`,
matching the existing TUI thread model. No async runtime is introduced.

Scheduling:

- On TUI start, run one refresh attempt if stale.
- While TUI is open, run an incremental refresh every 5 minutes by default.
- Do not start overlapping refreshes; if one is in progress, skip the next tick.
- Surface state in the footer: `🔄 indexing…`, `index fresh`, `index failed: …`,
  or `index busy; skipped`.

#### 3.2.4 Optional systemd-user timer (documented, not installed)

Ship:

- `contrib/systemd/sess-index.service`
- `contrib/systemd/sess-index.timer`

Document:

```sh
systemctl --user enable --now sess-index.timer
```

The service should default to:

```sh
sess --no-semantic index
```

Rationale: a background timer should not unexpectedly initialize/download/use
`fastembed`. Users who want semantic embeddings can override the unit command.

### 3.3 CLI surface

Add these flags:

| Flag | Where | Behavior |
|---|---|---|
| `--no-refresh` | global | Suppresses age-based refresh on `search`/TUI launch and TUI periodic refresh. |
| `--max-age <DURATION>` | global | Override 15m freshness threshold, e.g. `1h`, `5m`. |
| `--dry-run` | `index` subcommand | Scan + report what would change, including would-delete rows, with no DB/Tantivy writes. |

Precedence:

- `--no-auto-index` means “do not run indexing automatically for any reason.”
- `--no-refresh` means “allow explicit `sess index`, but suppress automatic
  freshness refresh.”
- For `sess index`, neither flag matters because indexing is explicit.

Parsing:

- Add `humantime` dependency, or implement a local parser.
- Add CLI parse tests for valid (`5m`, `1h`) and invalid duration strings.

### 3.4 Dry-run design

Dry-run must report deletion candidates. It must not skip stale detection.

Add an internal read-only planner:

```rust
pub struct IndexDryRunReport {
    pub would_scan_by_agent: HashMap<Agent, usize>,
    pub would_insert: usize,
    pub would_update: usize,
    pub would_delete: Vec<MissingSource>,
    pub uncertain_paths: Vec<(i64, PathBuf, String)>,
}
```

`Indexer::incremental_index_dry_run()`:

1. Runs connector scans with the normal incremental `since_ts`.
2. Uses `Storage::needs_reindex` to classify inserts/updates.
3. Calls `find_missing_sources(detected_agents)` to report deletions.
4. Performs no SQLite writes and no Tantivy writes.
5. Prints a concise summary and, with `--json` later if desired, machine-readable
   output. JSON is not required for this plan.

### 3.5 Concurrency and lock behavior

Background refresh can collide with explicit CLI indexing. Make contention
fail-safe and user-visible:

- Add SQLite `PRAGMA busy_timeout = 5000` in `Storage::new`.
- Add retry-on-busy around background refresh writes: 200ms backoff, bounded
  attempts.
- If Tantivy writer acquisition fails because another writer is active, send
  `RefreshEvent::BusySkipped` instead of crashing the TUI.
- Never run overlapping TUI refreshes.
- Explicit `sess index` should still fail loudly on unrecoverable lock errors.

---

## 4. Implementation tasks

Listed in dependency order. Each box is a single PR-sized commit.

### Phase 0 — Make validation honest

- [ ] **0.1** Create/switch to `fix/incremental-staleness-and-freshness` and
      preserve unrelated dirty working tree changes.
- [ ] **0.2** Clean current compiler/clippy warnings that would block
      `cargo clippy -- -D warnings`, or update the final validation command if
      the team explicitly decides warning cleanup is out of scope.
- [ ] **0.3** Add `humantime` to `Cargo.toml` if using it for `--max-age`.

### Phase 1 — Stop the bleeding (Bug A)

- [ ] **1.1** Add `MissingSource`, `StaleDeletionSummary`,
      `Storage::find_missing_sources(detected_agents)`, and
      `Storage::delete_missing_sources(detected_agents)` in
      `src/storage/sqlite.rs`.
- [ ] **1.2** Implement fail-safe existence checks: delete only on confirmed
      `Ok(false)`; keep and warn on metadata/permission errors.
- [ ] **1.3** Add `TantivyIndex::delete_conversations(ids: &[i64])` in
      `src/search/index.rs`. Use `field_conv_db_id` and one commit at the
      caller level.
- [ ] **1.4** Replace `delete_stale(&all_source_paths)` calls in both
      `Indexer::full_index` and `Indexer::incremental_index`
      (`src/indexer.rs:105,175`) with the new pair.
- [ ] **1.5** Compute `detected_agents` from the same connector list used for
      scanning:
      `connectors.iter().filter(|c| c.detect()).map(|c| c.agent()).collect()`.
- [ ] **1.6** Remove `delete_stale` or keep a short-lived internal deprecated
      shim only if compile fallout requires it.

### Phase 2 — Regression tests for Bug A

- [ ] **2.1** New unit test
      `storage::tests::delete_missing_sources_keeps_existing_files`:
      seed 3 conversations backed by real temp files, delete file #2, call
      `delete_missing_sources([Agent::PiAgent])`, assert only #2 is gone.
- [ ] **2.2** New unit test
      `storage::tests::delete_missing_sources_ignores_undetected_agents`:
      seed 1 PiAgent + 1 OpenCode row with missing files, call with
      `[Agent::PiAgent]`, assert OpenCode row survives.
- [ ] **2.3** New unit test for metadata errors if practical on this platform:
      existence-check errors are kept and reported as uncertain. If hard to
      make portable, isolate behind Unix-only permissions test.
- [ ] **2.4** New Tantivy unit test:
      `delete_conversations_removes_multiple_docs_with_single_commit`.
- [ ] **2.5** New integration test in `tests/cli_contract.rs`:
      `incremental_index_does_not_delete_unchanged_agents`:
        1. Build temp data-dir and temp fake agent roots.
        2. Run subprocesses with controlled env (`HOME`, `CODEX_HOME`,
           `OPENCODE_STORAGE_ROOT`, `PI_CODING_AGENT_DIR`) so tests never touch
           real user agent data.
        3. Run `sess index --full`; assert both agents indexed.
        4. Touch only agent A's files.
        5. Run `sess index`; assert both agents' conversation counts are
           unchanged.
- [ ] **2.6** New integration test:
      `incremental_index_drops_deleted_files`. Same setup, but delete agent A's
      file before incremental run. Assert that agent's row count drops by 1 in
      **both** SQLite stats and Tantivy `total_hits`.

### Phase 3 — Auto-freshness (Bug B)

- [ ] **3.1** Add `Indexer::last_scan_age() -> Result<Option<Duration>>`.
- [ ] **3.2** Add `Indexer::should_refresh(max_age) -> Result<bool>` including
      future timestamp / clock-skew handling.
- [ ] **3.3** Add CLI flags `--no-refresh` and `--max-age` to `Cli` (global).
      Parse `--max-age` via `humantime` or local parser.
- [ ] **3.4** Add parse tests for `--max-age` and precedence tests for
      `--no-auto-index` / `--no-refresh`.
- [ ] **3.5** Wire refresh into `main.rs` for the `Search` arm:
      empty DB → `full_index`; stale DB → `incremental_index`; respect both
      suppressing flags.
- [ ] **3.6** Add `TantivyIndex::reload_reader(&self) -> Result<()>`.
- [ ] **3.7** Add a TUI `RefreshThread` with event channel as described in
      §3.2.3.
- [ ] **3.8** Extend `App` state with `indexing` and `last_index_status`, render
      footer status, and trigger search regeneration after successful refresh.
- [ ] **3.9** Add TUI state/channel tests proving refresh events update status
      and successful refresh increments search generation.
- [ ] **3.10** Add `Index { dry_run: bool, … }`, plumb through to
      `Indexer::incremental_index_dry_run()`, and print would-insert/update/
      delete/uncertain summary.

### Phase 4 — Concurrency hardening

- [ ] **4.1** Add SQLite `busy_timeout` in `Storage::new`.
- [ ] **4.2** Add bounded retry/backoff for background refresh write conflicts.
- [ ] **4.3** Handle Tantivy writer-busy failures in the refresh thread as
      `BusySkipped` rather than TUI crashes.
- [ ] **4.4** Ensure periodic TUI refreshes do not overlap.

### Phase 5 — Documentation & ops

- [ ] **5.1** Add `contrib/systemd/sess-index.service` and
      `sess-index.timer`; default command should be `sess --no-semantic index`.
- [ ] **5.2** Add README snippet for `systemctl --user enable --now
      sess-index.timer` and document semantic override.
- [ ] **5.3** Update `ARCHITECTURE.md §6` workflows to reflect refresh behavior.
- [ ] **5.4** Update `ARCHITECTURE.md §7` — remove "incremental stale-deletion
      semantics are risky" from known limitations after tests pass.
- [ ] **5.5** Update `ROADMAP.md` — strike P0 #1 after implementation lands.
- [ ] **5.6** Add a short "Operational guarantees" section to
      `ARCHITECTURE.md` documenting:
        - successful stale-deletion runs delete from DB and Tantivy together
        - rows for currently-undetected agents are never auto-deleted
        - filesystem uncertainty keeps rows
        - auto-refresh interval and how to disable
        - `--no-auto-index` vs `--no-refresh` semantics

### Phase 6 — Validation

- [ ] **6.1** `cargo fmt` green.
- [ ] **6.2** `cargo clippy -- -D warnings` green, assuming Phase 0 warning
      cleanup remains in scope.
- [ ] **6.3** `cargo test` green.
- [ ] **6.4** Manual smoke:
        1. Snapshot current `sess stats` numbers.
        2. `sess index` (incremental). Confirm no agent loses rows.
        3. Trigger a real OpenCode session, wait for TUI refresh or run
           `sess search`, confirm it appears without manual `--full`.
- [ ] **6.5** Replay the original repro from §1 and confirm it no longer
      destroys OpenCode rows.

---

## 5. Test matrix

| Scenario | Before (current) | After (expected) |
|---|---|---|
| Full index, no missing files | OK | OK |
| Full index, 1 file deleted on disk | row gone in DB only / Tantivy may retain | row gone in DB + Tantivy |
| Incremental, no file mtime changes anywhere | **all detected agents can be wiped** | no rows touched |
| Incremental, agent A files updated, agent B unchanged | agent B can be wiped | A reindexed, B untouched |
| Incremental, 1 file deleted | row gone in DB only / Tantivy split-brain | row gone in DB and Tantivy |
| Connector `detect() == false` | rows can be wiped on next incremental | rows preserved |
| Metadata/permission error checking source path | can look missing with `exists()` | row preserved, warning recorded |
| TUI idle for 5 min, new session lands on disk | invisible until manual reindex | appears automatically after refresh |
| `sess search` after 38 days | stale results unless manual index | age-based refresh kicks in |
| `sess --no-refresh search` | N/A | no age-based refresh |
| `sess --no-auto-index search` | no initial index only | no automatic initial or age refresh |
| Concurrent CLI `sess index` while TUI refresh ticks | possible lock error/crash risk | refresh skips/retries visibly |
| `sess index --dry-run` with deleted file | unavailable | reports would-delete, no writes |

---

## 6. Risks & mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `stat()`-per-row makes `delete_missing_sources` slow at 100k+ convs | Low | Medium | Benchmark; if needed, group by directory and `readdir` once per parent. |
| Backups/network-mounted session dirs flap missing | Medium | High | Delete only on confirmed `Ok(false)` and only for currently detected agents; keep on errors. Add tombstone grace period later if reports come in. |
| Metadata permission errors masquerade as missing files | Medium | High | Use `try_exists`/metadata tri-state; keep and warn on errors. |
| Background TUI indexer races with CLI index | Medium | Medium | SQLite busy timeout, bounded retry, writer-busy skip event, no overlapping refreshes. |
| TUI search reader does not see newly committed Tantivy docs | Medium | Medium | Add explicit `reload_reader()` on refresh completion and retrigger search. |
| `--no-auto-index` semantics drift | Low | Low | Document precedence with `--no-refresh` in CLI help and README. |
| Removing `delete_stale` breaks external consumers | Very Low | Low | Repo has no published library API; keep deprecated shim only if compile fallout requires it. |
| Timer unexpectedly initializes semantic model | Medium | Low/Medium | Systemd service defaults to `sess --no-semantic index`; document override. |

---

## 7. Rollback plan

Every phase lands behind small, individually revertable commits.

- If Phase 3 TUI refresh causes weirdness, revert TUI refresh commits only;
  Phase 1 + Phase 2 remain independently valuable.
- If `delete_missing_sources` misbehaves, temporarily disable the call site in
  `Indexer::*_index`. The index will then never auto-delete rows, which is
  strictly safer than today's behavior.
- If lock/backoff behavior is noisy, disable periodic TUI refresh while keeping
  `sess search` startup refresh.

---

## 8. Out of scope (follow-ups)

- ROADMAP P0 #2 — RRF semantic-only materialization
- ROADMAP P0 #3 — incremental embedding refresh beyond any incidental fixes
- Real-time inotify-based file watcher
- Full cross-store transactional semantics between SQLite and Tantivy
- Tombstone grace-period config for flaky network mounts
- JSON output for `sess index --dry-run`

---

## 9. Acceptance criteria

A reviewer should be able to verify:

1. `cargo test` includes the new storage, Tantivy, CLI, and TUI refresh tests
   in §4 and they pass.
2. Running `sess index` twice in a row on an unchanged corpus leaves
   `sess stats` numbers identical.
3. Deleting a session file on disk and running `sess index` removes the row from
   **both** `sess stats` and `sess search` output.
4. Metadata/permission errors during stale detection preserve rows and produce
   visible warning/summary telemetry.
5. Launching `sess` (TUI) on a stale index automatically refreshes without user
   action, surfaces an indicator, reloads the Tantivy reader, and refreshes
   results.
6. `sess search` on a stale non-empty DB runs incremental refresh unless
   suppressed by `--no-refresh` or `--no-auto-index`.
7. `sess index --dry-run` reports would-insert/update/delete/uncertain rows and
   performs no DB or Tantivy writes.
8. `ARCHITECTURE.md` and `ROADMAP.md` no longer list incremental stale-deletion
   as an unresolved known limitation.
