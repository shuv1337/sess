# Contributing to `sess`

Thanks for contributing.

This project is a Rust CLI/TUI application for indexing and searching coding-agent transcripts.

## Development Setup

### Prerequisites
- Rust toolchain (stable, with `cargo`)

### Build
```bash
cargo build
```

### Run
```bash
# CLI
cargo run -- --help

# TUI
cargo run --
```

> Tip: for local experiments, use a throwaway data dir:
>
> ```bash
> cargo run -- --data-dir /tmp/sess-dev stats
> ```

## Test & Quality Checks

### Required before opening a PR
```bash
cargo test
```

### Recommended
```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
```

If clippy `-D warnings` is too strict for your branch, at least run clippy without `-D` and explain any remaining warnings in the PR.

## Code Organization

- `src/connectors/` — per-agent parsers/scanners
- `src/storage/` — SQLite, migrations, persistence APIs
- `src/search/` — Tantivy + query logic + semantic helpers
- `src/tui/` — terminal app state/rendering/background search
- `tests/cli_contract.rs` — command/output contract coverage

## Common Contribution Types

## 1) Adding a new connector
1. Add a new file in `src/connectors/`.
2. Implement the `Connector` trait.
3. Normalize to `Conversation`/`Message` model.
4. Register connector in `all_connectors()`.
5. Add parser and scan tests (empty files, malformed lines, timestamp handling, role mapping, etc.).

## 2) Search/ranking changes
1. Update logic in `src/search/query.rs`.
2. Add/adjust unit tests for ordering and filters.
3. Validate JSON contract is still stable (`tests/cli_contract.rs`).

## 3) Storage/schema changes
1. Add a migration under `src/storage/migrations/`.
2. Register migration in `MIGRATIONS` (`src/storage/sqlite.rs`).
3. Ensure migrations are idempotent.
4. Add tests for both old/new behavior paths.

## PR Guidelines

Please include:
- **What changed**
- **Why it changed**
- **How you tested it** (commands + results)
- **Any known trade-offs / follow-ups**

Small, focused PRs are strongly preferred over broad mixed changes.

## Suggested Commit Style

Conventional commit format is preferred:
- `feat: ...`
- `fix: ...`
- `refactor: ...`
- `test: ...`
- `docs: ...`

## Current Priority Areas

See [`ROADMAP.md`](./ROADMAP.md) for prioritized work.

In particular, correctness fixes around incremental indexing and hybrid semantic result handling are high-impact.
