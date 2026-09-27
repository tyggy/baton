# Baton

See and drive your Claude Code and Codex sessions from your iPhone, your Mac
and your terminal.

Status: early. The engine is being ported from a Python prototype to Rust —
see [PLAN.md](PLAN.md).

## Layout

| Path | What |
|---|---|
| `crates/baton-core` | Session engine: Claude + Codex transcript parsing (incremental) |
| `crates/baton-daemon` | `baton` binary: daemon, TUI, setup |
| `apple/` | BatonKit, iPhone and Mac apps (to be moved in) |
| `relay/` | Notification relay (Cloudflare Worker) |
| `bench/` | Parity harness against the Python reference |
| `docs/` | API contract |

## License

MIT OR Apache-2.0.
