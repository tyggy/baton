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

## Health

`GET /api/health` → `{status, hostname, version, release, src, files, pid,
started, python, …}`. `release` is a hash of the running source.
*To add:* `api_version`, `min_client`.

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
- `POST /api/send {session, text}`, `POST /api/send-key {session, keys}`.
- `POST /api/resume {id}`, `POST /api/spawn {…}`, `POST /api/kill-session {name}`.
- `GET /api/sessions` → tmux sessions with status (legacy list).

## Other

`POST /api/respond`, `POST /api/voice`, `POST /api/paste-image`,
`POST /api/register-device`, `POST /api/hook-event`, `GET /api/browse`,
`GET /api/temp`, `GET /api/plugins`, `GET /api/entity-logo`,
`POST /api/client-metrics` (client timings, marks and error records).

Development only (to be gated off in release builds): `/api/bench/*`.
