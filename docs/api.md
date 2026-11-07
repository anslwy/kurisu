# Kurisu HTTP API v1

`ku web` serves this API and the embedded web UI. The web UI uses **only**
this API, so other clients (a mobile or Tauri app, a Telegram channel, scripts)
can do everything it does.

* Base URL: `http://127.0.0.1:<port>/api/v1` (port 8787 by default; `ku web --port N`).
* Content type: JSON (`content-type: application/json` on POST).
* Envelope: success is `{"ok": <result>}`. An error is `{"error": "<message>"}`
  with HTTP 400 (bad request or failure), 401 (token), 403 (Host header) or
  404 (unknown route).
* Session identifiers: anywhere `{id}` appears, a session's **UUID**, a unique
  **UUID prefix** (6+ hex characters) or its live **name** is accepted.

## Authentication

Every request needs the token that `ku web` prints (set `KURISU_WEB_TOKEN` to
fix it; 16+ characters). Send it as one of:

* `Authorization: Bearer <token>` (preferred);
* `x-kurisu-token: <token>`;
* `?token=<token>`, accepted **only** on `GET /stream`, for EventSource
  clients that cannot set headers.

There are no accounts. The token is the only credential, so treat it like a
password.

## Network and CORS

* By default the server binds `127.0.0.1` and rejects requests whose `Host`
  header is not that address (a DNS-rebinding guard).
* For a remote Kurisu (for example over Tailscale): run
  `ku web --host 0.0.0.0` (or the tailnet IP). This prints a warning, and the
  Host check is off. Prefer `tailscale serve` for HTTPS: the PWA service worker
  needs a secure context.
* Browser clients on another origin (a UI hosted elsewhere, a Tauri webview)
  need `KURISU_WEB_CORS_ORIGINS=https://app.example,tauri://localhost`.
  Only the listed origins get CORS headers.

## Data types

```ts
Session {
  id: string (uuid); name: string; role: "lead" | "worker"; harness: "claude" | "codex";
  cwd: string; model: string; effort: string | null;
  status: "new" | "starting" | "working" | "idle" | "error" | "stopped" | "awaiting_approval" (future);
  lifecycle: "active" | "paused" | "archived";
  description: string;  // the "topic"
  note: string;         // the session's own status note (set_my_status)
  created_by_session: string | null; created_by_name: string | null;  // null = created by the user
  children: { id, name }[];
  replies: number;      // non-user messages, for unread counts
  last?: { seq, role, text (preview), at };
  last_activity: number | null; harness_session_id: string | null;
}
Message  { seq: number (global, increasing); session: uuid; role: "user" | "agent" | "error" | "peer:<name>"; text: string; at: unix seconds }
AuditRow { seq; actor: "user" | uuid; actor_name; target: uuid; target_name; field: "<field>" | "event:<created|started|resumed|stopped|failed>"; old; new; at }
```

## Endpoints

| Method and path | Body | Result |
| --- | --- | --- |
| `GET /status` | | `{api, version, clis: string[], server: {pid, rss_kb, home}, home}` |
| `GET /sessions?all=true` | | `Session[]`. `all=false` hides archived sessions; the default is `true`. |
| `POST /sessions` | `{name, description?, model?}` | the created worker `Session`, plus `warning?` for an unverified model |
| `GET /sessions/{id}` | | `Session` |
| `GET /sessions/{id}/messages?after=<seq>` | | `Message[]` with `seq > after` |
| `POST /sessions/{id}/messages` | `{text}` | `{id, name, reply, stop_reason}`. **Blocks until the turn ends**; watch `/stream` for live progress. |
| `GET /sessions/{id}/events` | | `AuditRow[]` for that session (lifecycle events and field changes) |
| `POST /sessions/{id}/update` | any of `{description, model, effort, name, lifecycle, role, task}` | `{name, changes: [{field, new}], unchanged: [field], warning?}`. Unchanged values are not written or audited. Model and effort apply from the next turn. |
| `POST /sessions/{id}/stop` | | `{name, was_running}` (the CLI resumes on its next message) |
| `POST /lead` | `{text, model?}` | Starts or reuses the Lead and sends it the task; same result as a send |
| `GET /audit` | | all `AuditRow[]` |
| `GET /stream` | | **Server-sent events** (see below) |

## Live events: `GET /stream`

A `text/event-stream` response. Each event has a name and a JSON `data`
payload, and a keep-alive comment is sent every 20 s.

| Event | Data | When |
| --- | --- | --- |
| `hello` | `{sessions: Session[], msg_seq, audit_seq}` | first event on connect (a snapshot) |
| `sessions` | `Session[]` | the session list changed: status, new session, rename, archive, last message, … |
| `message` | `Message` | a message was recorded in any session (user, agent, peer or error) |
| `audit` | `AuditRow` | a lifecycle event or field change |
| `resync` | `{}` | the client fell behind and missed events: reload `/sessions` and the open channel |

Client pattern (this is what the web UI does): on connect, load
`/sessions`; when a channel opens, load `/sessions/{id}/messages` and
`/events`; then apply `message` and `audit` events for that channel and
replace the list on `sessions`. Reconnect with backoff, and resync after a
reconnect. Events come from a poller in `ku web` that checks the server about
every 0.7 s, so the latency is under a second.

### Notifications (designed, not built)

The `message` / `audit` / `sessions` events are the hook for notifications. A
notifier subscribes to `/stream` (or runs inside `ku web`) and maps events
such as an `agent` message in a session you aren't viewing, an `error`
message, or status `awaiting_approval` to Web Push or ntfy. No API change is
needed; a later version may add `POST /notify/subscriptions` for Web Push
endpoints.

## Versioning

Breaking changes go to `/api/v2`; `/api/v1` stays as documented. Adding fields
to results or new events is not breaking, so clients must ignore unknown
fields and events.

## Examples

```bash
T=<token>; B=http://127.0.0.1:8787/api/v1
curl -H "Authorization: Bearer $T" $B/sessions
curl -H "Authorization: Bearer $T" -H content-type:application/json -d '{"text":"hi"}' $B/sessions/worker-a/messages
curl -N "$B/stream?token=$T"
```
