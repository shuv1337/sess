# Architecture & Project Assessment

Snapshot date: **2026-02-22**

## 1) Current-State Assessment

### Overall status
- ✅ End-to-end workflow is implemented and functional:
  - ingest agent files
  - normalize to shared model
  - persist in SQLite
  - index/search with Tantivy
  - browse in CLI/TUI
- ✅ Connector-based architecture is clean and extensible
- ✅ Good test breadth across parsing, indexing, querying, TUI state, and CLI contracts
- ⚠️ Some correctness and polish issues remain (documented below)

### Test health at snapshot
- `cargo test` passed locally
- Unit tests: **196** (lib target; mirrored for bin target)
- Integration tests: **6** (`tests/cli_contract.rs`)

### Maturity
- Strong **alpha / early beta** for local personal workflows
- Not yet “fully hardened production” due to known edge cases and quality debt

---

## 2) Technology Stack

- **Language**: Rust (edition 2024)
- **CLI**: `clap`
- **Storage**: SQLite via `rusqlite` (bundled SQLite)
- **Full-text search**: `tantivy`
- **Semantic embeddings**: `fastembed` (AllMiniLML6V2)
- **TUI**: `ratatui` + `crossterm`
- **Parallelism**: `rayon` (notably OpenCode connector assembly)
- **Serialization**: `serde`, `serde_json`

---

## 3) High-Level Data Flow

```text
Agent transcript files (JSON/JSONL)
  ↓ scan + parse per connector
Conversation / Message / UsageRecord normalized model
  ↓ upsert
SQLite (conversations, messages, usage_events, embeddings, meta)
  ├─ derive search docs → Tantivy index → CLI/TUI search
  └─ aggregate usage_events → terminal / JSON / standalone HTML reports
```

Key design choice: **SQLite is source of truth**, Tantivy is a derived search index.

---

## 4) Repository Structure

```text
.
├── Cargo.toml
├── src
│   ├── cli.rs
│   ├── indexer.rs
│   ├── main.rs
│   ├── model.rs
│   ├── usage.rs
│   ├── connectors
│   │   ├── mod.rs
│   │   ├── claude_code.rs
│   │   ├── codex.rs
│   │   ├── hermes.rs
│   │   ├── opencode.rs
│   │   └── pi_agent.rs
│   ├── search
│   │   ├── mod.rs
│   │   ├── index.rs
│   │   ├── query.rs
│   │   └── semantic.rs
│   ├── storage
│   │   ├── mod.rs
│   │   ├── sqlite.rs
│   │   └── migrations
│   │       ├── 001_initial.sql
│   │       ├── 002_add_embeddings.sql
│   │       ├── 003_add_usage_events.sql
│   │       └── 004_usage_provenance.sql
│   └── tui
│       ├── mod.rs
│       ├── app.rs
│       ├── search.rs
│       └── ui.rs
└── tests
    └── cli_contract.rs
```

---

## 5) Core Components

## 5.1 `src/model.rs`
Normalized domain layer:
- `Agent`, `Role`
- `Message`
- `UsageRecord`
- `Conversation`
- `SourceFile`
- helper functions: title truncation, fingerprints, timestamp parsing

Important behavior:
- `Conversation::full_text()` creates indexed text payload
- `source_fingerprint()` gives deterministic source identity based on file path + metadata

## 5.2 Connectors (`src/connectors/*`)
`Connector` trait defines:
- `agent()`
- `detect()`
- `default_roots()`
- `scan(roots, since_ts)`

Implemented connectors:
- `ClaudeCodeConnector`
- `CodexConnector`
- `HermesConnector`
- `OpenCodeConnector`
- `PiAgentConnector` (also supports shiv/openclaw layouts)

Each connector converts native transcript structures to common `Conversation`,
`Message`, and invocation/session-model `UsageRecord` values. Usage stays
separate from normalized messages because a single provider call may produce
several searchable message parts, while Hermes and OpenCode can expose bounded
session/model aggregates without per-message token counts. Stable source event
IDs suppress copied invocation rows; explicit event/interval/session grain and
interval bounds prevent aggregates from masquerading as exact events.

Conversation provenance distinguishes physical transcript records from logical
sessions, parent/child records, synthetic/test records, and record kind. Usage
provenance retains raw and canonical provider/model identities, inference
source/confidence, model variant/task, billing route/mode, request attempts,
token semantics, component/reported totals, and cost status/source/version.

## 5.3 Storage (`src/storage/sqlite.rs`)
Responsibilities:
- migration management
- upsert conversations/messages/usage events transactionally
- stale deletion
- metadata and stats
- embedding read/write

Schema tables:
- `conversations`
- `messages`
- `usage_events`
- `embeddings`
- `meta`
- `schema_migrations`

## 5.4 Search index (`src/search/index.rs`)
Tantivy wrapper:
- schema creation + schema hash tracking
- writer lifecycle (`start_writer`, `commit`)
- add/update/delete documents by conversation DB ID
- rebuild from SQLite rows

## 5.5 Query execution (`src/search/query.rs`)
- builds boolean/term/range queries
- applies filters (agent/workspace/time)
- supports ranking modes (recent/balanced/relevance/newest/oldest)
- snippet generation
- RRF fusion for hybrid mode

## 5.6 Semantic (`src/search/semantic.rs`)
- embedding generation via FastEmbed
- model cache scoped to `<data-dir>/fastembed`
- cosine similarity search over stored vectors
- used as optional augmentation path in CLI search

## 5.7 Index orchestration (`src/indexer.rs`)
Modes:
- full scan
- incremental scan
- rebuild Tantivy from SQLite

Coordinates connectors, storage upsert decisions, Tantivy updates, and embedding indexing.

## 5.8 TUI (`src/tui/*`)
- app state machine (`app.rs`)
- rendering (`ui.rs`)
- background search thread (`search.rs`) for responsive interaction

## 5.9 Usage analytics (`src/usage.rs`)

- filters normalized usage rows by harness, provider, model, variant, task,
  workspace, synthetic status, and time
- builds one serializable report with transcript/logical-session/API-call/
  attempt totals, raw and canonical attribution, joint provider-model splits,
  source coverage, hierarchy/grain, deduplication, costs, and UTC calendar trends
- renders the same report as compact terminal text or standalone inline-CSS/SVG HTML
- preserves explicit Unknown groups and reports attribution coverage by both
  represented calls and tokens
- keeps source-health coverage explicitly full-corpus/raw because records with
  assistant output but no usage cannot be filtered by provider/model/time
- reports token-semantics provenance and reconciles comparable source totals
  against normalized components without treating non-comparable rows as errors

---

## 6) Operational Workflows

## 6.1 Initial startup (no command)
1. Construct `Indexer`
2. If auto-index enabled and DB empty → full index
3. Launch TUI with storage + cloned Tantivy reader

## 6.2 `sess index --full`
1. Start Tantivy writer
2. Scan all detected connectors (no time filter)
3. Upsert changed/new conversations into SQLite
4. Update Tantivy docs for changed rows
5. Delete stale conversations
6. Commit Tantivy
7. (Optional) generate missing embeddings

## 6.3 `sess index` (incremental)
1. Read `last_scan_ts` from meta
2. Compare each connector's discovered-root fingerprint and parser revision
3. Scan only files modified since the timestamp, or scan that connector without
   a time bound when either cursor changed
4. Skip unchanged fingerprints
5. Upsert/index changed rows
6. Delete stale conversations (see limitation below)
7. Commit, then persist connector cursors and update last scan meta only for
   complete connector scans; partial valid rows are retained and an incomplete
   migration is retried without a time bound on the next refresh

## 6.4 `sess search`
1. Build `SearchQuery` from CLI args
2. Run Tantivy keyword retrieval
3. If semantic enabled + initialized:
   - load embeddings
   - compute semantic nearest neighbors
   - RRF merge into result ranking
4. Emit human or JSON output

## 6.5 `sess usage`

1. Run the normal initial/stale/connector-migration refresh policy
2. Load normalized `usage_events` joined to conversation harness/workspace data
3. Apply report filters and aggregate tokens/API calls/costs once
4. Emit terminal text, renderer-independent JSON, or a standalone HTML report

Provider/model coverage is reported by represented API calls and by tokens.
Aggregate intervals enter a requested range only when fully contained and enter
a trend bucket only when the entire interval fits; unallocated and excluded
tokens remain explicit. Actual cost, source estimates, and opt-in versioned
public-list estimates are never collapsed into one billing-truth field. The
public-list estimator requires complete components, exact first-party models,
and an unambiguous direct route; unsupported modifiers, gateways, regional
pricing, and ambiguous cache-write TTLs remain visibly uncovered.

---

## 7) Known Limitations (Important)

1. **Hybrid fusion excludes semantic-only hits**
   - RRF currently builds final result payloads from keyword-side `result_map`.
   - Pure semantic candidates without keyword presence can be dropped.

2. **TUI path is keyword-only today**
   - TUI search thread uses query execution without semantic merge path.

3. **Incremental embedding refresh trigger is narrow**
   - Incremental `index_embeddings()` call is gated by `conversations_updated > 0`.
   - Insert-only incremental runs may skip embedding generation.

4. **`source_files` reconstruction is minimal when reading from DB**
   - `get_conversation()` reconstructs simplified source metadata.

5. **Engineering polish debt**
   - Many compiler warnings currently exist.
   - No repository CI pipeline file present yet.

---

## 7a) Operational Guarantees (stale deletion + refresh)

- A successful stale-deletion sweep removes the row from **both** SQLite and
  the Tantivy index in the same indexer call; they cannot drift in the
  happy path.
- Rows whose agent is **not currently detected** (env var or root
  temporarily disappeared) are **never** auto-deleted.
- Filesystem uncertainty (`Path::try_exists()` returning `Err`) **keeps**
  the row and emits a warning. Confirmed `Ok(false)` is required for
  deletion.
- Auto-refresh is opt-out:
  - `--no-auto-index` suppresses *all* automatic indexing (initial + age).
  - `--no-refresh` suppresses only age-based refresh; initial run on empty
    DB still happens.
  - `--max-age <DURATION>` overrides the 15-minute default.
- The TUI runs a background refresh thread every 5 minutes by default;
  refreshes never overlap and back off on SQLite/Tantivy lock contention
  (`BusySkipped` event).

---

## 8) Extension Points

- Add new connector by implementing `Connector` and registering in `all_connectors()`.
- Extend ranking by modifying blend strategy in `RankingMode`.
- Improve semantic retrieval by replacing brute-force vector scan with ANN index.
- Add richer transcript normalization by enhancing per-agent parse handlers.

---

## 9) Suggested Hardening Priorities

1. Include semantic-only rows in hybrid result materialization
3. Integrate semantic mode into TUI search flow
4. Resolve warnings (`fmt`, `clippy`, dead code cleanup)
5. Add CI checks (format + clippy + tests)
6. Improve source metadata fidelity persistence/round-trip
