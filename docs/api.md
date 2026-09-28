# Baton daemon API

The contract every client (iPhone, Mac app, TUI) talks to. Written from the
Python reference server (`claude-session-manager/server/api_server.py`);
the Rust daemon must keep it byte-compatible slice by slice (see PLAN.md).

## Conventions

- **Auth:** `Authorization: Bearer <token>` on everything except
  `GET /api/health`. Wrong/missing → `401`.
- **Request ids:** clients send `X-Request-Id` (8 hex chars); the server logs
  it next to its own id, joining client and server timings.
- **Compression:** JSON bodies over ~1400 bytes are gzipped when the client
  sends `Accept-Encoding: gzip`.
- **Keep-alive:** HTTP/1.1; responses are length-delimited, so connections
  stay open (except streams).
- **Errors:** JSON `{"error": "...", "rid": "..."}`; a crashed handler
  answers `500` if nothing was sent yet.
- **POST bodies (daemon):** a JSON object, or empty (= `{}`). Invalid JSON,
  invalid UTF-8, a non-object, or a field of the wrong type → `400
  {"error": "..."}` naming the problem (Python answers `500` there,
  INC-041/045). `/api/bench/*` bodies are checked the same way before the
  daemon proxies them.
- **tmux session names (daemon):** every endpoint that takes one
  (`pane`, `pane-stream`, `scroll`, `send`, `send-key`, `respond`,
  `kill-session`, voice's `X-Session`) requires `^[A-Za-z0-9._-]{1,200}$`
  (else `400 invalid session name`) and targets it exactly (`=name`): a
  missing session is `404 {"error": "no such session"}`, never another
  session whose name starts the same (INC-049/050).

## Health

`GET /api/health` → `{status, hostname, version, release, src, files, pid,
started, python, …}`. `release` is a hash of the running source.
*To add:* `api_version`, `min_client`.

## Pairing

The Mac app shows a QR code holding
`{"v":1,"url":"https://<mac>.<tailnet>.ts.net","fallback":"http://<tailscale ip>:19877","lan":"http://<lan ip>:19877","token":"<bearer>"}`
(`fallback`/`lan` omitted when unknown; without Tailscale, `url` is the LAN
address). The tailnet name is known before remote access (`tailscale serve`)
is on, so the QR code is the same whichever order the user sets things up in.

`GET /api/pairing` (auth; Rust daemon only, `404` when the Mac app hasn't
written it yet) → `{v, url, fallback?, lan?, remote_access, updated}`: the
same addresses without the token, from
`~/Library/Application Support/Baton/pairing.json` (the app rewrites it when
Tailscale changes; `BATON_PAIRING_FILE` overrides the path).

**iOS contract (to implement in the iPhone app):** after pairing, and on
every launch/foreground, call `GET /api/pairing` over whichever address
works and replace the stored `url`/`fallback`/`lan` with the answer; keep the
token. Try `url` first (HTTPS, works away from home once remote access is
on), then `lan`, then `fallback`. Treat `404` or a Python server (no
`daemon` in `/api/health`) as "keep what you have".

## Session list

`GET /api/local-sessions[?project=…]` → `{recent: [Row], momentum: [Row],
areas: {"<Area>": [Row]}}`. ETag + `If-None-Match` → `304`.

`Row`:

| Field | Type | Notes |
|---|---|---|
| `id` | string | session UUID (Claude file stem / Codex thread id) |
| `title`, `folder`, `area` | string | title from the title cache; area from the folder |
| `msg_count`, `user_msg_count`, `assistant_msg_count` | int | |
| `last_ts`, `last_assistant_ts` | ISO-8601 string | |
| `age_str` | string | e.g. `3m`, `2h` (volatile) |
| `momentum`, `importance` | int, float | |
| `status` | `active` \| … | `active` when a tmux pane runs it |
| `state` | string | richer state taxonomy |
| `tmux_session` | string \| null | live tmux session name → open as terminal |
| `first_msg`, `last_msg` | string ≤1000 | user messages |
| `last_reply`, `last_role` | string ≤280 \| null, `user`\|`assistant`\|null | newest assistant line; who spoke last |
| `entity`, `entities` | object \| null, list | optional plugin |
| `parent_id` | string \| null | spawn lineage |
| `origin` | `claude` \| `codex` | |
| `subagent_count` | int | subagents listed via `/api/children/{id}` |
| `drift_topic` | string \| null | |

`GET /api/search-index` → `{index: {id: text}}` (ETag). Split out of the list.

`GET /api/background` → headless (`sdk-cli`) sessions hidden from the list.
`GET /api/children/{id}` → subagents of a session.

## Live updates

`GET /api/events` — SSE. Events are `data: <json>\n\n`; `: keepalive` every 15 s.

- `{"type": "sessions_delta", "etag": "…", "changed": [Row + {"groups": ["recent", "momentum", "area:<Name>"]}], "removed": [id]}`
  — pushed 2–4 s after a transcript changes. Clients replace each changed
  row in the groups it lists, add it where new, drop it elsewhere; Recent
  re-sorts by `last_ts`.
- `{"type": "sessions_update", "sessions": [...]}` — tmux session status, every 5 s.

## One session

- `GET /api/session/{id}?limit=N` → Markdown preview + metadata.
- `GET /api/messages/{id}?limit=N` → `{id, messages: [{role, ts, blocks: [{type, text|name|preview|…}], attachments: [{path, name, kind, size, preview_first_line}]}]}`.
  Client message id = `"<role>-<ts>"`.
- `GET /api/tail/{id}[?since=<ts>|?from=0]` — SSE of new transcript records
  (`data: <jsonl record>`), starting at end of file; `since` also replays
  records newer than that timestamp; Codex records arrive Claude-shaped.
- `GET /api/asset?path=<abs path>` → file bytes. Allowed roots + secret-name
  deny; `413` over 100 MB; gzip for text; ETag → `304` on reopen.

## Terminal (tmux)

- `GET /api/pane-stream/{name}` — SSE. Headers `X-Pane-Cols`/`X-Pane-Rows`
  size the pane (only if it differs; first frame held until settled). First
  event `event: dims\ndata: <cols>x<rows>`, then frames (full pane, ANSI).
- `POST /api/scroll {session, direction: up|down, ticks}` — SGR wheel at the pane centre.
- `POST /api/send {session, message}`, `POST /api/send-key {session, keys}`
  (`keys`: a string or a list; `key` is the single-key fallback). Missing
  pane → `404` (daemon; Python: `500` / silent `200`).
- `POST /api/spawn {…}`, `POST /api/kill-session {session}`.
- `POST /api/resume {session_id}` → `{tmux_session, claude_session_id,
  jsonl_bytes, estimated_ms}`. Daemon behaviour (INC-012/047):
  - `session_id` must be `[A-Za-z0-9_-]`, ≤ 128 chars → else `400`;
  - the origin comes from the transcript: `~/.claude/projects/*/<id>.jsonl`
    → `claude --resume <id>` in `claude-<short>`; else a Codex rollout under
    `~/.codex/sessions` → `codex resume <id>` in `codex-<short>` (rollout
    cwd). Neither → `404 {"error": "unknown session"}`, nothing spawned;
  - already running (the pane the live list maps to the id — process walk,
    Codex tty / app-server cwd mapping — or a pane with the id's name that
    has something running) → that pane, nothing typed (`reused` in the
    `resume` event; Codex responses also carry `"reused": true`);
  - a Codex session running on a terminal outside tmux → `409`;
  - a pane with the id's name holding only an idle shell (the agent exited)
    → the shell is replaced (`respawn-pane -k`) and the command started
    there; the response has `"respawned": true`;
  - Codex responses and events carry `"origin": "codex"`; Claude ones keep
    Python's shape.
- `GET /api/sessions` → tmux sessions with status (legacy list).

## Other

`POST /api/respond`, `POST /api/voice` (daemon: no transcription backend →
`503 {"error": "no transcription backend"}`, INC-039; Python `500` or a
`200` whose "transcript" is the error), `POST /api/paste-image`,
`POST /api/register-device`, `POST /api/hook-event`, `GET /api/browse`,
`GET /api/temp`, `GET /api/plugins`, `GET /api/entity-logo`,
`POST /api/client-metrics` (client timings, marks and error records).

Development only (to be gated off in release builds): `/api/bench/*`.
