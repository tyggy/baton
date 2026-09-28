# Baton for Mac (v0)

Menu-bar app (no Dock icon) that hosts the `baton` daemon and walks you
through setup. It exists because of INC-001/INC-002 (`docs/incidents.md`): a
bare launchd job that touches ~/Documents or sends Apple Events to Terminal
blocks on macOS privacy prompts nobody can see. Here the daemon runs as a
LaunchAgent owned by a signed app (`SMAppService`), so prompts and grants
belong to "Baton".

## Build

```sh
# needs Xcode, xcodegen, cargo
CARGO_TARGET_DIR=/path/to/cargo-target DERIVED_DATA=/path/to/derived ./build.sh
# other identity: ./build.sh DEVELOPMENT_TEAM=ABCDE12345 BATON_BUNDLE_ID=org.example.baton.mac BATON_DAEMON_LABEL=org.example.baton.daemon
```

`build.sh` runs `cargo build --release`, `xcodegen generate` and
`xcodebuild -allowProvisioningUpdates`. The build phase
`scripts/embed-daemon.sh` copies the daemon to `Contents/Helpers/baton`,
signs it with the app's identity (hardened runtime) and installs
`Contents/Library/LaunchAgents/<label>.plist` from `LaunchAgents/daemon.plist`.

## Layout

| Path | What |
|---|---|
| `BatonMac/Services/Daemon.swift` | SMAppService register/unregister, `launchctl kickstart -k`, `/api/health` poll every 10 s, config file |
| `BatonMac/Services/Permissions.swift` | Automation → Terminal, Documents (background probe, 3 s deadline), Accessibility, Notifications |
| `BatonMac/Services/Tailscale.swift` | read-only `tailscale status --json` / `serve status --json` |
| `BatonMac/Services/Pairing.swift` | pairing payload + QR |
| `BatonMac/Views/` | menu, onboarding window, pairing window |

## Daemon config

The LaunchAgent has fixed arguments:
`baton serve --config ~/Library/Application Support/Baton/daemon.conf`
(the daemon expands `~/`). The app writes `listen=` / `upstream=` there
before each start. Defaults are the shadow setup, `127.0.0.1:19880` →
upstream `http://127.0.0.1:19877`. Daemon logs go to `~/Library/Logs/Baton`
(`BATON_LOG_DIR`), stdout/stderr to `/tmp/baton-daemon.{out,err}.log`.

## Launch arguments (scripted checks)

- `--daemon register|unregister`
- `--show onboarding|pairing`, plus `--snapshot <png>` to render that window
  to a file after 6 s (no Screen Recording permission needed)
