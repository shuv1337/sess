# sess

> Search coding-agent transcripts from one local index.

`sess` is a local-first Rust CLI and terminal UI for people who work across
Claude Code, Codex CLI, Hermes Agent, OpenCode, and Pi Agent sessions. It normalizes local
transcripts into SQLite, derives a Tantivy keyword index, and can add optional
FastEmbed-powered semantic ranking.

## Quickstart

Build the binary, create a local index without downloading the embedding model,
and run a JSON search:

```bash
cargo build --release
./target/release/sess --no-semantic index --full
./target/release/sess --no-auto-index search "auth middleware" --json --limit 20
```

The data directory defaults to your platform local-data directory plus `sess`
(for example, `~/.local/share/sess` on Linux). Pass `--data-dir <PATH>` to use
a different location.

## Usage

Run the built binary without a subcommand to open the interactive terminal UI.
It shows a query field, results, and the selected conversation's full message
stream.

![sess TUI showing a local search result and conversation detail](docs/images/tui-search.png)

Use the command help for the complete, generated interface:

```bash
./target/release/sess --help
```

### Common operations

```bash
./target/release/sess agents --json
./target/release/sess stats --json
./target/release/sess usage
```

`search` accepts agent, workspace, date, pagination, and ranking filters. Use
`--semantic` on a search to combine keyword results with embeddings when they
are available. `view` accepts either a conversation ID or its source path.

```bash
./target/release/sess search --help
./target/release/sess index --help
./target/release/sess stats --help
./target/release/sess agents --help
./target/release/sess view --help
./target/release/sess usage --help
```

### Usage analytics

`sess usage` summarizes provider-reported API calls, request attempts, and
tokens by harness, provider route/family, model ID/family, and provider-model
pair. The terminal report is the default; the same normalized report is
available as JSON or as a standalone, responsive HTML dashboard with inline
SVG charts and no remote assets.

```bash
# Terminal summary across all indexed usage
./target/release/sess usage

# Filters repeat within a dimension; --harness is an alias for --agent
./target/release/sess usage --harness codex --provider openai --since 30d

# Separate organic work, model variants, and task kinds
./target/release/sess usage --exclude-synthetic --variant high --task coding

# Machine-readable data or a shareable local visual report
./target/release/sess usage --bucket week --json
./target/release/sess usage --since 90d --html ./usage-report.html

# Opt in to the bundled, versioned public list-price estimator
./target/release/sess usage --estimate-list-costs
```

Usage rows are read from the original source stores during indexing. Existing
indexes receive a one-time connector migration scan so historical rows gain
usage data. Transcript records, logical sessions, usage records, represented
API calls, and failed/retried attempts are distinct metrics. Raw provider/model
values are retained while canonical families carry their inference provenance;
unknown buckets remain visible instead of being guessed away. Source coverage
also shows assistant-bearing transcript records that have no normalized usage.
Because those records cannot be filtered by provider/model/time, the source
coverage table is explicitly full-corpus, unfiltered, and pre-report-dedup; its
scope is machine-readable as `full_corpus_raw_pre_report_dedup`.

The report also separates source completeness from analytical quality. It
shows provider/model attribution, token-semantics provenance, component-total
reconciliation, and mismatch rates by both represented API calls and tokens.

Token totals remain source-reported when available. Fresh input, cache reads,
cache writes, output, and the reasoning subset are kept separate, and source
component totals are retained for reconciliation. Duplicate invocation IDs are
removed across copied/archived transcripts and the removed rows/tokens are
reported. Session/model aggregates contribute to all-time totals; a date range
includes an aggregate only when its complete interval is contained, and a trend
bucket receives it only when the interval fits that one bucket. Unallocated and
excluded tokens are disclosed rather than assigned invented timing.

Actual cost, source estimates, and the optional public-list estimate are
separate fields with explicit coverage. The bundled estimator only prices exact
first-party model matches when complete token components and an unambiguous
direct route are available. Known long-context tiers are applied; regional,
gateway, contract, batch, priority/flex, and ambiguous cache-write TTL cases
remain unpriced. Its versioned base-list output is a comparison aid, not a bill.
Provider/model coverage is reported both by represented API calls and by
tokens. `--top` limits attributed terminal and HTML rows while keeping Unknown
buckets visible; JSON always contains every group. Extremely wide explicit
timelines omit zero-only gaps instead of truncating late usage and are marked as
sparse.

### Index behavior

`sess search`, `sess usage`, and the TUI create an initial index when needed,
then refresh a stale index by default after 15 minutes. Connector parser or
source-root changes trigger one migration scan even when the age threshold has
not elapsed. `--no-refresh` disables only the age-based refresh;
`--no-auto-index` disables automatic indexing entirely.
`--max-age` accepts values such as `5m`, `1h`, and `2h30m`.

Use `index --full` to rescan all supported transcript roots, or combine it with
`--dry-run` to inspect the full prospective update without creating files,
running migrations, or mutating SQLite, Tantivy, or embeddings. Use `index
--rebuild` to derive Tantivy from the SQLite database without rescanning source
files. Full and rebuild are distinct modes, and rebuild and dry-run are
intentionally mutually exclusive.

### Supported sources

| Source | Default location | Override |
|---|---|---|
| Claude Code | `~/.claude/projects` | — |
| Codex CLI | `~/.codex/sessions`, `~/.codex/archived_sessions` | `CODEX_HOME` |
| Hermes Agent | `~/.hermes/state.db`, `~/.hermes/profiles/*/state.db` | `HERMES_HOME` |
| OpenCode / shuvcode | `~/.local/share/opencode/storage`, `~/.local/share/opencode/*.db` | `OPENCODE_STORAGE_ROOT`, `OPENCODE_DB` |
| Pi Agent and compatible layouts | `~/.pi/agent`, `~/.shuvpi/agent`, `~/.shuvhelm/pi-agent`, `~/.shuvhelm/mate`, `~/.local/share/shiv`, `~/.openclaw` | `SESS_PI_AGENT_DIRS`, `PI_CODING_AGENT_DIR`, `SHIV_AGENT_DIR`, `OPENCLAW_HOME` |

`SESS_PI_AGENT_DIRS` accepts a platform path list (`:`-separated on Unix) for
additional Pi-compatible agent roots. The legacy single-root
`PI_CODING_AGENT_DIR` remains supported. Both are additive: they do not hide
the personal Pi root, Codex external-runtime Pi root, or the standard shuvhelm
fleet and mate roots.

OpenCode discovery reads both legacy file storage and every top-level SQLite
store, including late-v1 and v2/shuvcode layouts, and merges duplicate sessions
deterministically. Rows whose parent session cannot be found are excluded by
default; set `SESS_OPENCODE_RECOVER_ORPHANS=1` for a deliberate recovery scan
that indexes those rows as synthetic orphan records instead of silently mixing
them into normal usage.

## Background refresh

For an unattended local index, optional integrations are provided for
[systemd user units](./contrib/systemd/README.md) and
[oxmgr](./contrib/oxmgr/README.md). The systemd service disables semantic
embeddings by default; the oxmgr loop enables them unless configured otherwise.
When enabled, FastEmbed's downloaded model cache lives under the sess data
directory at `<data-dir>/fastembed`.

## Development

```bash
cargo test
```

See [CONTRIBUTING.md](./CONTRIBUTING.md) for contribution guidance and
[ARCHITECTURE.md](./ARCHITECTURE.md) for the storage, indexing, and connector
layout. The prioritized project direction is in [ROADMAP.md](./ROADMAP.md).

## Security

Session transcripts may contain sensitive prompts, source code, or secrets.
Treat the local SQLite database, Tantivy index, and optional embedding data as
sensitive. See [SECURITY.md](./SECURITY.md) for private vulnerability reporting.

## License

MIT — see [LICENSE](./LICENSE).
