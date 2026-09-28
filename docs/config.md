# Configuration

`baton serve` reads one config file, `~/.config/baton/config.toml`
(`$XDG_CONFIG_HOME/baton/config.toml` when `XDG_CONFIG_HOME` is set).
`baton setup` writes it; you can edit it by hand. Nothing personal is built
in: every path defaults to something under your home directory, and the
personal integrations (knowledge base, entity catalog) stay off until you
turn them on.

See what the daemon will actually use, and where each value comes from:

```sh
baton config          # effective config + the source of every key
baton doctor          # the same, plus health, Tailscale, permissions, errors
```

## Precedence

Highest first:

1. **Command-line flags**: `baton serve --listen … --upstream … --relay …
   --claude-dir … --codex-dir … --kb-path …`
2. **Environment variables** (table below).
3. **Config files**: `~/.config/baton/config.toml`, then the file given with
   `--config FILE` (or `BATON_CONFIG`) on top of it.
4. **Defaults**.

## Keys

| Key | Default | Env | What |
|---|---|---|---|
| `listen` | `"127.0.0.1:19877"` | `BATON_LISTEN` | Address the daemon listens on. `baton setup` suggests `0.0.0.0:19877` so a phone on your LAN or tailnet can connect directly. 19877 is the one default port: phones paired with earlier builds use it. |
| `upstream` | none | `BATON_UPSTREAM` | The Python reference server, for endpoints the daemon doesn't implement yet. With no upstream those endpoints answer **501** with a JSON `error` naming the endpoint. `""`, `"none"` or `"off"` = none. |
| `relay` | none | `BATON_RELAY_URL` | Push relay for notifications (docs/relay.md). Unset = no push. |
| `claude_dir` | `"~/.claude"` | `BATON_CLAUDE_DIR` | Claude Code's data dir (`projects/`, `tty_session_map.json`, …). |
| `codex_dir` | `"~/.codex"` | `BATON_CODEX_DIR` | Codex's data dir (`sessions/`). |
| `asset_roots` | see below | `BATON_ASSET_ROOTS`, `CSM_ASSET_ROOTS` (comma-separated) | Directories `/api/asset` may serve files from. |
| `kb_path` | off | `BATON_KB_PATH`, `CSM_BASE_PATH` | Optional personal knowledge base. When set, session markdown exports in `<kb_path>/sessions.nosync/` are used as a preview cache (`CSM_SESSIONS_CACHE_DIR` overrides that dir). |
| `entities` | `false` | `BATON_ENTITIES` | Optional entity catalog integration: reads `~/.baton/entities.json` and `~/.baton/session_entities.json` for list rows and `/api/entity-logo`. Off: those files are ignored and `/api/entity-logo` answers 404. |
| `project_groups` | none | — | Table of project → group label for the session list (below). |

### `asset_roots`

Phones open files that sessions mention (screenshots, plans, markdown)
through `/api/asset`. A file is served only if its real path (symlinks
resolved) is under one of the roots and it isn't a secret: `.env*`,
`.netrc`, `auth.json`, `credentials*`, private keys (`id_*`, `*.pem`,
`*.key`, `*.p8`, …) and anything under `.git/`, `.ssh/` or `.gnupg/` are
always refused.

Default (key unset): your **home directory, minus its hidden top-level
entries (`~/.ssh`, `~/.config`, `~/.aws`, …) and `~/Library`**, plus the two
hidden dirs Baton writes attachments to, `~/.baton/pastes` and
`~/.claude/plans`. Setting the key replaces the whole list:

```toml
asset_roots = ["~/Documents", "~/Downloads", "~/Desktop", "/tmp", "~/code"]
```

### `project_groups`

Keys are a directory path (`~/…` or absolute), or a project dir name as it
appears under `~/.claude/projects` (`-Users-me-code-app`). The placeholders
`{home}` (`-Users-<you>`) and `{prefix}` (`-Users-<you>-Documents-`) work
in dir names, as in the Python server's `area_map.json`.

```toml
[project_groups]
"~/code/my-app" = "My App"
"{prefix}GitHub-some-repo" = "Some Repo"
```

Sessions in your home directory itself are grouped as "Home".

## Other files

| Path | What |
|---|---|
| `~/.config/baton/token` | Bearer token (mode 600), created by `baton setup`. The daemon reads, in order: `BATON_BEARER`, this file, then `BATON_BEARER=` in `~/.claude-sessions/config` (the Python server's). No token = auth off (setup always makes one). |
| `~/.config/baton/devices.json` | Registered push devices (`BATON_DEVICES_FILE`). |
| `~/Library/LaunchAgents/com.baton.daemon.plist` | Login service written by `baton setup`: runs `baton serve` with no arguments, so everything comes from `config.toml`. |
| `~/Library/Application Support/Baton/daemon.conf` | The Mac app's file (below). |
| `~/Library/Application Support/Baton/pairing.json` | Addresses for `GET /api/pairing` (no token inside), written by the Mac app or `baton setup` / `baton pair` (`BATON_PAIRING_FILE`). |
| `~/Library/Logs/Baton/` | Structured logs when started by `baton setup`'s agent or the Mac app (`BATON_LOG_DIR`; default `~/Library/Logs`). |

## The `key=value` format (`--config FILE`, the Mac app)

Before config.toml, `baton serve --config FILE` read `key=value` lines;
that still works, and it is what the Mac app writes to `daemon.conf` (its
LaunchAgent runs `baton serve --config ~/Library/Application Support/Baton/daemon.conf`).
A `--config` file is TOML if it parses as TOML, else `key=value`:

```
# Written by Baton.app
listen=127.0.0.1:19880
upstream=http://127.0.0.1:19877
relay=https://relay.example.com
asset_roots=~/Documents, /tmp
kb_path=~/Notes
entities=true
project_groups.~/code/app=App
```

Lists are comma-separated; booleans are `true/false/yes/no/1/0`; empty
values are ignored. Keys it sets override `config.toml` (it is layered on
top), so with the Mac app, `listen`/`upstream` come from the app and
everything else from `config.toml`.

## Other environment variables

| Variable | What |
|---|---|
| `BATON_CONFIG` | Extra config file, same as `--config`. |
| `BATON_BEARER` | Bearer token (overrides the token file). |
| `BATON_LOG_DIR` | Structured log directory. |
| `BATON_DEVICES_FILE`, `BATON_PAIRING_FILE` | Push devices / pairing file paths. |
| `BATON_SESSION_PREFIX` | tmux session name prefix for spawned sessions (default `claude-`). |
| `BATON_CSM_DIR` | Python app dir holding `area_map.json` / `session_scores.json` (default: the one the upstream reports). |
| `CS_WHISPER_CPP`, `CS_MLX_PYTHON`, `CS_MLX_WHISPER_MODEL` | Voice transcription backends. |
