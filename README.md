# sess (`session-search`)

Search across your coding-agent sessions from one place.

`sess` is a local-first Rust CLI + TUI that ingests transcripts from multiple agents, indexes them, and lets you search/filter/view conversations quickly.

## Highlights

- Multi-agent ingestion (Claude Code, Codex, OpenCode, Pi Agent)
- Fast local keyword search (Tantivy)
- Optional semantic search (FastEmbed)
- JSON output for automation/bots
- Interactive terminal UI (`sess` with no subcommand)

## Install / Build

```bash
cargo build --release
```

Binary:

```bash
./target/release/sess
```

## Quick Start

```bash
# 1) Build
cargo build --release

# 2) Create initial index
./target/release/sess index --full

# 3) Search
./target/release/sess search "auth middleware"

# 4) Launch TUI
./target/release/sess
```

## Screenshots

![sess TUI search](docs/images/tui-search.png)

![sess search workflow](docs/images/search-workflow.gif)

## CLI Commands

```bash
sess --help
sess search --help
sess index --help
sess stats --help
sess agents --help
sess view --help
```

Common examples:

```bash
# JSON output for scripts
sess search "voice regression" --json --limit 20

# Filter by agent/workspace/time
sess search "voice" --agent pi_agent --workspace /home/user/project --since 7d --json

# Hybrid keyword + semantic
sess search "crashes when opening tool panel" --semantic --ranking balanced --json

# Stats and agent detection
sess stats --json
sess agents --json

# View conversation by ID or source path
sess view 42
sess view /path/to/session.jsonl --json
```

## TUI Keys (short)

- `q` / `Ctrl+C` — quit
- `?` — help overlay
- Type in search bar — live query
- `Tab` — switch focus
- `Enter` — open detail pane
- `F3` — cycle agent filter
- `F5` — cycle time filter
- `F12` — cycle ranking mode

## Supported Agent Sources

| Agent | Default location(s) | Env overrides |
|---|---|---|
| Claude Code | `~/.claude/projects` | — |
| Codex | `~/.codex` | `CODEX_HOME` |
| OpenCode | `~/.local/share/opencode/storage` | `OPENCODE_STORAGE_ROOT` |
| Pi Agent (+ shiv/openclaw layouts) | `~/.pi/agent`, `~/.local/share/shiv`, `~/.openclaw` | `PI_CODING_AGENT_DIR`, `SHIV_AGENT_DIR`, `OPENCLAW_HOME` |

## Data Location

Default data dir is your local data directory + `/sess` (for example, often `~/.local/share/sess` on Linux).
You can override with:

```bash
sess --data-dir /custom/path ...
```

## Keeping the index fresh

`sess search` and the TUI auto-refresh a stale index (default threshold 15
minutes). Tune or disable via global flags:

```bash
sess --max-age 1h search foo
sess --no-refresh search foo   # use whatever's on disk
sess --no-auto-index search foo  # don't index automatically at all
```

For users who never keep the TUI open, an optional systemd-user timer is
shipped under `contrib/systemd/`:

```bash
mkdir -p ~/.config/systemd/user
cp contrib/systemd/sess-index.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now sess-index.timer
```

The service defaults to `sess --no-semantic index` so background runs
never silently download or initialize the embedding model. See
[`contrib/systemd/README.md`](./contrib/systemd/README.md) for the
semantic override.

## Documentation

- Architecture and deep project assessment: [`ARCHITECTURE.md`](./ARCHITECTURE.md)
- Contributing guide: [`CONTRIBUTING.md`](./CONTRIBUTING.md)
- Prioritized roadmap: [`ROADMAP.md`](./ROADMAP.md)

## Security Note

Session transcripts may include sensitive prompts/code/secrets. Treat your index data (`sess.db`, Tantivy index) as sensitive local data.
