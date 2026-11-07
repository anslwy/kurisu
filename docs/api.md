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
  status: "new" | "starting" | "working" | "idle" | "error" | "stopped" | "awaiting_approval";
  lifecycle: "active" | "paused" | "archived";
  description: string;  // the "topic"
  note: string;         // the session's own status note (set_my_status)
  created_by_session: string | null; created_by_name: string | null;  // null = created by the user
  children: { id, name }[];
  replies: number;      // non-user messages, for unread counts
  last?: { seq, role, text (preview), at };
  last_activity: number | null; harness_session_id: string | null;
  created_at: number;               // unix seconds
  last_user_at: number | null;      // the user's last message here (not peers'), for sorting
  // From the CLI's status line (null until the session's first turn):
  context_tokens: number | null;    // tokens in the context now
  context_window: number | null;    // the model's window (e.g. 200000 or 1000000)
  context_used_pct: number | null;
  cost_usd: number | null;          // Claude Code's own API-equivalent estimate for the current CLI run
  metrics_at: number | null;        // when those were captured
  // While a turn runs: the tool it is running now (or ran last); null between turns.
  activity: { tool: string, title: string ("Bash: npm test"), state: "running" | "done", at } | null;
  // From the transcript (all turns, also across restarts):
  usage: { input, output, cache_read, cache_write, messages } | null;
}
Message  { seq: number (global, increasing); session: uuid; role: "user" | "agent" | "error" | "peer:<name>" | "approval" | "compaction"; text: string; at: unix seconds }
         // role "compaction": the summary the CLI kept after a compact
         // role "approval": text is "[approval:<id>] <title>", the place of a permission request in the channel
AuditRow { seq; actor: "user" | uuid | "rule#<n>" | "kurisu"; actor_name; target: uuid; target_name;
           field: "<field>" | "event:<created|started|resumed|stopped|failed|approval_requested|approval_approved|
                   approval_denied|approval_expired|approval_cancelled|rule_added|rule_removed|question_answered|steered|
                   compacted|compact_failed>"; old; new; at }
Approval { id: string (8 hex; any unique prefix of 4+ works); session: uuid; session_name; request_id;
           tool: string; title: string ("Bash: ls -la"); input: object (the tool input); cwd: string;
           status: "pending" | "approved" | "denied" | "expired" | "cancelled";
           decided_by: "user" | "rule#<n>" | "kurisu" | null; reason: string | null; rule_id: number | null;
           created_at; expires_at; decided_at: number | null;
           kind: "permission" | "question";
           preview: string;       // short display-safe summary: the command, path or URL, or "Which color? [Red / Blue]"
           questions: [{question, header?, options: [{label, description?}], multiSelect}] | null;  // kind "question"
           answer: {<question>: string} | null }                                                  // once answered
Rule     { id: number; scope: "role" | "session"; target: string (role name or session uuid); target_name;
           tool: string ("Bash", "mcp__github__*", "*"); in_cwd: boolean; command_prefix: string | null;
           decision: "allow" | "deny"; created_by; created_at }
```

## Endpoints

| Method and path | Body | Result |
| --- | --- | --- |
| `GET /status` | | `{api, version, clis: string[], server: {pid, rss_kb, home}, home}` |
| `GET /sessions?all=true` | | `Session[]`. `all=false` hides archived sessions; the default is `true`. |
| `POST /sessions` | `{name, description?, model?, cwd?, harness?}` | the created worker `Session`, plus `warning?` for an unverified model. `cwd` must be absolute (default: Kurisu's workspace for that name); `harness` is `claude` (default) or `codex`. |
| `GET /sessions/{id}` | | `Session` |
| `GET /sessions/{id}/messages?after=<seq>` | | `Message[]` with `seq > after` (up to 500) |
| `GET /sessions/{id}/messages?tail=<n>` | | the last `n` messages (1–200), oldest first |
| `GET /messages?after=<seq>&limit=<n>` | | `{messages: (Message & {session_name})[], msg_seq}`: new messages in **all** sessions (limit 1–1000, default 200). One cursor for a whole-team poll; `msg_seq` is the newest seq. |
| `POST /sessions/{id}/messages` | `{text, wait?, mode?, images?, files?}` | `wait` true (default): `{id, name, reply, stop_reason, notice}` when the turn ends. `wait: false`: `{id, name, seq, queued, steered}` at once; the turn runs in the background (after the running one, if any; `queued: true`), and its reply arrives as a `message` event. `mode: "steer"`: if a turn is running, the text is typed into it (Claude folds it into that turn, or runs it right after; the reply comes with that turn's reply; `steered: true`); if no turn is running it behaves like `wait: false`. `notice`: see below. |
| `POST /sessions/{id}/files` | `{name, data_b64}` | `{path, size}`: the file is saved in the session's folder under `.kurisu-uploads/<unix time>-<safe name>` (mode 0600), so the session can read it without a permission prompt. `data_b64` may be a `data:<mime>;base64,` URL. At most 25 MB decoded per file; request bodies up to about 35 MB. |
| `POST /sessions/{id}/compact` | | Runs the CLI's own `/compact` and waits (up to 10 min): `{id, name, pre_tokens, post_tokens_estimate, summary, trigger}`. `pre_tokens` comes from the transcript's compact boundary; `post_tokens_estimate` is the summary length / 4 (the next turn reports the real context size). Refused while a turn is running, before the session's first turn, and for Codex sessions. Also recorded as a `compaction` message and an `event:compacted` audit row. |
| `GET /sessions/{id}/mcp` | | `{builtin: ["kurisu-mcp"], servers: [{name, enabled, config}], note}`; `env` and `headers` values show as `"<set>"` |
| `POST /sessions/{id}/mcp` | `{name, config?, enabled?}` | add a server (needs `config`), change its config, or turn it on or off: `{name, server, changed, note}`. `config` is an `mcpServers` entry: `{command, args?, env?}` (stdio) or `{type: "http" \| "sse", url, headers?}`. |
| `POST /sessions/{id}/mcp/{server}/delete` | | remove it |
| `GET /sessions/{id}/events` | | `AuditRow[]` for that session (lifecycle events and field changes) |
| `POST /sessions/{id}/update` | any of `{description, model, effort, name, lifecycle, role, task}` | `{name, changes: [{field, new}], unchanged: [field], warning?}`. Unchanged values are not written or audited. Model and effort apply from the next turn. |
| `POST /sessions/{id}/stop` | | `{name, was_running, turn_cancelled}`. Closes the CLI; a running turn ends at once (its blocking send fails with "the turn was stopped", a `turn stopped: …` error message is recorded, and pending approvals are cancelled). Status becomes `stopped`; the next message resumes the CLI and is told not to answer the stopped one. |
| `POST /lead` | `{text, model?}` | Starts or reuses the Lead and sends it the task; same result as a blocking send |
| `GET /lead?create=false` | | the Lead's `Session` (the active session named `lead`, else the first active lead). 400 if there is none, unless `create=true`. This is the default target for a chat channel (e.g. Telegram): send to it with `POST /sessions/{id}/messages`. |
| `GET /models?harness=claude` | | `{harness, models: [{id, label, verified, default}], note}`. Other well-formed `claude-*` ids are accepted with a warning; Codex has no catalogue yet. |
| `GET /audit` | | all `AuditRow[]` |
| `GET /usage` | | `{rate_limits, sessions: {<name>: {messages, input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens, models, context_tokens, context_window, context_used_pct, cost_usd, metrics_at}}, total, note}`. `rate_limits` is `{five_hour: {used_percentage, resets_at}, seven_day: {…}, captured_at, source}` or `null` (see below). |
| `GET /approvals?all=false&session=<id>` | | pending `Approval[]`, oldest first; `all=true` returns the last 100 of any status, newest first |
| `POST /approvals/{id}/approve` | `{always?: bool}` | `{id, status, session, session_name, title, rule}`. Single use. `always` also adds a session rule: that tool is allowed inside the session's folder. |
| `POST /approvals/{id}/deny` | `{reason?}` | as above; the session sees "The user denied this tool call: <reason>". For a question this dismisses it. |
| `POST /approvals/{id}/answer` | `{answers: {<key>: string \| string[]}}` | answers a `kind: "question"` approval. `<key>` is the question text, its `header`, or its index (`"0"`); the value is an option label or free text, or a list of labels for `multiSelect`. Returns the approval summary plus `answer`. Every question needs an answer. |
| `GET /rules` | | `Rule[]` |
| `POST /rules` | `{scope, target, tool, in_cwd?, command_prefix?, decision?}` | the new `Rule`. `target` is a role name, or a session id or name for `scope: "session"`. An allow rule for `*` must set `in_cwd` or `command_prefix`. |
| `POST /rules/{id}/delete` | | `{removed: id}` |
| `GET /stream` | | **Server-sent events** (see below) |

## Live events: `GET /stream`

A `text/event-stream` response. Each event has a name and a JSON `data`
payload, and a keep-alive comment is sent every 20 s.

| Event | Data | When |
| --- | --- | --- |
| `hello` | `{sessions: Session[], msg_seq, audit_seq, approvals: Approval[]}` | first event on connect (a snapshot) |
| `sessions` | `Session[]` | the session list changed: status, new session, rename, archive, last message, tool activity, context, … |
| `message` | `Message` | a message was recorded in any session (user, agent, peer or error) |
| `audit` | `AuditRow` | a lifecycle event or field change |
| `approvals` | `Approval[]` (pending) | the pending list changed (a request arrived, was answered, expired or was cancelled) |
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

## Sending, queueing and steering

* Every send records the message first (`seq` in the reply, and a `message`
  event), then runs one turn per message, in order.
* Only one turn runs per session. A blocking send while a turn is running
  waits for it; `wait: false` returns at once with `queued: true`.
* `mode: "steer"` types the message into the running turn instead of waiting
  (Telegram's `/send_now`). It is recorded like any message, and an audit row
  `event:steered` names its `seq`.
* If the previous turn failed (for example a rate limit), the failed message
  is still in the CLI's context. The next turn tells the session not to answer
  it, and the blocking send returns `notice` saying so.

### Attachments

A send may carry `images: [{mime?, name?, data_b64}]` and
`files: [{name, data_b64}]`. Each one is saved as with
`POST /sessions/{id}/files`, and the message text gets a list of the saved
paths ("[Attached (in your folder; read them with the Read tool): …]"). The
recorded message includes that list. Claude reads images by path.

## Usage, context and plan limits

Kurisu sets a status-line command in the `--settings` it passes to each
Claude CLI. Claude Code feeds that command a JSON object (model, context
window, cost, and for subscription logins `rate_limits`), and Kurisu keeps
the latest one per session. No credential is read: this is the same data
Claude Code shows in its own status line.

* `rate_limits` (the official 5-hour and 7-day plan limits) is account-wide,
  so `GET /usage` reports the newest capture from any session. It is `null`
  until a session has finished a turn, and with an API-key login, which has
  no plan limits.
* `context_*` and `cost_usd` on a session come from its latest capture.
  `cost_usd` is Claude Code's estimate for the current CLI process: it starts
  again when the CLI restarts or resumes.
* `usage` token counts come from the session's transcript and cover every
  turn.

## MCP servers per session

Every session gets `kurisu-mcp`. Other MCP servers are stored per session in
Kurisu and merged into the `--mcp-config` file Kurisu generates when the
session's CLI starts (still with `--strict-mcp-config`). Kurisu never reads
or edits `~/.claude.json`, `.mcp.json` or the Codex config. A change marks the
session for a restart (resume) before its next turn. Tools from these servers
are not pre-allowed: their first use is an approval (or add a rule, e.g.
`{"scope": "session", "target": "<id>", "tool": "mcp__github__*"}`). Only the
user changes MCP servers; sessions cannot.

## Approvals

When a session's Claude CLI wants a tool that is not pre-allowed (in its
`default` permission mode), the CLI's `PermissionRequest` hook hands the
request to Kurisu instead of showing a dialog in the terminal:

1. Rules are checked first. Deny rules win over allow rules. A rule applies to
   a role or to one session, names a tool (or a `prefix*`, or `*`), and can
   require `in_cwd` (every path inside the session's folder; for Bash: the
   shell's cwd, and no `..`, `~` or outside absolute path in the command) or
   a `command_prefix` (Bash only; no `;`, `&&`, `|`, redirects or `$(`).
   A match is settled at once and audited as `rule#<n>`.
2. Otherwise an `Approval` is created, a `role: "approval"` message marks its
   place in the channel, and the session's status becomes
   `awaiting_approval`. Answer it here, in `ku web` (a card in the channel), or
   with `ku approve <id>` / `ku deny <id>`.
3. Unanswered requests expire after `KURISU_APPROVAL_TTL` seconds (default
   600) and are denied. Requests still pending when the turn ends, the session
   is stopped, or the server restarts are cancelled.

**Questions.** Claude's `AskUserQuestion` tool arrives the same way, as an
approval with `kind: "question"` and `questions[]`. Rules never settle a
question. Answer it with `POST /approvals/{id}/answer` (`ku answer <id>
"<label>"`); the CLI receives the answers as the tool's result. Deny it to
dismiss the question. `approve` is refused for questions.

Every request and decision is in the audit trail. Only the user answers
approvals and edits rules; sessions cannot (their MCP calls are refused).
The standalone `claude-cli-acp` agent maps the same requests to ACP
`session/request_permission` for its client.

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
