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

## First run

A four-step setup window (after Apple's own setup sheets; Liquid Glass
buttons, macOS 26+), whose only goal is "your iPhone is connected". Each step
advances by itself once its check passes:

1. **Welcome** — Continue registers the background item and waits for it to answer.
2. **Terminal** — pre-alert with a single Continue that triggers the system
   Automation prompt; a denial gets its own screen (Open System Settings /
   Continue Without It).
3. **Scan with your iPhone** — QR pairing code; "Waiting for your iPhone…"
   completes when the phone registers (`~/.config/baton/devices.json`) or a
   non-local client shows up in the request logs.
4. **Reach your Mac away from home** — shows only the next Tailscale action
   (install, sign in, HTTPS certificates, then "Turn On Remote Access", which
   runs `tailscale serve --bg --https=443 http://127.0.0.1:<port>` on click
   only). "Set Up Later" skips it.

Then a brief "You're all set" pointing at the menu bar; `onboardingComplete`
stops the window from opening again. Menu → Show Setup Again… reopens it.
Notifications, Accessibility and Documents are not asked during setup; they
live in Settings → Status. Settings → Advanced has the listen/upstream
addresses, logs and "Show Setup Again…".

Screenshots: `docs/onboarding-step-{1..5}-{light,dark}.png`,
`docs/settings-status-light.png`.

## Layout

| Path | What |
|---|---|
| `BatonMac/Services/Daemon.swift` | SMAppService register/unregister, `launchctl kickstart -k`, `/api/health` poll every 10 s, config file |
| `BatonMac/Services/Permissions.swift` | Automation → Terminal, Documents (background probe, 3 s deadline), Accessibility, Notifications |
| `BatonMac/Services/Tailscale.swift` | read-only `tailscale status --json` / `serve status --json` |
| `BatonMac/Services/Pairing.swift` | pairing payload + QR |
| `BatonMac/Views/Onboarding/` | setup flow (`SetupFlow`, `Pages`, `SetupScaffold`) |
| `BatonMac/Views/Settings/` | Settings scene: Status and Advanced tabs |
| `BatonMac/Services/PairingWatcher.swift` | detects the first iPhone connection |

## Daemon config

The LaunchAgent has fixed arguments:
`baton serve --config ~/Library/Application Support/Baton/daemon.conf`
(the daemon expands `~/`). The app writes `listen=` / `upstream=` there
before each start. Defaults are the shadow setup, `127.0.0.1:19880` →
upstream `http://127.0.0.1:19877`. Daemon logs go to `~/Library/Logs/Baton`
(`BATON_LOG_DIR`), stdout/stderr to `/tmp/baton-daemon.{out,err}.log`.

## Launch arguments (scripted checks)

- `--daemon register|unregister`
- `--show onboarding|pairing|settings`, `--step N` (pin the setup flow to a
  step: no side effects, no auto-advance), `--appearance light|dark`
- `--snapshot <png>` renders the shown window after 6 s with ScreenCaptureKit's
  current-process content (no Screen Recording permission), then quits. The
  pairing token is replaced with a placeholder in this mode.

```sh
B=.../Baton.app/Contents/MacOS/Baton
for n in 1 2 3 4 5; do for ap in light dark; do
  $B --show onboarding --step $n --appearance $ap --snapshot docs/onboarding-step-$n-$ap.png
done; done
```
