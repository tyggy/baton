# Migrating from the Python server

If you ran the Python prototype (`claude-session-manager/server/api_server.py`
under the `com.workbench.cs-server` LaunchAgent), the Rust daemon used to
carry a few of that setup's values built in: the asset roots, the
`~/Documents/Claude` knowledge-base path, one hard-coded project group, and
always-on entity catalog reading. Those now live in the config file. Until
you write one, the daemon uses the generic defaults (docs/config.md), which
differ from what you're used to.

## One command

```sh
baton setup
```

When there is no `~/.config/baton/config.toml` yet, setup looks for the
Python server and writes the config that behaves the same. Read-only: it
reads the plist with `plutil`, never unloads or edits it.

| Found | Becomes |
|---|---|
| `~/Library/LaunchAgents/com.workbench.cs-server.plist`, `--port N` | `upstream = "http://127.0.0.1:N"`; `listen = "127.0.0.1:19880"` (the daemon runs beside Python, which keeps its port) |
| `CSM_ASSET_ROOTS` in the plist env or its `CSM_ENV_FILE`s, else the default list in the server's `csm_config.py` | `asset_roots = [...]` |
| `CSM_BASE_PATH`, else the default in `csm_config.py` (`~/Documents/Claude`) | `kb_path = "..."` |
| the hard-coded `AREA_MAP` entries in the server's `sessions_core.py` | `[project_groups]`, e.g. `"{prefix}GitHub-some-repo" = "Label"` |
| (the prototype always read `~/.baton/entities.json`) | `entities = true` |
| `BATON_BEARER=` in `~/.claude-sessions/config` | kept where it is; no new token, so paired phones keep working |

Preview it first, or re-import later over an existing file:

```sh
baton setup --dry-run      # shows the config it would write; changes nothing
baton setup --migrate      # fill keys missing from an existing config.toml
```

If the Mac app hosts the daemon (`~/Library/Application Support/Baton/daemon.conf`
exists), setup doesn't install a LaunchAgent; the app's `listen`/`upstream`
still win over `config.toml`, and everything else comes from `config.toml`.
Restart the daemon from the app (Settings → Advanced) to pick it up.

## Checking it

```sh
baton config    # every key and where it came from
baton doctor    # also: is the daemon up, does the upstream answer
```

The regression suite runs the daemon with this migrated config (asset
roots) and must stay green: `python3 bench/regress/run.py --target daemon`.
The list/read parity suites compare the daemon with the live Python server
and are byte-identical with the migrated config (`BATON_CONFIG=<file>`).

## Cut-over (later)

When the daemon implements everything and the Python server can go:

1. Move Python off 19877 (or stop it): edit its plist's `--port`.
2. In `config.toml`: `listen = "0.0.0.0:19877"` and either
   `upstream = "http://127.0.0.1:<python port>"` or `upstream = ""`.
3. `baton setup` (reloads the LaunchAgent), or restart from the Mac app.
4. If you use `tailscale serve`, point it at the daemon's port.

Phones keep the same address and token.
