# Incident catalog (input for the regression suite)

Every real failure the user hit, or that we found, in Baton (iOS), the `cs`/`ccs` sessions TUI,
the Python API server / Rust daemon, deploys, and search-hub. Each entry names an automated check
that would have caught it. This file does not contain the tests; `bench/regress/` will be built
from it.

**Sources:** the user's messages in Claude Code sessions `d4039fa0` (search-hub project, Apr–Sep),
`297f2d18` (Apr, iPhone remote phases 2–9), `70a9a2ba` (May, Baton connect/voice), and the
watcher sessions `23b56636` / `48025373` / `77a60279`. Git history of `workbench-exporter-master`
(incl. `baton-evolution`), `~/Superapp-Projects/baton`, `~/GitHub/baton` (`port/*`,
`integration`), and search-hub. `~/Library/Application Support/cs-server/deploys.jsonl`.
Logs: `cs-server.{requests,errors,client,events}.jsonl`, `cs-server.err.log`,
`cs-tui.startup.jsonl`, `~/.claude/.logs/sessions_tui.log`. Rust-port parity findings.

**Times:** local (PDT) unless marked Z. Transcript timestamps are UTC, so the user's
"17:24" message on 2026-09-27 appears as 2026-09-28T00:24Z in the session file.

**How a check runs** (the `Runs on` field):

| Tag | Meaning |
|---|---|
| `ANY` | Black-box HTTP against any server port (Python, daemon, canary). Read-only or bench-pane-only. |
| `SANDBOX` | Needs a fake world: temp `HOME` with fixture transcripts, `csm-bench-*` tmux panes, fake `claude`/`codex` binaries on `PATH`, ps/tmux shims. Never touches live sessions. |
| `UI` | Swift unit/XCUITest (stub transport) or Textual Pilot (headless TUI). |
| `PHONE` | Needs the real iPhone (network path, backgrounding, iOS lifecycle). Final confirmation only. |
| `DEPLOY` | Checks on the deploy/launchd pipeline (plist, start-up, single listener, release hash). |
| `LOG` | A monitor over the structured logs (runs after a live trial or canary). |

---

## Summary

### Counts by component (primary component; 68 entries)

| Component | Entries | Open or unfixed (incl. latent Python bugs) |
|---|---|---|
| Server / daemon (Python `api_server.py` + `sessions_core.py`, Rust `baton serve`) | 42 (15 of them parity-found, INC-043–057) | 22 (INC-001, 012, 031, 039, 040, 041, 042, and 043–057 in Python) |
| Baton iOS | 9 | 2 (INC-014; INC-018 copy part) |
| cs / ccs TUI | 9 | 2 (INC-033, 066) |
| Infra / deploy (launchd, deploy script, host) | 6 | 2 (INC-002, 004) |
| search-hub | 2 | 0 |

User-reported incidents (with a quote from the user): 25. Found by us (logs, parity, bench, deploy gate): 43.

Severity tiers used for ordering: **S1** outage or cannot use; **S2** core feature broken or
wrong data shown; **S3** slow enough that the user noticed or reported; **S4** latent robustness
bug (500s, crashes on bad input, silent corruption) found by tests, parity or logs; **S5**
cosmetic / annoyance.

### Top 10 checks to build first

| # | Check | Covers | Runs on |
|---|---|---|---|
| 1 | **Live pane ↔ row mapping.** For every live tmux pane (fake `claude --resume <id>`, `claude --chrome --resume <id>`, `codex resume <id>`), the row with that id in `/api/local-sessions` has `tmux_session == <pane>` and `origin` right; a stale tty-map entry never overrides the pane's own process. Also asserted on the first list served after process start (prewarm). | INC-001, 011, 015 | SANDBOX |
| 2 | **Resume by origin.** `POST /api/resume` for a Codex id spawns a pane whose command is `codex resume <id>` (and finds an existing `codex-<short>` pane); for a Claude id, `claude --resume <id>` in the right cwd; unknown id → 4xx, no pane. | INC-001, 012 | SANDBOX |
| 3 | **Deadline sweep.** For every row in `/api/local-sessions`: `/api/session/{id}`, `/api/messages/{id}?limit=50`, 2 s of `/api/tail/{id}`, `/api/children/{id}` each return 200 within 10 s; whole sweep < 120 s; any timeout fails with the path. Run against the candidate port before every cutover. | INC-001, 017, 028, 029 | ANY |
| 4 | **Tail semantics.** `/api/tail` starts at EOF (0 records in first 2 s), `?since=` replays only newer records, a record written in 3 pieces arrives once and whole, Codex rollouts stream Claude-shaped records. | INC-016 | SANDBOX |
| 5 | **Malformed-input fuzz.** Every POST endpoint with invalid JSON, wrong types (`session: 1`, `keys: 1`, `ticks: "x"`), missing fields, pipelined bodies on one keep-alive connection, short/long `Content-Length`; every path param with `;`, `*`, `?`, `[`, `..%2F`, non-UTF-8 → 4xx, never 500, connection state sane. | INC-038, 040, 041, 043–057 | ANY (+SANDBOX for tmux-less cases) |
| 6 | **Liveness under streams.** Hold 20 SSE streams (`/api/events`, `/api/pane-stream/<bench>`, `/api/tail/<id>`), then 20× `/api/health` and 5× `/api/local-sessions`: all < 500 ms. | INC-025, 026 | ANY |
| 7 | **Start-up under launchd.** Exec the plist's `ProgramArguments` with the plist env in a temp HOME: arguments parse, port bound < 10 s, no filesystem walk at import time; second instance on the same port exits non-zero once (no tight loop); `kill -9` → respawned < 15 s. | INC-002, 003, 004, 039 | DEPLOY + SANDBOX |
| 8 | **Asset policy.** Allowed roots → 200; secrets (`.env`, `*.p8`, `*.pem`, `.git/config`, Claude credentials, Codex `auth.json`) and `..` traversal → 4xx; > 100 MB → 413 in < 100 ms with flat RSS; latin-1 filename → 200; unchanged reopen → 304. | INC-019, 020, 051 | SANDBOX |
| 9 | **Terminal input correctness.** On bench panes with a mouse-reporting fake app: `/api/scroll` lands at pane centre, exactly one tmux call per scroll, p90 < 100 ms under continuous rebuilds; scroll/send/send-key to a missing pane → 404 < 100 ms; name `csm-bench-a` never hits `csm-bench-ab`; `a;b` rejected. | INC-021, 030, 031, 049, 050 | SANDBOX |
| 10 | **Baton stream/connection recovery (client).** Stub transport: probe timeout then success → reconnects without user action; pane stream closed/errored → reconnect in ≤ 1 s, orange not red; foreground after 45 s quiet → reconnect; a late callback from a replaced connection doesn't change state; sending while a stream reconnects still delivers `/api/send`. | INC-005, 013, 014 | UI (PHONE to confirm) |

---

## S1: outages and "cannot use"

### INC-001: Rust daemon cutover: Codex can't resume, sessions open as bubbles, sweep hangs
- **Date:** 2026-09-27, ~17:20–17:30.
- **Component:** server/daemon (Rust `baton serve`, `integration` branch), infra/deploy.
- **Symptom:** User, 17:24: *"codex can't resume and some sessions default to bubble view"*.
  A sweep through the daemon hung for more than 300 s, and the switch was rolled back. There was
  also 1–2 min of downtime around 17:20–17:22 from switchover mistakes, and a few more minutes
  during the rollback (see INC-002).
- **Root cause:** **open.** Every parity script passed, because they compare the daemon with
  Python on a snapshot and don't replay the live failure. Leads to check first:
  1. `/api/resume` is origin-blind in **both** Python and Rust. It only looks for
     `claude-<short>` / `claude-resume-<short>` and always spawns `claude --resume <id>`, so a
     Codex id (tmux name `codex-<short>`) can never resume through it. See INC-012.
  2. Commit `36f78a4` ("list prewarm at start") landed at 17:21, one minute before the cutover.
     A list prewarmed before the tmux map or Codex app-server mapping is ready would carry
     `tmux_session: null`. Baton then opens bubbles until the next rebuild.
  3. Sweep hang: unknown. Candidates are a missing per-request deadline on a native handler, a
     blocking call on the async runtime, or pass-through to Python while Python was down.
- **Fix:** rollback to Python (release `7bd60399f4d0`, started by hand at 17:30:49). No code fix yet.
- **Detection:**
  - Top-10 checks #1, #2 and #3 against the daemon port.
  - Add a **switchover gate**: during a 10-minute shadow trial, compare the daemon's
    `/api/local-sessions` with Python's on live data, by id. Diff these fields: `tmux_session`,
    `status`, `origin`, `state`. Any row where Python has `tmux_session` and the daemon doesn't
    fails the gate.
  - LOG: no new `fp` in the daemon's `errors.jsonl` during the trial.
- **Runs on:** SANDBOX (checks 1 and 2), ANY (sweep and shadow diff), LOG.

### INC-002: Rollback: Python under launchd hangs or fails before binding 19877
- **Date:** 2026-09-27, ~17:22–17:30.
- **Component:** infra/deploy (launchd), server.
- **Symptom:** After the rollback, Python started by launchd hung in start-up before binding the
  port, three times in a row. The same command started by hand bound in 2 s. The server now runs
  outside launchd, so it has no crash or reboot protection.
- **Root cause:** **open.** Evidence in `cs-server.err.log`:
  - 4× `api_server.py: error: unrecognized arguments: 19877 127.0.0.1`. Something (the plist, or
    `scripts/launchd.sh` in the monorepo) passes positional port/host, but argparse only accepts
    `--port/--host`.
  - `InterruptedError: [Errno 4] Interrupted system call: '~/Documents/Claude/work'`, raised at
    **import time** from `sessions_core.py:98 build_folder_tree` (called at module level, line 120,
    imported from `api_server.py:59`). An import that walks `~/Documents` (iCloud, TCC) can block
    under launchd's environment.
- **Fix:** none yet (next task in the main session).
- **Detection:**
  - DEPLOY: parse the installed plist and exec its `ProgramArguments` with `--help` appended.
    Assert that argparse accepts them.
  - SANDBOX: start from those exact args with `env -i` plus the plist's `EnvironmentVariables`
    and a temp HOME. Assert `/api/health` answers within 10 s.
  - Import-time guard: importing `sessions_core` / `api_server` with `os.scandir`/`os.listdir`
    patched to raise must succeed, so no filesystem walk happens at import.
- **Runs on:** DEPLOY, SANDBOX.

### INC-003: Server dead after tmux cleanup / reboot; Baton lists but can't connect
- **Date:** 2026-05-10 → 05-11.
- **Component:** infra/deploy, Baton iOS.
- **Symptom:** Task brief: Baton *"can't connect to claude sessions after a kill+respawn cycle
  of tmux sessions… Baton lists sessions but shows default bubble view, can't connect/stream"*.
  User: *"it was dead after computer restart - i need it working robust after restarts"*.
- **Root cause:** `api_server` wasn't running. The launchd job started it through
  `osascript → Terminal.app`, with `RunAtLoad` but no `KeepAlive`, so it died when its Terminal
  tab closed. The iOS client also kept showing its stale session list after fetch errors.
- **Fix:** new plist with `RunAtLoad` + `KeepAlive` + `ThrottleInterval=10`, a runtime copy in
  `~/Library/Application Support/cs-server/`, and `cs-server-sync`. iOS `APIClient` clears
  sessions on fetch error (2026-05-11; later superseded by `deploy-server.sh` releases).
- **Detection:**
  - DEPLOY plist lint: `KeepAlive` and `RunAtLoad` are true, and `ProgramArguments` contains no
    `osascript` or `Terminal`.
  - SANDBOX (own label and port): `kill -9 <pid>` → `/api/health` 200 again within 15 s.
  - UI: when the list fetch fails, the list shows a disconnected state, not stale rows presented
    as live.
- **Runs on:** DEPLOY, SANDBOX, UI.

### INC-004: Restart loop: "Address already in use" ×16,865
- **Date:** undated (`err.log` has no timestamps); most likely the 2026-09-26 manual-restart and
  nohup period.
- **Component:** infra/deploy.
- **Symptom:** 16,865 `OSError: [Errno 48] Address already in use` tracebacks in `cs-server.err.log`.
- **Root cause:** two starters for one port: a nohup server outside launchd, and launchd
  `KeepAlive` respawning its own copy, which failed to bind, exited, and was respawned again.
- **Fix:** deploy script verifies the release after the switch. The loop itself is unguarded.
- **Detection:**
  - DEPLOY: exactly one listener on :19877, and its pid equals launchd's job pid (or the
    recorded manual pid when the deploy log says `manual-start`).
  - SANDBOX: start a second instance on an occupied bench port. It must exit non-zero within
    2 s and write one structured line; under a KeepAlive-like supervisor it must not log more
    than 10 lines per minute.
- **Runs on:** DEPLOY, SANDBOX.

### INC-005: "baton stopped connecting" (connect failed once, never retried)
- **Date:** 2026-09-27, 11:19–11:30.
- **Component:** Baton iOS.
- **Symptom:** *"baton stopped connecting"*.
- **Root cause:** a connect failed after the 6 s health timeout at 11:19:50, and nothing retried
  until the app was relaunched at 11:30. There was also no LAN fallback by default.
- **Fix:** `8773383` (Baton):
  - keep-alive never gives up (backoff 2…30 s);
  - the old transport is kept until a new one wins;
  - the race reruns on foreground or network-path change;
  - LAN fallback on by default.
- **Detection:**
  - UI (stub transport): the first probe times out and the second succeeds. Assert connected
    within 4 s with no user action. A failed re-probe while connected must not drop the working
    transport.
  - PHONE: server down 20 s, then up. `cs-server.client.jsonl` shows a `connect` record within
    35 s of the first `connect_failed`.
- **Runs on:** UI, PHONE.

### INC-006: Baton slow to connect, especially on cellular
- **Date:** 2026-09-24/25.
- **Component:** Baton iOS, server.
- **Symptom:** *"please dwbug why baton sessions on mobile take ages to connect"*, then
  *"especially on cellular - wifi is better"*.
- **Root cause:**
  1. The health probe had no timeout (iOS default 60 s) and addresses were tried one after the
     other, the unreachable `.local` first.
  2. `/api/health` spawned two subprocesses per call, taking 2–25 s under load.
  3. Mac memory pressure (swap nearly full, load ~29).
- **Fix:** 6 s probe timeout plus a parallel race (`ConnectionManager.swift`,
  `HTTPTransport.swift`); health caches tmux/uptime for 60 s (`api_server.py`). Later gen-1/2
  keep-alive and gzip (deploys 09-26 15:13 and 15:23).
- **Detection:**
  - ANY: 20× `GET /api/health` p95 < 100 ms.
  - SANDBOX: a PATH shim counts `tmux`/`ps` spawns during 10 health calls after warm-up; the
    count must be 0.
  - UI: candidate A blackholed and B healthy → connected in < 1.5 s.
- **Runs on:** ANY, SANDBOX, UI.

### INC-007: Morning brown-out: 378 s list rebuild, scroll 500s, connect failures
- **Date:** 2026-09-27, 09:34–09:42.
- **Component:** server, infra (host load).
- **Symptom:**
  - The phone logged 29 `connect_failed` (−1001 / −1005).
  - `/api/scroll` returned 500 after 24–41 s (15 of them).
  - The `ls_rebuild` at 09:42 took `total_ms` 377,913, of which `load_ms` was 369,755.
  - Phone requests reached the Mac about 10 s late.
- **Root cause:** a full re-parse of all transcripts on every rebuild, under heavy memory
  pressure and swap, competing with terminal requests for the GIL. Part of the delay was the
  network path.
- **Fix:** S1 incremental rebuild (11:56), L3/L4 incremental parse (12:18, 12:31), L4b streamed
  parse (12:50), R8 rebuild deferral during input (13:21).
- **Detection:**
  - SANDBOX, with `bench/run.py --pressure` and a forced continuous rebuild:
    - `/api/health` p99 < 500 ms;
    - `/api/scroll` p99 < 1 s;
    - warm `ls_rebuild` < 1 s on the fixture.
  - LOG: alert on any `ls_rebuild.total_ms > 30000` and any 5xx on `/api/scroll`.
- **Runs on:** SANDBOX, LOG.

### INC-008: Midday brown-out: first incremental parse of a 221 MB transcript (+1.48 GB RSS)
- **Date:** 2026-09-27, 12:36–12:50.
- **Component:** server.
- **Symptom:**
  - 44 phone `connect_failed`.
  - `/api/health` took over 5 s eight times.
  - `ls_rebuild` 198 s at 12:45; `group_ms` 52 s at 12:47.
  - The request log is silent from 12:50:05 to 13:16:27.
- **Root cause:** the first incremental parse read, decoded and split a whole 221 MB file.
  That peaked at +1,483 MB RSS, and under memory pressure one rebuild took 52 s.
- **Fix:** L4b: parse in streamed 4 MB chunks, +328 MB peak (deploy 12:50).
- **Detection:**
  - SANDBOX: cold incremental parse of a 200 MB synthetic transcript. Peak RSS delta < 400 MB,
    and the result equals a full parse.
  - LOG: the `mem` event's `peak_rss_mb` stays under 1,000.
- **Runs on:** SANDBOX, LOG.

### INC-009: Live server was running June code (hand-copied mixed deploy)
- **Date:** found 2026-09-26.
- **Component:** infra/deploy, server.
- **Symptom:** building Baton's list took **39.8 s**. The launchd copy mixed a June
  `sessions_core` with newer files and never had the session cache.
- **Root cause:** manual file copies with no release identity.
- **Fix:** `scripts/deploy-server.sh`: immutable releases behind a `current` symlink, canary,
  verified switch, rollback, `deploys.jsonl`. `/api/health.release` is a hash of the on-disk
  sources. The first attempt hashed lazily imported modules, and two identical builds reported
  `f8e8e82b685f` vs `ad2bcb175072`; fixed.
- **Detection:**
  - DEPLOY: `/api/health.release` equals a hash of the deployed commit's sources, and all
    `files` entries resolve under one release directory.
  - ANY: cold `/api/local-sessions` < 3 s.
- **Runs on:** DEPLOY, ANY.

### INC-010: Deploy stalls under extreme load; rollback couldn't start either
- **Date:** 2026-09-26, 13:13–13:18.
- **Component:** infra/deploy.
- **Symptom:** deploy log: *"launchd xpcproxy spawn stalled (load avg 385, swap 13.5/14.3GB);
  auto-rollback could not start either; job booted out and server started via nohup outside
  launchd"*.
- **Root cause:** the deploy ran during an overload. launchd couldn't spawn in time.
- **Fix:** load preflight in `deploy-server.sh`. The first version used a wrong swap-free
  criterion; it now uses the kernel memory-pressure level ≥ 4 or load > 4× cores.
- **Detection:**
  - DEPLOY: dry-run the deploy with the pressure probe stubbed "critical". It must refuse and
    change nothing.
  - After any deploy, `launchctl print` pid equals the listener pid (see INC-004).
- **Runs on:** DEPLOY.

---

## S2: core feature broken or wrong data shown

### INC-011: Codex (and flagged Claude) tmux panes open as bubbles in Baton
- **Date:** 2026-09-26 22:12 and 2026-09-27 11:15.
- **Component:** server.
- **Symptom:** *"nice works! codex only opened as bubbles"*; after the Codex session was moved
  into tmux: *"in baton codex is still opened as bubbles"*.
- **Root cause:** the tmux → session map only matched `claude --resume <id>`. `codex resume
  <id>` panes, and `claude --chrome --resume <id>` (flags before `--resume`, which the TUI's ⇪
  upgrade generates), never matched, so the row had no `tmux_session`. A stale tty-map entry for
  `ttys026` also tied the pane `codex-01a08980` to Claude session `5c5a3354`.
- **Fix:** deploy 2026-09-27 11:16. Match `codex resume <id>` and `claude [flags] --resume <id>`;
  the pane's own processes take precedence over the tty map. Codex panes without an id in argv
  are linked via app-server and cwd (L2, 12:13). Codex desktop-app threads have no terminal and
  stay bubbles by design.
- **Detection:** top-10 check #1. Bench panes run fake `codex resume <uuidA>` and
  `claude --chrome --resume <uuidB>`, with fixture transcripts for both. Seed `tty_map` with a
  stale entry that maps the Codex pane's tty to a third session. Assert:
  - the `uuidA` row has `tmux_session == csm-bench-codex`;
  - the `uuidB` row has `tmux_session == csm-bench-claude`;
  - the third session's row has `tmux_session == null`.
- **Runs on:** SANDBOX.

### INC-012: Codex resume goes through Claude (TUI fixed; `/api/resume` suspected)
- **Date:** 2026-09-26 (TUI); 2026-09-27 17:24 report (see INC-001).
- **Component:** server/daemon, cs TUI.
- **Symptom:** *"codex can't resume"*. Earlier, `m` on a stopped Codex session in the TUI ran
  `claude --resume <codex-id>`.
- **Root cause:**
  - TUI: fixed; it now uses `tmux_resume_command(sid, cwd, origin, …)`.
  - Server: **confirmed** by `resume-by-origin` (sandbox). `/api/resume` in Python (and the
    daemon before `fix/control`) has no origin branch. It checks `claude-<short>` /
    `claude-resume-<short>` and spawns `claude --resume <id>`, so a Codex id starts Claude in
    `claude-<short>`; Claude exits and leaves an idle shell there (e.g. the live
    `claude-01a08980`), which the next resume then returns silently. Unknown ids spawn too.
- **Fix:** TUI in `5cbff2b`. Daemon: `fix/control` — origin from the transcript (Claude jsonl,
  else Codex rollout; neither → 404, nothing spawned); `codex resume <id>` in `codex-<short>`
  in the rollout's cwd; a running session (live pane mapping incl. the Codex tty/app-server
  mapping, or a busy pane with the id's name) is returned without typing into it; a Codex CLI
  running outside tmux → 409; a pane with the id's name holding only an idle shell is
  respawned with the command (`respawned: true`) rather than returned. Python: open.
- **Detection:** top-10 check #2. Temp HOME with a Codex rollout plus a Claude transcript, and
  fake `claude`/`codex` binaries that record argv and cwd to a file.
  - `POST /api/resume {session_id: <codex>}` → 200 with `tmux_session`, and the recorded argv is
    `codex resume <id>`.
  - With an existing `codex-<short>` pane, it reuses that pane (`reused: true`) and spawns
    nothing.
  - `resume-existing-pane`: an idle-shell `claude-<short>` pane gets `claude --resume <id>`
    started in it; a busy one is returned untouched.
  - TUI Pilot: `m` on a Codex row builds `codex resume`.
- **Runs on:** SANDBOX, UI.

### INC-013: Terminal dot stuck red (or orange) after backgrounding or server restarts
- **Date:** 2026-09-27 16:29; earlier 2026-05-11.
- **Component:** Baton iOS.
- **Symptom:** *"when i lose focus on baton and come back - sometimes the session is red, not
  green, and only heals when i go to session list and back"*. May: *"my batos currect active
  session got to orange - why not keep it live?"*
- **Root cause:**
  - Sep: `PaneStream` started once on view appear and never retried after iOS killed the socket,
    a network change, or a server restart.
  - May: the global event-stream backoff maxed out at 64 s after 5 server restarts, and the
    failure counter only reset on success.
- **Fix:** `998805b`:
  - reconnect with backoff 0.5 → 10 s while wanted;
  - orange while connecting;
  - reconnect on foreground if the stream is down or quiet for 45 s;
  - ignore late callbacks from replaced connections.
- **Detection:**
  - UI (URLProtocol stub):
    - the stream errors → a new request within ≤ 1 s, and state goes through `connecting`,
      never stuck at `error`;
    - clean close → reconnect;
    - simulated background/foreground with 46 s of silence → immediate reconnect;
    - a late `didCompleteWithError` from the old task after replacement → state unchanged.
  - PHONE: lock 60 s, unlock → the `pane_reconnect` mark is followed by frames within 2 s.
- **Runs on:** UI, PHONE.

### INC-014: Input box stopped working
- **Date:** 2026-09-27 ~16:50–16:56.
- **Component:** Baton iOS.
- **Symptom:** *"input box in baton ios stopped working during these fixers"*. At 16:56 a
  "test ignore" sent from Baton arrived, so it had recovered.
- **Root cause:** **open.** Sends worked until 16:50:01. From 16:52 the phone logged 20
  `pane_stream` −1005 errors and 3 `events_stream` drops, and the server saw the same sockets
  close. Suspected: input state tied to a stream connection that was replaced (the stale-callback
  guard was added but not verified on the phone), or the network flapping. The phone was still
  on build `163132`.
- **Fix:** stale-callback guard (unverified).
- **Detection:**
  - UI: with the pane stream in `connecting` (stub drops it repeatedly), type and press Send. The
    stub receives `POST /api/send {session, text}` once, and the field clears and keeps focus.
    Also check that the keyboard appears on tap while the stream is erroring.
  - SANDBOX: `POST /api/send` to a bench pane → the text is visible in `capture-pane` within 1 s.
- **Runs on:** UI, SANDBOX.

### INC-015: Sending to a session didn't work (no tmux name for list rows)
- **Date:** 2026-04-27.
- **Component:** Baton iOS, server.
- **Symptom:** *"sending to session didn't work"*.
- **Root cause:** sessions from `/api/local-sessions` had no tmux name. The client fell back to
  `claude-resume-<id8>`, which only existed after an explicit resume, so the send silently went
  nowhere.
- **Fix:** `94e70ee` (live tmux mapping in the grouped JSON), `a25c154` (terminal-first routing).
  Daemon (`fix/control`): `send` / `send-key` to a missing pane → 404 (Python: 200 / 500).
- **Detection:**
  - Covered by check #1.
  - Plus ANY/SANDBOX: `POST /api/send` / `send-key` to a non-existent pane returns 404
    (not 200 and not 500), and the client shows an error.
- **Runs on:** SANDBOX, UI.

### INC-016: Chat live tail replayed the whole file (old messages appended out of order)
- **Date:** found 2026-09-27 (phone journey); deploy R6 10:38.
- **Component:** server, Baton iOS.
- **Symptom:**
  - Opening a chat pulled 17 MB and appended **3,155 old messages, back to April, below the
    newest 50**, out of order. The smaller session got 97 stray messages.
  - Partial lines were sent while being written.
  - Codex chats had no live updates: `/api/tail` returned 404, failing 5 of 15 bench live-tail
    steps.
- **Root cause:** `/api/tail` started at byte 0, had no `since`, sent incomplete lines, and had
  no Codex support.
- **Fix:** R6: start at EOF, `?since=` gapless replay, `?from=0` compatibility, complete lines
  only, Codex rollouts tailed as Claude-shaped records. Baton sends its newest message ts.
- **Detection:** top-10 check #4. Fixture transcript with 1,000 records:
  - `GET /api/tail/{id}` → 0 `data:` events in the first 2 s.
  - Append one record in 3 writes, 50 ms apart → exactly 1 event, valid JSON.
  - `?since=<ts of #998>` → exactly #999 and #1000, in order.
  - A Codex rollout id → 200, with events carrying `type` and `message.role`.
  - ANY-lite: on the newest real session, bytes received in the first 2 s < 10 KB.
- **Runs on:** SANDBOX, ANY.

### INC-017: Opening Codex sessions returned 404
- **Date:** 2026-09-26 (R1 deploy 15:31).
- **Component:** server.
- **Symptom:** tapping the most recent session (Codex `01a08980…`) failed: 69× 404 on
  `/api/session/01a08980…`, and 68 client `open_unsupported` marks. Sweep success was 98.8%
  for preview and 99.0% for messages.
- **Root cause:** `/api/session` and `/api/messages` only searched Claude project dirs.
- **Fix:** R1 Codex open (`_find_codex_rollout`, `_codex_messages`), then 100% on 399–515
  sessions.
- **Detection:** top-10 check #3. Both origins must be present in the fixture, and the sweep
  asserts `messages` is non-empty when `msg_count > 0`.
- **Runs on:** ANY, SANDBOX.

### INC-018: Terminal links not tappable; tapped file path "not found"
- **Date:** 2026-09-26/27 (21:58–22:12).
- **Component:** Baton iOS.
- **Symptom:** *"in baton i can't click on links you give me"*, *"still cant click after
  update"*, *"with double or triple tap i can select the line, but copy isnt availavle"*,
  *"…link ok but file isnt found"*, *"same error"*.
- **Root cause:**
  1. The tap location included `contentOffset` (16 `term_tap_oob` marks).
  2. Claude Code wraps long paths at the pane edge and indents the continuation, so only the
     first row was read (`…/AboutBlank/lattic` → 404).
  3. The join inserted a separator space.
  4. NUL cells.
- **Fix:** `LinkRouter.swift` (`logicalLine` join, offset fix, `TerminalLinkTapper`) in
  `8773383`. Terminal copy is still open (SwiftTerm needs first responder).
- **Detection:** UI (unit tests on `LinkRouter` with a fixture cell grid):
  - wrapped plus 2-space-indented path → the full path, with no inserted space;
  - NUL cells between words;
  - tap with a non-zero `contentOffset` → the correct row and column;
  - URL → `open_web_link`; path → asset request with the exact path.
  - Copy (open item): a long-press on the terminal exposes Copy.
- **Runs on:** UI.

### INC-019: Tapped files refused: asset roots too narrow (36% of paths)
- **Date:** 2026-09-27 (deploy A1–A3 11:01).
- **Component:** server.
- **Symptom:** the `~/.baton/pastes/…png` screenshot link failed. Of 1,747 existing files
  mentioned in transcripts, 627 were refused: `~/.claude` 281, `~/GitHub` 248,
  `~/Superapp-Projects` 41, other 57. The request log has 1,206 `/api/asset` 404s (mostly
  bench).
- **Root cause:** the allow-list covered only `~/Documents`, `~/Downloads`, `~/Desktop` and
  `/tmp`.
- **Fix:** roots now include `~/GitHub`, `~/Superapp-Projects`, `~/.baton/pastes` and
  `~/.claude/plans`, with a secret-name deny (`.env`, `.p8`, `.pem`, keys, `.git/config`,
  Claude credentials, Codex `auth.json`).
- **Detection:** top-10 check #8. In a temp HOME, one file per allowed root → 200. Each
  deny-list name, placed inside an allowed root → 403/404. `path=/Users/<u>/GitHub/../.ssh/id_x`
  → 4xx. Symlink from an allowed root to a denied file → 4xx.
- **Runs on:** SANDBOX.

### INC-020: 18 GB asset read into RAM → 500, then the list endpoint 500s
- **Date:** 2026-09-27, 10:59.
- **Component:** server.
- **Symptom:** `GET /api/asset?path=…/live-games/ws/frames-…jsonl` (18,004,698,699 bytes)
  → 500 after 4.9 s (`OSError(22, 'Invalid argument')`). Six seconds later the phone's
  `/api/local-sessions` → 500.
- **Root cause:** the whole file was read into memory. The failure left the process in a bad
  state for the next request.
- **Fix:** stream from disk; 413 over 100 MB (deploy 11:01).
- **Detection:** SANDBOX:
  - a sparse 5 GB file → 413 in < 100 ms, and server RSS grows < 20 MB;
  - a 50 MB file streams, with `Content-Length` correct;
  - immediately afterwards `/api/local-sessions` → 200.
- **Runs on:** SANDBOX.

### INC-021: Codex scroll from Baton did nothing
- **Date:** 2026-09-27, 11:53.
- **Component:** server.
- **Symptom:** *"codex scroll doesnt work from baton"*.
- **Root cause:** SGR wheel events were sent at column 1, row 1. Claude ignores the position;
  Codex drops wheel events outside its transcript area.
- **Fix:** send at the pane centre (deploy 11:54).
- **Detection:** SANDBOX: bench pane running a mouse-reporting app (`bench/tmux_bench_app.py`)
  that logs the SGR sequences it receives. `POST /api/scroll {direction: up, ticks: 3}` →
  exactly 3 events with button 64, at col = cols/2 and row = rows/2.
- **Runs on:** SANDBOX.

### INC-022: Pane redraws / flicker on open; same-size opens slow
- **Date:** 2026-09-27 11:42 (earlier 2026-04-27).
- **Component:** server, Baton iOS.
- **Symptom:** *"theres a few redraws in baton when the sizes didnt match"*. In April:
  *"still old messages though 0 width is better but is see some old resizes"*, and the banner
  stuck at "cancelled / 0 ev".
- **Root cause:**
  - Sep: the resize decision was based on per-process memory, not the pane's actual size, so
    every first open after a restart resized. The iOS-only path resized on every open; same-size
    opens took 0.2–6.6 s. Intermediate frames were streamed during the resize.
  - Apr: `updateUIView` called `onSizeChanged` on every chunk → reconnect → cancel loop.
- **Fix:** R7 (deploy 11:46): compare with the actual size, hold the first frame until settled,
  60 ms grace. April `4696522`: size-change guards.
- **Detection:** SANDBOX: open `/api/pane-stream/<bench>` with `X-Pane-Cols/Rows` equal to the
  pane's size → no `pane_resize` event with a resize action, and the first frame in < 150 ms.
  With a different size, the first data frame's `dims` equals the target. UI: the size
  coordinator fires once per real grid change.
- **Runs on:** SANDBOX, UI.

### INC-023: Stale Baton list (older sessions, not live)
- **Date:** 2026-05-11 and 2026-09-27 10:58.
- **Component:** server, Baton iOS.
- **Symptom:** May: *"i see older sessions on ios - not ones that are fresh in macos tui"*.
  Sep: *"does the list in baton show new messages as they come live"*. The list was 30–45 s
  behind, and the second line showed the user's own last message.
- **Root cause:**
  - May: see INC-026 (thread exhaustion → list timeouts → cached list shown).
  - Sep: 5 s polling vs 30 s server rebuild; no push; rows carried only user messages.
- **Fix:** L2 live list (`sessions_delta` over `/api/events`, `last_reply`/`last_role`;
  12:13) plus Baton `8773383` (patch in place, re-sort).
- **Detection:** SANDBOX: subscribe to `/api/events`, then append an assistant record to a
  fixture transcript. Within 6 s, a `sessions_delta` arrives whose `changed` includes that id
  with `last_role == "assistant"` and `groups` containing `recent`. A new transcript file →
  appears in `changed`. A deleted file → `removed`.
- **Runs on:** SANDBOX.

### INC-024: TUI click opened the session preview in place instead of switching terminal
- **Date:** 2026-09-27, 10:46.
- **Component:** cs TUI.
- **Symptom:** *"why did it open session in place rather that switching to terminal?"*
- **Root cause:** after the ListView → OptionList migration (T8), Textual's own
  option-selected handler ran after the custom click handler and opened the preview. The
  behavior test only checked that the switch happened, not that nothing else did.
- **Fix:** in `5cbff2b`; the behavior check now asserts both.
- **Detection:** UI (Textual Pilot on the fixture):
  - click a session row → `offer_resume`/switch called once, preview not open;
  - click "+N more" → the group expands (141 → 421 rows);
  - Enter on a row → the same as click.
- **Runs on:** UI.

### INC-025: TUI couldn't switch to Codex sessions (Codex CLI 0.157)
- **Date:** 2026-09-27, 10:48.
- **Component:** cs TUI.
- **Symptom:** *"codex switch isnt happening"* (`sessions_tui.log`: `FIND.nomatch sid=01a08980
  windows=74`).
- **Root cause:** Codex updated itself on 09-25 to 0.157. The shared Codex app-server now writes
  the rollout, not the CLI process, so "CLI holding the rollout open" (lsof) found nothing.
- **Fix:** ask the app-server for open top-level threads, pair each with the CLI running in the
  same cwd, and leave ambiguous pairs unmatched (in `5cbff2b`).
- **Detection:** SANDBOX (ps/lsof shims):
  - a fake app-server holding rollout `X` plus one fake `codex` CLI on `ttys0NN` with the same
    cwd → `_codex_tty_map` maps X to that tty;
  - two CLIs in the same cwd → no mapping;
  - an older CLI holding its own file → mapped the old way.
- **Runs on:** SANDBOX.

### INC-026: Persistent streams starved the server (SSE deadlock, then thread cap)
- **Date:** 2026-04-27; 2026-05-11.
- **Component:** server.
- **Symptom:**
  - Apr: the banner showed "live" but stuck at "1 ev"; *"search/sessions sometimes hung"*.
  - May: 8 concurrent phone streams against a 14-thread cap → `/api/local-sessions` timed out,
    and the app showed its cache (INC-023).
- **Root cause:**
  - Apr: a single-threaded `HTTPServer`; one SSE client wedged everything.
  - May: a bounded thread pool plus an expensive uncached list.
- **Fix:** `512b7d1` (`ThreadingHTTPServer`, "CRITICAL fix"), then a list TTL cache
  (2026-05-11), and later the cached rebuild.
- **Detection:** top-10 check #6 (20 streams, then health and list latency). Also: close the
  phone-side sockets abruptly (RST) → the server's thread count returns to baseline within 30 s.
- **Runs on:** ANY (+SANDBOX for the pane stream).

### INC-027: `/api/sessions` statuses wrong, no previews, "waiting" never shown
- **Date:** found 2026-09-26 (deploy 14:52).
- **Component:** server.
- **Symptom:** only 4 of 17 tmux sessions got a status, previews 0 of 17, and Baton never saw a
  `waiting` session.
- **Root cause:** an invalid `capture-pane -l 15` flag made the per-pane capture fail silently.
- **Fix:** batched `_capture_panes` (254 ms → 16 ms), correct flags.
- **Detection:** SANDBOX: bench panes rendering fixture screens (permission prompt, idle prompt,
  working spinner) → `/api/sessions` returns `waiting`/`idle`/`active` respectively, and every
  pane has a non-empty preview.
- **Runs on:** SANDBOX.

### INC-028: Duplicate and noise rows in the session list
- **Date:** 2026-06-05 (`e317fbb`); 2026-09-26 (R2, R3, R4).
- **Component:** server, cs TUI (shared loader).
- **Symptom:**
  - The same session appeared twice after a project move (`~/Documents/GitHub` → `~/GitHub`).
  - One Codex session was listed **8 times**: every subagent thread was filed under its parent,
    and each opened the same 130 MB file.
  - 1,735 Claude subagents and 136 headless `sdk-cli` runs cluttered the list (538 → 402 rows
    after the fix).
- **Root cause:** dedup by id was missing. Codex `session_meta.id` vs `session_id` was confused.
  Subagent and headless runs weren't filtered.
- **Fix:** R2 (thread ids), R3 (hide subagents and headless, `/api/children`, `/api/background`),
  R4 (keep the copy with the most messages; open the largest file).
- **Detection:**
  - ANY: ids unique within each group of `/api/local-sessions`; no row whose transcript has
    `entrypoint: sdk-cli`.
  - SANDBOX: the same id under two project dirs → one row, and `/api/messages` returns the
    larger copy. A Codex rollout with `source.subagent` → absent from the list and present in
    `/api/children/<parent>`.
- **Runs on:** ANY, SANDBOX.

---

## S3: slow enough to notice

### INC-029: Opening a big session took 15 s (quadratic tail reader)
- **Date:** 2026-09-26 (gen4 S2, deploy 22:41).
- **Component:** server.
- **Symptom:** a cold open of a large session took 15.3 s; the worst was 16.2 s. Across 769
  files the tail read summed to 192.5 s.
- **Root cause:** `_tail_jsonl_lines` repeatedly prepended to a bytes buffer.
- **Fix:** a chunk list (192.5 s → 2.9 s total; the open takes 1.4–2.2 s).
- **Detection:** SANDBOX: a 200 MB transcript → `/api/messages/{id}?limit=50` < 500 ms and
  `/api/session/{id}` < 1 s. The output equals a naive reference reader.
- **Runs on:** SANDBOX.

### INC-030: Scrolling slowed down after the Codex-scroll fix and the live list
- **Date:** 2026-09-27, 13:19.
- **Component:** server.
- **Symptom:** *"did scrolling slowed down?"* `/api/scroll` p50 went 32 → 64 ms and p90
  288 → 984 ms.
- **Root cause:** the Codex fix added a tmux call per scroll to find the pane centre, and
  live-list rebuilds competed for the GIL.
- **Fix:** R8: the pane size is cached for 5 s (cleared on resize); the watcher defers rebuilds
  until 3 s after scroll, keystroke or send. p50 14 ms, p90 28 ms (deploy 13:21).
- **Detection:** top-10 check #9, latency part. 200 scrolls on a bench pane while the harness
  forces a rebuild every 500 ms: p50 < 30 ms and p90 < 100 ms, and a shim counts exactly 1 tmux
  spawn per scroll.
- **Runs on:** SANDBOX.

### INC-031: Scroll returns 500 under load or for a vanished pane
- **Date:** 2026-09-26 22:45; 2026-09-27 09:34–09:35, 11:19–11:44, 13:18, 15:23–15:40 (36 phone
  500s).
- **Component:** server.
- **Symptom:** `/api/scroll` 500 after 3–41 s. The user sees a scroll that does nothing, then a
  stall.
- **Root cause:** partly open. `tmux send-keys -t <name>` failed or timed out (3 s timeout,
  exceeded under load and GIL contention) and was mapped to 500. A pane that vanished also gives
  500, not 404. There is no `=` exact-target prefix (see INC-050).
- **Fix:** R8 reduced the contention. Daemon (`fix/control`): exact `=name:` targets and a
  vanished pane → 404 on scroll / send / send-key / respond / pane. The 504-on-slow-tmux part
  is not done. Python: unchanged.
- **Detection:**
  - ANY/SANDBOX: scroll to a non-existent session → 404 in < 100 ms.
  - SANDBOX: a tmux shim that sleeps 5 s → a 504-class error within 3.5 s, not a 40 s hold.
  - LOG: no `/api/scroll` 5xx during the phone journey.
- **Runs on:** SANDBOX, LOG.

### INC-032: TUI launch 32 s, detect-running 1 s, 1 s freeze every 30 s
- **Date:** 2026-09-26/27 (gen6).
- **Component:** cs TUI.
- **Symptom:**
  - `cs-tui.startup.jsonl`: `total_ms` 32,481 (`content_match` 24,107).
  - `detect_running` 977–5,959 ms; refresh freeze 992 ms.
  - Under load, a key press took 8.5 s.
  - Arrow keys took 187 ms; search keystrokes 146 ms.
- **Root cause:**
  - Terminal-tab content matching blocked first render.
  - `ps -o tty` took 0.8 s.
  - The refresh rebuilt all rows on the UI thread.
  - The ListView cost grew with the row count.
- **Fix:** background content match; `tdev`-based tty; T5–T7 (order stub, corpus cache,
  background refresh); T8 OptionList (all in `5cbff2b`).
- **Detection:** UI (Pilot on the fixture, with the load guard):
  - first render < 5 s;
  - `detect_running` < 500 ms;
  - arrow-key → render p50 < 120 ms;
  - search keystroke p50 < 100 ms;
  - worst key latency during a forced refresh < 300 ms.
  - LOG: alert on `first_render.total_ms > 10000`.
- **Runs on:** UI, LOG.

### INC-033: TUI `detect_running` 20 s on two launches
- **Date:** 2026-09-27, 12:45:02 and 12:45:57.
- **Component:** cs TUI.
- **Symptom:** `cs-tui.startup.jsonl`: `detect_running` 20,306 ms and 20,294 ms (normally about
  270 ms). This was during the 12:36–12:50 brown-out (INC-008), right after the Codex
  app-server mapping landed.
- **Root cause:** **open.** Suspected: the Codex app-server `lsof` scan under load. The main
  session measured +0.16 s normally, because "the background server has a lot of open files".
- **Fix:** none.
- **Detection:** SANDBOX: shim `lsof` to take 3 s → `detect_running` must still return within
  1 s, with a timeout and Codex rows unmatched, not blocking. LOG: phase alert as in INC-032.
- **Runs on:** SANDBOX, LOG.

### INC-034: ccs froze 15 s every minute; crashed under Python 3.9
- **Date:** 2026-09-18.
- **Component:** cs TUI (`sessions_core` cache).
- **Symptom:** a 15 s TUI freeze every minute, then a SIGABRT (`BUG_IN_CLIENT_OF_LIBMALLOC…`)
  under Xcode's Python 3.9.
- **Root cause:** a shared sqlite connection with `check_same_thread=True`. 7 of 8 pool workers
  got `ProgrammingError`, which was silently treated as a cache miss (full re-parse). The first
  fix (`check_same_thread=False`) crashed on 3.9, where `sqlite3.threadsafety == 1`.
- **Fix:** `930985d`, then `79fedb6` (per-thread connection).
- **Detection:** SANDBOX: run `load_all_sessions` on the fixture under **both**
  `/usr/bin/python3` (3.9) and Homebrew 3.11 with the 8-worker pool. No crash, and the second
  run reports 0 parses (all cache hits).
- **Runs on:** SANDBOX.

### INC-035: List rebuild budget (codex re-read, full re-parse)
- **Date:** 2026-09-26/27.
- **Component:** server.
- **Symptom:** the rebuild took 1.15 s in-process (Codex 42% of it, re-reading an unchanged
  144 MB rollout), 1.6 s p50 live, and up to 29 s under load.
- **Root cause:** no per-file memo; a full re-parse on every change.
- **Fix:** S1 Codex cache (22:41), S1 incremental rebuild (11:56: warm 400 → 15 ms), L3/L4
  incremental parse.
- **Detection:** SANDBOX: a warm rebuild with one appended record < 100 ms, and the output
  digest equals a full re-parse. Also for new-file and deleted-file cases.
- **Runs on:** SANDBOX.

---

## S4: latent robustness bugs (500s, corruption, silent failure)

### INC-036: Parse-cache SQLite silently corrupted
- **Date:** found 2026-09-27 (R9 deploy 13:23).
- **Component:** server, cs TUI.
- **Symptom:** none visible; two sessions were quietly re-parsed on every cold start. Damaged
  file: `~/.claude/ccs_session_cache.db.corrupt-20260927-132303`.
- **Root cause:** two broken overflow chains; `sqlite3.Error` was swallowed.
- **Fix:** self-heal. On "malformed" / "not a database" the file is quarantined, threads
  reconnect, and an error record is logged.
- **Detection:** SANDBOX: overwrite page 2 of a copy of the cache with garbage, then start the
  server and the TUI. `.corrupt-<ts>` exists, `errors.jsonl` has one record, and the list equals
  the no-cache result.
- **Runs on:** SANDBOX.

### INC-037: One malformed record broke the whole list rebuild (incremental path)
- **Date:** 2026-09-27 (`a409f8d`, deploy 15:43).
- **Component:** server.
- **Symptom:** latent; found by the Rust port's parity work.
- **Root cause:** an exception in the incremental parser escaped into the rebuild.
- **Fix:** drop the state; the full parse returns `None` for that file.
- **Detection:** SANDBOX: append `{"type":"assistant","message":123}` and a truncated line to an
  active fixture transcript. `/api/local-sessions` stays 200 with all other rows, and the
  affected row equals the full-parse result.
- **Runs on:** SANDBOX.

### INC-038: Keep-alive POST framing: body leftovers parsed as the next request
- **Date:** 2026-09-26, 15:18 (during gen2 development).
- **Component:** server.
- **Symptom:** 2× `/api/scroll` 500 with `JSONDecodeError('Extra data: line 1 column 58')`.
- **Root cause:** POST keep-alive with the body not fully consumed or length-delimited.
- **Fix:** keep the connection only if the body was fully read; otherwise `Connection: close`.
- **Detection:** ANY:
  - two pipelined `POST /api/scroll` (to a bench pane) on one socket → both 200;
  - a body longer than `Content-Length` → 400 and the connection closed;
  - a body shorter than it, after a 2 s wait → 400 or timeout, never 500;
  - a fresh connection afterwards → 200.
- **Runs on:** ANY.

### INC-039: Voice 500 / "No Whisper backend available"
- **Date:** 2026-05-14 (and in the parity list).
- **Component:** server, infra.
- **Symptom:** *"can you make sure voice input from baton works - sends no whisper installed"*.
- **Root cause:** the launchd-spawned Python didn't source `~/.claude-sessions/config` (Groq
  key), and the orphan `whisper` had a Python 3.9 shebang. With no backend, `/api/voice` → 500.
- **Fix:** `csm_config._maybe_load_env_file` (`CSM_ENV_FILE` list, strips `export `), plist env.
  Daemon (`fix/control`): no backend → 503 `{"error":"no transcription backend"}` (also when
  `whisper` exists but can't run, where Python returned the text "No Whisper backend
  available" as a 200 and typed it into the pane); an OpenAI error → 502, not typed. The
  sandbox pins `CS_MLX_PYTHON` to a missing path so mlx-whisper can't download a model.
- **Detection:** SANDBOX: a server started with a launchd-like env (`env -i` plus the plist env)
  and no backends. `POST /api/voice` (1 s silent WAV) → 503
  `{"error":"no transcription backend"}`, never 500. With a stub Groq endpoint → 200 plus the
  text.
- **Runs on:** SANDBOX.

### INC-040: `/api/browse` 500 on a missing dir, a file, or `~`
- **Date:** 2026-09-27, 16:14–17:13 (parity control runs).
- **Component:** server.
- **Symptom:** `errors.jsonl` has `FileNotFoundError` (`/nonexistent…`, and `~` not expanded)
  and `NotADirectoryError` (a file path) → 500.
- **Root cause:** there is no validation before `os.scandir`.
- **Fix:** open (Python). Daemon (`fix/read`, `api/sysread.rs`): a missing dir → 404, a file → 400,
  `~` / `~/x` expanded → 200; parity accepts the divergence (Python still 500s).
- **Detection:** ANY: `GET /api/browse?dir=/nonexistent` → 404, `?dir=<file>` → 400,
  `?dir=~` → 200 (home) or 400, never 500.
- **Runs on:** ANY.

### INC-041: Wrong JSON types → 500 on `/api/scroll` and `/api/send-key`
- **Date:** 2026-09-27, 16:09–17:09 (parity control).
- **Component:** server.
- **Symptom:** `AttributeError: 'int' object has no attribute 'strip'` (`/api/scroll` with
  `session: 1`) and `TypeError: 'int' object is not iterable` (`/api/send-key` with `keys: 1`).
- **Root cause:** `body.get(...).strip()` without type checks.
- **Fix:** open (Python). Daemon (`fix/control`): typed field access (`http::Fields`) on every
  POST endpoint → 400 naming the field.
- **Detection:** part of top-10 check #5, with type fuzz on every POST field → 400.
- **Runs on:** ANY.

### INC-042: Client disconnects logged as crashes; `err.log` without timestamps
- **Date:** 2026-04 → 2026-09-27.
- **Component:** server (observability).
- **Symptom:** `cs-server.err.log` is 19 MB: 497 `BrokenPipeError` and 328 `ConnectionResetError`
  full tracebacks, and no timestamps or request ids. The 16,865-line restart loop (INC-004)
  can't be dated. `/api/events` logged a 500 for `OSError 49` (a link-local client).
- **Root cause:** Python's default `handle_error` writes to stderr, and disconnects are not
  classified.
- **Fix:** structured `errors.jsonl` (12:50); disconnect noise is still in `err.log`.
- **Detection:** ANY: open `/api/events` and `/api/pane-stream/<bench>`, then RST the socket.
  Assert no new `errors.jsonl` record and no new `err.log` traceback; every `err.log` line from
  the server has an ISO timestamp.
- **Runs on:** ANY, LOG.

### INC-043 … INC-057: Python bugs found during the Rust port (parity reports)
All are latent. The daemon should do the right thing, and the Python behavior is recorded so
parity scripts allow the divergence.

| ID | Bug | Correct behavior / assertion | Runs on |
|---|---|---|---|
| INC-043 | `POST /api/kill-session` → 500 when no tmux server is running | 404 `{"error":"no such session"}` (or 200 `{ok:false}`); never 500. Test with `TMUX_TMPDIR` pointing to an empty dir. | SANDBOX |
| INC-044 | Voice 500 without whisper | see INC-039 | SANDBOX |
| INC-045 | Malformed request bodies (invalid JSON, non-object JSON, wrong `Content-Type`) → 500 | 400 with a JSON error, for every POST endpoint. **Daemon fixed** (`fix/control`: one parser, `http::read_object`; `/api/bench/*` validated before proxying) | ANY |
| INC-046 | A malformed tty map (`~/.claude/…tty map` with garbage or wrong types) hides orphans | `/api/orphans` ignores bad entries and still lists orphans; the list still builds | SANDBOX |
| INC-047 | Resume ids unvalidated, then used in a glob | `POST /api/resume {session_id: "*"}` / `"../x"` / `"[a-f]*"` → 400; no pane spawned (fake-binary argv log stays empty). **Daemon fixed** (`fix/control`: ids must be `[A-Za-z0-9_-]{1,128}`) | SANDBOX |
| INC-048 | Session ids used as glob wildcards in lookups | `/api/session/*`, `/api/messages/0*`, `/api/tail/?…` → 400/404, never another session's content. Daemon fixed (`fix/read`): ids must be a UUID or `agent-[A-Za-z0-9_-]+` before any lookup; ids with glob/path syntax (`* ? [ ] / \\`) → 400 `{"error":"invalid session id"}`, other malformed ids get Python's unknown-id answer (404, or 200 with no children) | ANY |
| INC-049 | `a;b` in tmux path params is passed to tmux (command separator) | `/api/pane/a;b`, `/api/pane-stream/a%3Bb`, scroll `session:"a;b"` → 400 (`_is_safe_tmux_name` on every tmux entry point). **Daemon fixed** (`fix/control`: `tmux_target::is_safe` on pane, pane-stream, scroll, send, send-key, respond, kill, voice `X-Session`; a key or message ending in `;` — tmux's separator, e.g. send-key `["x;", "kill-server"]` — is escaped, check `tmux-arg-separator`) | SANDBOX |
| INC-050 | tmux prefix matching: `-t name` without `=` targets `name-2` or `name…` when `name` is gone | bench panes `csm-bench-a` (killed) and `csm-bench-ab` (alive): scroll/send/kill/pane to `csm-bench-a` → 404, and `csm-bench-ab` receives nothing. **Daemon fixed** (`fix/control`: every target is `=name` / `=name:`; kill of a missing session stays 200 `already_gone`, INC-043) | SANDBOX |
| INC-051 | `/api/asset` crashes on latin-1 / non-UTF-8 filenames | file named `caf\xe9.txt` in an allowed root → 200 with correct bytes; `Content-Disposition` is RFC 5987-encoded | SANDBOX |
| INC-052 | An unpaired markdown/HTML tag in a transcript drops the following text in the preview | fixture message `a <b> c` / `<details>x` → the preview HTML contains `c` / `x` | SANDBOX |
| INC-053 | An invalid UTF-8 line in a transcript → `/api/messages` 500 | fixture transcript with a `\xff` line → 200, the other messages present. Daemon fixed (`fix/read`): the line is skipped | SANDBOX |
| NEW-2 | A record of the wrong shape (`{"type":"assistant","message":123}`, non-str text parts, non-object Codex `payload`) → `/api/messages` and `/api/session` 500; a bad subagent `.meta.json` → `/api/children` 500 | append `message:123` + a truncated line to a fixture transcript → messages 200 with the other messages, session 200 (check `malformed-record`). Daemon fixed (`fix/read`): such records are skipped everywhere in `baton_core::read` (messages, preview, Codex messages/tail, children) | SANDBOX |
| INC-054 | Codex activity signature only looks back 3 days, so older Codex sessions that become active don't trigger live deltas | touch a 10-day-old rollout → `sessions_delta` within 6 s | SANDBOX |
| INC-055 | `load_catalog` makes a blocking Airtable call inside the list rebuild | with the network blackholed (proxy to `127.0.0.1:9`), rebuild < 2 s and the list is 200; the catalog loads async or from cache | SANDBOX |
| INC-056 | Invalid UTF-8 in `ps` output disables the whole process walk (no running sessions detected) | ps shim emitting a `\xff` in one command line → other sessions are still detected as running | SANDBOX |
| INC-057 | Non-numeric `scores` (classification cache) → `/api/local-sessions` 500 | cache with `"importance": "high"` / `null` → 200, and the row sorts as 0 | SANDBOX |

INC-037 (incremental parse exception escaping) belongs to the same family and is already fixed.

---

## Other Baton iOS incidents (setup era, mostly S2 at the time)

### INC-058: "unauthorized" / "bearer token is 0" / "missing bearer token"
- **Date:** 2026-04-25.
- **Component:** Baton iOS.
- **Symptom:** *"shows unaothorized - check bearer token in sessions"*, *"bearer token is 0"*,
  *"missing bearer token"*.
- **Root cause:** the pasted token kept whitespace, and `HTTPTransport` captured the token at
  construction, so later edits were ignored.
- **Fix:** `1667a23` (trim, reveal eye), `f1681a3` (read the token live).
- **Detection:**
  - UI: SettingsManager trims `" tok\n"` → `tok`; changing the token without relaunch → the next
    request carries the new header.
  - ANY: a missing or wrong token → 401 JSON; `/api/health` without a token → 200.
- **Runs on:** UI, ANY.

### INC-059: "error - url nil" and "ATS policy requires secure connection"
- **Date:** 2026-04-25.
- **Component:** Baton iOS.
- **Symptom:** *"error - url nil"*, *"did both url and ip - same efect"*, *"error - resouce
  could not be loaded ats policy requirese use of secure conn"*.
- **Root cause:** strict `URL(string:)` building (trailing slash or space, port twice, no
  scheme), and ATS blocked plain HTTP.
- **Fix:** `d5921b8` (tolerant URL building), `77604c6` (ATS exception; to be replaced by
  `tailscale serve` HTTPS per PLAN).
- **Detection:** UI (unit): the inputs `100.x.y.z`, `http://host:19877/`, `host `,
  `host:19877` with port 19877, and `https://x.ts.net` all build a valid base URL. Plist check:
  the ATS setting matches the transport (HTTP allowed only while the server is plain HTTP).
- **Runs on:** UI.

### INC-060: LAN fallback broken by Mac hostname drift
- **Date:** 2026-09-27, 11:34–11:36.
- **Component:** infra, Baton iOS.
- **Symptom:** the computer name drifted to "MacBook Pro (10)" and the local name to
  `MacBook-Pro-109`. The Baton fallback and Tailscale names diverged, and the user asked
  *"should we rename our mac so automatic rename scheme is not failing our name"*.
- **Root cause:** a generic hostname plus Bonjour conflict renaming.
- **Fix:** renamed the Mac; the Baton default fallback was updated plus a migration.
- **Detection:**
  - DEPLOY/host check: `scutil --get LocalHostName` equals the host in Baton's default fallback
    and the Tailscale hostname.
  - Proposed: `/api/health` reports `local_hostname`, and the client stores it. A UI test then
    checks that the fallback follows a changed value.
- **Runs on:** DEPLOY, UI.

### INC-061: Session preview scrolled horizontally
- **Date:** 2026-04-25.
- **Component:** Baton iOS, server (HTML render).
- **Symptom:** *"session previes shows horiz scroll"*.
- **Root cause:** long code, paths and tokens didn't wrap.
- **Fix:** `3391d65` (word-wrap CSS plus server-side HTML).
- **Detection:** SANDBOX: `/api/session/{id}` HTML for a fixture with a 500-character token
  contains the wrap CSS. UI snapshot: the content width ≤ the viewport width.
- **Runs on:** SANDBOX, UI.

### INC-062: Baton saw only 1 tmux session while about 20 were active
- **Date:** 2026-04-27.
- **Component:** server.
- **Symptom:** *"it is also seeing just 1 tmux session, but i have about 20 active sessions on
  mac - something off"*.
- **Root cause:** at the time, most sessions ran in plain Terminal tabs, and `/api/sessions`
  only lists tmux. Later addressed by the tmux mapping, `cs up all` / ⇪ upgrade, and
  `/api/local-sessions` state.
- **Fix:** design change (tmux-first, `a03057c`, ⇪).
- **Detection:** SANDBOX: N bench tmux sessions plus M fake plain-tab processes → `/api/sessions`
  count == N. In `/api/local-sessions`, plain-tab running sessions are `status: active` with
  `tmux_session: null` (upgradeable), not missing.
- **Runs on:** SANDBOX.

---

## Other cs TUI incidents

### INC-063: malloc lines printed over the TUI on refresh
- **Date:** 2026-09-23.
- **Component:** cs TUI.
- **Symptom:** *"so when running sessions tui there's a malloc line happening on refresh on top
  3-10 lines repeating"* (a screenshot of `MallocStackLogging` warnings with a changing PID).
- **Root cause:** the TUI draws on stderr, and the short-lived `ps`/`tmux` children it spawns on
  each refresh inherited that stderr. macOS printed malloc warnings from them.
- **Fix:** the TUI draws through a private dup of the terminal, and children's stderr goes to
  `~/.claude-sessions/tui-stderr.log`.
- **Detection:** SANDBOX/UI: run `cs` under a pty with `MallocStackLogging=1` in the env and the
  refresh interval forced to 2 s, for 3 cycles. The captured screen contains no `malloc:`, and
  the log file does.
- **Runs on:** SANDBOX.

### INC-064: A tmux mouse-wheel binding broke scrolling
- **Date:** 2026-06-05.
- **Component:** cs TUI (`cs` tmux setup).
- **Symptom:** after `00fbd86`, scrolling stopped entirely at a shell prompt and in non-mouse
  apps.
- **Root cause:** an unconditional `send-keys -M` only forwards the wheel when the app has mouse
  mode on.
- **Fix:** `614d699` (revert), then `d4e03cb` (forward if mouse-aware, else copy-mode).
- **Detection:** SANDBOX: apply `cs new`'s tmux options to a bench session.
  - Wheel over a plain shell → `#{pane_in_mode}` = 1 (copy-mode).
  - Over a mouse-reporting app → the app logs the wheel and `pane_in_mode` = 0.
- **Runs on:** SANDBOX.

### INC-065: The theme picker had no effect
- **Date:** 2026-09-18 (`affcd4a`).
- **Component:** cs TUI.
- **Symptom:** picking light or dark in the command palette left the UI dark.
- **Root cause:** a class-level `theme = …` replaced Textual's reactive descriptor.
- **Fix:** set the theme in `__init__`.
- **Detection:** UI (Pilot): set `app.theme = "textual-light"` → the background CSS variable
  changes, and the value persists across restarts.
- **Runs on:** UI.

### INC-066: A TUI fallback can open a second writer on the same Codex thread
- **Date:** 2026-09-27, 10:48 (side effect of INC-025).
- **Component:** cs TUI.
- **Symptom:** a failed Codex switch fell through to opening a new Terminal window with
  `codex resume 01a08980` while the original CLI was still running.
- **Root cause:** **open.** There is no guard; the resume fallback runs when matching fails.
- **Fix:** proposed guard: never resume a session that has any live process referencing it.
- **Detection:** SANDBOX: a fake running `codex` process for thread X, with tty matching forced
  to fail. Clicking the X row must not call `run_in_terminal`, and it shows "running, can't
  locate terminal" instead.
- **Runs on:** SANDBOX, UI.

---

## search-hub

### INC-067: search-hub in demo mode after the GitHub folder move
- **Date:** 2026-05-15 (the paths moved again 2026-05-13).
- **Component:** search-hub.
- **Symptom:** *"search hub is in demo mode and stopped working"*.
- **Root cause:** stale `~/Documents/GitHub/...` paths in `BackendManager.swift` and elsewhere,
  so the backend never started and the app fell back to demo data.
- **Fix:** `e15aad4` (paths after the GitHub move; `build.sh`, hotkey, icon).
- **Detection:**
  - Static: `grep -r "Documents/GitHub"` across search-hub, `claude-session-manager` and baton
    → 0 hits.
  - Launch smoke: the app starts the backend and `GET localhost:19875/health` → 200 within
    10 s, and the app reports it is not in demo mode.
- **Runs on:** SANDBOX (static and launch).

### INC-068: search-hub couldn't load docs; permission prompts; stale results; keyboard nav
- **Date:** 2026-04-12.
- **Component:** search-hub.
- **Symptom:** *"why can't load doc"*, *"app says can't communicate with helper app"*,
  *"app asks permission everytime it previews file"*, *"i search zeely and still get the same
  results from prev search"*, *"up down don't cycle through"*, *"email filter just shows
  regular docs"*.
- **Root cause:**
  - iCloud-evicted (dataless) files and `Operation timed out` reads.
  - TCC / `com.apple.macl` for an unsigned app reading protected files.
  - The result list didn't reset on a new query.
  - The key monitor didn't handle arrows.
  - The category came from the wrong field.
- **Fix:** fixed across the April session (backend reads documents; the preview uses the
  backend; the key monitor; category from folder).
- **Detection:**
  - SANDBOX backend: `/document?path=<dataless or unreadable file>` → an explicit error within
    5 s, not a hang. `/search?q=A` then `/search?q=B` → different result sets.
    `/search?category=email` → every result's path is under the email folder.
  - UI (XCUITest): ↓/↑ moves the selection; Enter re-runs the search.
- **Runs on:** SANDBOX, UI.

---

## Not incidents (feature requests seen while mining)
Tappable key mentions (`ctrl+x ctrl+s`, `ctrl+b ctrl+b`) that send keys; themes that follow the
macOS/iOS appearance; Codex launch shortcut (`cs codex`); push notifications via a relay;
delta-sync last messages (shipped as L2). These aren't regressions and have no checks here.

## Evidence index (for test authors)
- Deploy notes with before/after numbers: `~/Library/Application Support/cs-server/deploys.jsonl`
  (R1–R9, S1, L2–L4b, gen1–gen5).
- Server error fingerprints: `~/Library/Logs/cs-server.errors.jsonl`. Current fps: `856a5e5e4a`
  (scroll int), `4251f848ed` (send-key int), `b35b18b3c4` / `082dbe8146` (browse), `c7289c1180`
  (events OSError 49).
- Phone errors: `cs-server.client.jsonl` `kind: error` (61× −1005, 32× −1004, 2× −1001,
  1× link_sheet 404); 85 `connect_failed` (09:3x–09:4x, 12:3x–12:4x, 17:2x–17:3x on 09-27);
  `open_unsupported` 68 (Codex `01a08980`); `term_tap_oob` 16; failed bench steps `messages` 20
  and `tail_first_event` 10.
- Slow-rebuild outliers: `cs-server.events.jsonl` `ls_rebuild` 09-27 09:42 (378 s), 11:10 (316 s),
  12:45 (198 s), 11:26 (136 s); 09-26 15:37 (111 s).
- TUI: `cs-tui.startup.jsonl` (32 s → 4 s; 20 s `detect_running` outliers at 12:45);
  `~/.claude/.logs/sessions_tui.log` `FIND.nomatch` (Codex `01a08980`, `5c5a3354` on 09-27).
- Start-up failure clues: `cs-server.err.log` tail (`unrecognized arguments: 19877 127.0.0.1`
  ×4; import-time `InterruptedError` in `build_folder_tree`).
