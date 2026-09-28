# Baton

See and drive your Claude Code and Codex sessions from your iPhone, your Mac
and your terminal.

Status: early. The engine is being ported from a Python prototype to Rust —
see [PLAN.md](PLAN.md).

## Quickstart (macOS)

**1. Install** the `baton` binary (Apple silicon and Intel):

```sh
brew install tyggy/baton/baton
# or
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/tyggy/baton/releases/latest/download/baton-daemon-installer.sh | sh
```

You also need [tmux](https://github.com/tmux/tmux) (`brew install tmux`),
plus Claude Code and/or Codex.

**2. Set up** — config, bearer token, login service, pairing code:

```sh
baton setup
```

It asks two questions (listen address, push relay), writes
`~/.config/baton/config.toml` and a token in `~/.config/baton/token` (mode
600), installs a LaunchAgent that runs `baton serve` at login, checks that
`tmux`, `claude` and `codex` are on your PATH, and prints a QR code.
Re-running it is safe: it keeps what's there. `baton setup --yes` accepts
the defaults; `--dry-run` shows what it would do.

**3. Pair your iPhone**: in the Baton app, scan the QR code (or paste the
pairing code printed under it). Show it again any time with `baton pair`.

**4. Optional — reach your Mac from anywhere** with
[Tailscale](https://tailscale.com): with the Mac and the phone on the same
tailnet, and HTTPS enabled for the tailnet,

```sh
tailscale serve --bg --https=443 http://127.0.0.1:19877
```

The pairing code already carries your Mac's `https://<mac>.<tailnet>.ts.net`
address, so a phone paired before this step picks it up.

**Something off?** `baton doctor` checks the install, config, daemon,
Tailscale, permissions and recent errors; `baton doctor --bundle` writes a
redacted archive to attach to a bug report.

Configuration: [docs/config.md](docs/config.md). Coming from the Python
prototype: [docs/migrating.md](docs/migrating.md).

## Layout

| Path | What |
|---|---|
| `crates/baton-core` | Session engine: Claude + Codex transcript parsing (incremental) |
| `crates/baton-daemon` | `baton` binary: daemon (`serve`), `setup`, `pair`, `doctor` |
| `apple/` | Mac app (hosts the daemon), push crypto |
| `relay/` | Notification relay (Cloudflare Worker) |
| `bench/` | Parity harness against the Python reference, regression suite |
| `docs/` | API contract, config, migration, relay |

## Development

```sh
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test --all
python3 bench/regress/run.py --target daemon      # regression suite (sandboxed)
```

Releases are built by [cargo-dist](https://opensource.axo.dev/cargo-dist/)
(`dist-workspace.toml`, `.github/workflows/release.yml`): pushing a `v*` tag
builds macOS arm64 + x86_64 archives, the shell installer and the Homebrew
formula for the `tyggy/homebrew-baton` tap. Preview with `dist plan`.

## License

MIT OR Apache-2.0.
