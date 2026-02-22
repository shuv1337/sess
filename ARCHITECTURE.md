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
Conversation / Message normalized model
  ↓ upsert
SQLite (conversations, messages, embeddings, meta)
  ↓ index docs
Tantivy index
  ↓
CLI search / TUI search thread
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
│   ├── connectors
│   │   ├── mod.rs
│   │   ├── claude_code.rs
│   │   ├── codex.rs
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
│   │       └── 002_add_embeddings.sql
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
- `OpenCodeConnector`
- `PiAgentConnector` (also supports shiv/openclaw layouts)

Each connector converts native transcript structures to common `Conversation` + `Message`.

## 5.3 Storage (`src/storage/sqlite.rs`)
Responsibilities:
- migration management
- upsert conversations/messages
- stale deletion
- metadata and stats
- embedding read/write

Schema tables:
- `conversations`
- `messages`
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
2. Scan only files modified since timestamp
3. Skip unchanged fingerprints
4. Upsert/index changed rows
5. Delete stale conversations (see limitation below)
6. Commit + update last scan meta

## 6.4 `sess search`
1. Build `SearchQuery` from CLI args
2. Run Tantivy keyword retrieval
3. If semantic enabled + initialized:
   - load embeddings
   - compute semantic nearest neighbors
   - RRF merge into result ranking
4. Emit human or JSON output

---

## 7) Known Limitations (Important)

1. **Incremental stale-deletion semantics are risky**
   - Current incremental flow can treat conversations not seen in this scan as stale.
   - Practical guidance today: use `sess index --full` when safety matters.

2. **Hybrid fusion excludes semantic-only hits**
   - RRF currently builds final result payloads from keyword-side `result_map`.
   - Pure semantic candidates without keyword presence can be dropped.

3. **TUI path is keyword-only today**
   - TUI search thread uses query execution without semantic merge path.

4. **Incremental embedding refresh trigger is narrow**
   - Incremental `index_embeddings()` call is gated by `conversations_updated > 0`.
   - Insert-only incremental runs may skip embedding generation.

5. **`source_files` reconstruction is minimal when reading from DB**
   - `get_conversation()` reconstructs simplified source metadata.

6. **Engineering polish debt**
   - Many compiler warnings currently exist.
   - No repository CI pipeline file present yet.

---

## 8) Extension Points

- Add new connector by implementing `Connector` and registering in `all_connectors()`.
- Extend ranking by modifying blend strategy in `RankingMode`.
- Improve semantic retrieval by replacing brute-force vector scan with ANN index.
- Add richer transcript normalization by enhancing per-agent parse handlers.

---

## 9) Suggested Hardening Priorities

1. Fix incremental stale-deletion semantics
2. Include semantic-only rows in hybrid result materialization
3. Integrate semantic mode into TUI search flow
4. Resolve warnings (`fmt`, `clippy`, dead code cleanup)
5. Add CI checks (format + clippy + tests)
6. Improve source metadata fidelity persistence/round-trip
