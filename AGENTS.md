# sess project guidance

## Architecture

- Rust 2024 CLI/TUI (`sess`) for indexing and searching coding-agent transcripts.
- Connectors in `src/connectors/` normalize source records into `Conversation`/`Message`.
- SQLite is the source of truth; Tantivy is a derived search index. `src/indexer.rs` must keep both consistent.
- Supported sources: Claude Code, Codex CLI, Factory Droid, Hermes, Oh My Pi,
  OpenCode, Pi Agent-compatible layouts, Prime Agent, and shuvlr.

## Connector conventions

- Preserve source paths and use source fingerprints for idempotent upserts.
- When a parser change alters normalized output for existing files, bump its `Connector::parser_revision()` and include that revision in its source fingerprint. This triggers one unfiltered migration scan and guarantees upserts do not skip old rows.
- Codex CLI active and archived rollouts live under `$CODEX_HOME/{sessions,archived_sessions}` (default `~/.codex`). Modern rollout timestamps are RFC 3339 strings; use `model::parse_timestamp` rather than numeric-only parsing.
- Keep connector scan telemetry structured with agent, root, discovered/parsed counts, and parse errors.

## Validation

```bash
cargo test
cargo clippy --all-targets --all-features
cargo fmt -- --check
```

The repository currently emits pre-existing compiler and Clippy warnings; do not mix unrelated warning cleanup into focused changes. Formatting checks and all-target Clippy complete successfully despite those warnings.

## Operations

- Build: `cargo build --release`
- ONNX Runtime is loaded dynamically through FastEmbed's `ort-load-dynamic` feature. On Linux it resolves `libonnxruntime.so` at semantic initialization, avoiding startup-time ELF symbol-version coupling to a particular distro package release.
- `sess` is project-scoped by default: inside a git/jj repo, `search`, `stats`, `usage`, and the TUI only cover sessions whose workspace is in that repo (worktrees unify; missing workspaces stay visible). Use `-g`/`--all` for the global view; explicit `--workspace` overrides scope.
- Full re-index: `sess --no-semantic index --full`
- Inspect detection/stats: `sess agents --json`; `sess stats --json`
- Default index data is under the platform local-data directory, commonly `~/.local/share/sess` on Linux.
