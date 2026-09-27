# Baton — plan

Baton lets you see and drive your Claude Code and Codex sessions from your
iPhone, your Mac and your terminal. Free and open source.

## Shape

```
                baton serve  (Rust daemon, runs at login)
                  │  HTTP API + live deltas (SSE) · tmux · transcripts
                  │  remote access: tailscale serve (HTTPS on *.ts.net)
                  │  notifications → relay (end-to-end encrypted)
      ┌───────────┼──────────────────────┬──────────────────────┐
   Baton iOS   Baton Mac app          baton (TUI)            relay
   App Store   notarized, menu bar    brew / curl            Cloudflare Worker
      └──── BatonKit (Swift): models · API client · live list · viewers ────┘
```

| Piece | What it is | Ships as |
|---|---|---|
| `baton serve` | Headless daemon: session engine (Claude + Codex transcripts, incremental parsing), tmux (stream, scroll, send, resize, spawn/resume), HTTP API with live deltas, push to relay | Rust binary; `brew install baton` or `curl -fsSL …/install.sh \| sh` |
| `baton` | Terminal UI (today's `cs`), a client of the daemon | same binary |
| `baton setup / pair / doctor` | Hooks, launchd, `tailscale serve`, pairing QR, bug-report bundle | same binary |
| Baton for Mac | Menu bar + main window: live list, search (⌥Space), pairing, status, embedded terminal (local `tmux attach`), terminal window management (holds Automation/Accessibility) | notarized app; `brew install --cask baton`; bundles the daemon |
| Baton for iPhone | Live list, chat, streamed terminal, assets, notifications | App Store (free) |
| relay | Forwards encrypted notification payloads to APNs; holds the APNs key; stores nothing | Cloudflare Worker; self-hostable |

## Decisions

- **Free, open source** (MIT OR Apache-2.0), everything in this repo.
- **Remote access via the user's Tailscale** (`tailscale serve` → valid HTTPS
  cert on `<mac>.<tailnet>.ts.net`; no ATS exceptions). No stream relay.
- **Notifications via a relay we run** (cheap: small encrypted payloads,
  APNs is free). Self-hostable; the daemon takes a relay URL.
- **Rust daemon**, ported slice by slice behind the existing HTTP API. The
  Python server (`claude-session-manager/server/api_server.py`) is the
  parity reference until each slice is replaced.
- **One API, three clients.** The TUI and the Mac app become clients of the
  daemon — one engine, live deltas everywhere.
- **Personal integrations become optional plugins**, off by default
  (Airtable entities, Haiku titles/classifier with the user's own key,
  knowledge-base search).

## Migration: strangler, gated by parity

The daemon proxies any endpoint it doesn't implement yet to the Python
server, so every step ships. A slice is promoted only when the parity
harness shows identical output on the full local session set and the
benchmarks show it at least as fast.

| Slice | Scope | Parity gate |
|---|---|---|
| 1. core | Claude + Codex transcript parsing (resumable, incremental), activity signature | per-file dict equality vs `sessions_core` on every transcript; incremental == full at random cut points |
| 2. list | grouping, titles, `/api/local-sessions`, `/api/search-index`, `/api/events` deltas | payload digest vs Python; delta stream equivalence |
| 3. read | `/api/session`, `/api/messages`, `/api/tail`, `/api/asset`, `/api/children` | `bench/sweep.py` 100% on 40 newest; asset bench |
| 4. tmux | `/api/pane-stream`, `/api/scroll`, `/api/send`, `/api/send-key`, resize | frame byte-equality on bench panes; scroll/pane benches |
| 5. control | spawn/resume, Codex thread mapping, hooks, push to relay | phone journey + TUI actions |
| 6. TUI | ratatui client of the daemon | TUI behaviour checks |

## Milestones

**M0 — foundations (now)**
- [x] Commit the evolution work (branches `baton-evolution` in the three current repos).
- [x] Monorepo skeleton, license, plan.
- [x] `docs/api.md`: the API contract as the Python server implements it.
- [x] Slice 1 in Rust with the parity harness green (752/752 transcripts, 7.6x faster, incremental 84/84).
- [x] CI: fmt, clippy, tests.
- [ ] CI: parity on synthetic fixtures (real transcripts stay local), Swift build.
- [ ] Push to GitHub (public repo).

**M1 — daemon replaces the Python server on your Mac**
- Slices 2–5, `baton serve` as the launchd service, proxy removed.
- `tailscale serve` HTTPS; Baton switched to `https://<mac>.<tailnet>.ts.net`.

**M2 — Mac app + BatonKit**
- Extract `BatonKit` from the iOS app; macOS target on it.
- Search-hub's hotkey panel becomes the Mac app's search.
- Pairing QR, status, embedded terminal, window management.

**M3 — installable by others**
- `baton setup/pair/doctor`; config file (no personal paths or hosts).
- cargo-dist: curl installer, Homebrew tap, GitHub Releases; notarized app + cask.
- Relay (Cloudflare Worker) + end-to-end encrypted notifications.
- iOS: QR pairing, demo mode (App Review + first run), HTTPS only.
- TestFlight beta, 5–10 users.

**M4 — public**
- App Store; docs site; `baton.sh/install`.

## Release process

- **Versions:** semver for the daemon/CLI; `api_version` + `min_client`
  in `/api/health`. The daemon supports clients one version back; clients
  show "update your Mac" when too far apart.
- **Channels:** `main` → beta tags (brew `--HEAD`/beta, TestFlight) →
  stable tags (brew, App Store). Stable every 2–3 weeks after a week of
  beta soak; hotfixes any time.
- **Gates (CI):** unit + parity tests, TUI behaviour checks, benchmark
  regression vs the last release. **Pre-release (manual):** phone journey,
  canary deploy with the error-log gate.
- **Tooling:** cargo-dist for binaries/installers/tap; `xcodebuild archive`
  + App Store Connect API key for TestFlight; Sparkle or brew for the Mac app.
- **Bug reports:** `baton doctor` bundles versions, structured error logs,
  recent requests.

## Performance and robustness baseline (from the Python server, 2026-09-27)

Kept as the numbers the Rust daemon must match or beat:

| Measure | Value |
|---|---|
| warm list rebuild (isolated) | ~15 ms load, ~140 ms total |
| active transcript update | 0.4–1.8 ms incremental (221 MB file: 415 ms full) |
| `/api/scroll` under live rebuilds | p50 14 ms, p99 43 ms |
| same-size pane open | 25–36 ms |
| phone: fresh list after launch | 300 ms |
| asset reopen | 304, ~170 bytes |
| live delta latency | 2–4 s after the write |
