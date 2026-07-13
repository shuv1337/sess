# sess oxmgr unit

Optional background refresh for users who run [oxmgr](https://github.com/Vladimir-Urik/OxMgr)
instead of (or in addition to) the systemd-user timer under `contrib/systemd/`.

## Why a loop, not a oneshot

Oxmgr supervises long-running processes. A bare `sess index` is a oneshot that
exits 0 immediately — pairing that with `restart_policy = "always"` would spin
forever. The wrapper runs `sess index` then sleeps, so oxmgr has one stable PID
to supervise.

## Drop-in for `~/.config/oxmgr/oxfile.toml`

```toml
[[apps]]
name = "sess-index"
command = "/home/you/repos/sess/contrib/oxmgr/sess-index-loop.sh"
cwd = "/home/you"
restart_policy = "always"
max_restarts = 50
restart_delay_secs = 30
stop_timeout_secs = 30
namespace = "sess"

[apps.env]
HOME = "/home/you"
PATH = "/home/you/.local/bin:/usr/local/bin:/usr/bin:/bin"
# Override interval (seconds). Default 900 = 15 minutes.
# SESS_INDEX_INTERVAL_SECS = "900"
# Set to 1 to skip fastembed (matches systemd unit default).
# SESS_INDEX_NO_SEMANTIC = "1"
```

Then:

```sh
oxmgr apply ~/.config/oxmgr/oxfile.toml
oxmgr logs sess-index -f
```

## Semantic embeddings

Unlike the systemd unit (which defaults to `sess --no-semantic index`), this
loop runs **with** semantic embeddings so new/updated conversations get
vectors. To match the conservative systemd default:

```toml
[apps.env]
SESS_INDEX_NO_SEMANTIC = "1"
```
