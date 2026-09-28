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

## Copy to Open

Copy a path in a terminal — `src/api/read.rs:120`, `read.rs`, `@docs/api.md`,
`~/x/y.md`, `file:///…` — and Baton opens it. The pasteboard's change count
is polled every 0.4 s; contents are read only when the frontmost app is a
terminal (editable list), the copy isn't marked concealed/transient
(password managers), and paste access isn't denied. Detection and
resolution live in the AppKit-free package `apple/PathDetect` (unit tests:
`swift test` there), so the iPhone app can reuse them.

Partial paths are resolved against, in order: the frontmost tab's tmux
session cwd (Terminal tab tty → `tmux list-clients`; other terminals: the
client with the latest activity), Claude sessions active in the last 24 h
(daemon `/api/local-sessions` → their tmux panes' cwd), other tmux sessions
active in the last 24 h, each followed by its git root. Bare names and
unjoined relative paths get a bounded search (depth 6, 200 ms, skips
`.git`/`node_modules`/`target`/`build`/`.venv`/DerivedData…). One hit
opens; several show a chooser; none shows a short "no match" (setting).
tmux is only read (`list-clients`, `list-panes`).

Open: directories reveal in Finder; with a line number, VS Code / Cursor /
Zed / BBEdit open at the line through their URL schemes, Xcode through
`xed -l`, Sublime through `subl file:line`; otherwise the default app.
Apps, installers, `.command`/`.terminal` files and executables (anything
whose "open" would run it) are revealed instead. The HUD (top right,
non-activating, ~3 s) has Reveal and Quick Look. Settings → Copy to Open:
mode (Open / Quick Look / Ask First), editor for line numbers, terminal-only
and its app list, "no match" hint, paste-access status. Menu → Open Copied
Paths pauses it.

Debug: `Baton --clip-resolve 'src/api/read.rs:120' [--clip-base DIR]`
prints the candidate, bases and matches as JSON (no pasteboard, opens
nothing); `--clip-debug` logs every clipboard step to stderr.
Screenshots: `docs/clip-hud-{opened,choose}-light.png`,
`docs/clip-hud-opened-dark.png`, `docs/settings-clip-{light,dark}.png`
(`--show hud --hud opened|ask|choose|nomatch`, `--show settings
--settings-tab clip`).

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
| `BatonMac/Services/ClipWatcher.swift` | Copy to Open: pasteboard polling, gates, settings |
| `BatonMac/Services/ClipContext.swift` | base directories (tmux read-only, daemon session list) |
| `BatonMac/Services/ClipActions.swift` | open / open at line / reveal / Quick Look, run-safety check |
| `BatonMac/Views/ClipHUD.swift` | the HUD panel and chooser |
| `../PathDetect/` | path-like test, `:line` parsing, resolution (Foundation only) |

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
