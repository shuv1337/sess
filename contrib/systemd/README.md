# sess systemd user units

Optional background refresh for users who want the index to stay fresh
without keeping the TUI open or remembering to run `sess index`.

## Install (per-user)

```sh
mkdir -p ~/.config/systemd/user
cp sess-index.service sess-index.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now sess-index.timer
```

Check it's working:

```sh
systemctl --user list-timers | grep sess
journalctl --user -u sess-index.service -n 50
```

## Disable

```sh
systemctl --user disable --now sess-index.timer
```

## Semantic embeddings

The service defaults to `sess --no-semantic index` so the timer never
silently downloads or initializes the `fastembed` model in the
background. If you want semantic embeddings refreshed on the timer:

```sh
systemctl --user edit sess-index.service
```

…and set:

```
[Service]
ExecStart=
ExecStart=%h/.cargo/bin/sess index
```

(The empty `ExecStart=` clears the inherited value; the second line
sets the new one.)
