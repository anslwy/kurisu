# Kurisu runtime design (Omnigent dropped)

Status: design plus a validated spike and **Milestone 1** (`crates/`, branch `main`, formerly `pivot/runtime`).
Date: 2026-11-06.

## Decisions

* **Kurisu has its own lean runtime, written in Rust.** It is a single static
  binary (`kurisu`, a CLI whose `kurisu serve` server starts on demand) built with tokio and SQLite (rusqlite, bundled), with no
  Python and no Node in the server. Omnigent is dropped.
* **Everything is a session.** The Lead is a session the user talks to. It
  messages other sessions as **peers** (create, send, read history, status).
  There is no parent/child sub-agent model.
* **Harnesses at first: Claude Code and Codex.** The user runs `claude` login
  and `codex login` themselves before starting Kurisu. Kurisu never reads,
  stores, refreshes or forwards their credentials.
* **ACP is the lingua franca.** Every harness sits behind one internal driver
  interface shaped like the [Agent Client Protocol][acp] and uses the official
  [`agent-client-protocol`][acp-rs] crate's v1 schema types (`SessionId`,
  `SessionUpdate`, `PromptResponse`, `StopReason`, and so on).

## Session model

> Update (post-M1): every session has a UUID `id` (primary key; used by
> messages, lineage, audit and the `KURISU_SESSION` identity) and a `name`
> (unique among non-archived sessions, renamable). All CLI commands, kurisu-mcp
> tools and web API routes accept either. `ku web` is a loopback web UI and,
> like the CLI, a client of the server socket.

| Field | Meaning |
| --- | --- |
| `name` | Unique slug (`lead`, `dev-app`, `qa`). This is what the Lead and the user address. |
| `harness` | `claude`, `codex`, `opencode` (installed CLI via ACP), or reserved `acp`. |
| `cwd` | Absolute working folder. |
| `harness_session_id` | The harness's own id: a Claude session uuid, Codex thread id or OpenCode ACP session id. It is **bound once** and a rebind is refused, so restarts reuse it and never fork history silently. |
| `model`, `effort` | Set per task by the router. They change only between tasks, to keep the prompt cache. |
| `mcp` | MCP servers for this session. Every session gets `kurisu-sessions`; the Lead's copy has all the tools. |
| `status` | `stopped`, `idle`, `working` or `error` (plus `waiting_permission` in Phase 1). |

**Store (SQLite, WAL):** the `sessions` table above, plus `messages(seq,
session, role, text, at)`. `role` is `user`, `agent` or `peer:<name>`, so every
peer message records who sent it. Phase 1 adds `events` (the ACP update log for
channels), `approvals` and `schedules`. Implemented in `crates/kurisu/src/store.rs`.

## Driver interface (ACP-shaped)

```rust
trait Driver {                       // crates/kurisu/src/driver.rs
    async fn new_session(cwd) -> SessionId;               // ACP session/new
    async fn load_session(id, cwd);                       // ACP session/load|resume
    async fn prompt(id, text, updates: Sender<SessionNotification>) -> PromptResponse; // session/prompt + session/update stream
    async fn cancel(id);                                  // ACP session/cancel
    async fn close(id);
}
```

Phase 1 completes the ACP mapping:
* `initialize`/capabilities per driver;
* `mcpServers` on new and load;
* `session/request_permission` for tool approvals and questions, surfaced to
  the user as approval cards;
* `Plan` updates.

### 1. Claude driver: `claude-cli-acp` (separate crate, also a standalone ACP agent)

This is the **real, unmodified, interactive `claude` CLI** in a private tmux
server. It does not use the Agent SDK, `-p` headless mode or the ACP SDK
adapter. It mirrors Omnigent's `claude_native` harness, which also runs
`claude` in tmux, delivers input with `tmux load-buffer` + `paste-buffer -p`,
and listens to `SessionStart`/`UserPromptSubmit`/`Stop`/`StopFailure`/
`PreCompact` hooks. Sources in the locally installed Omnigent package:
`harnesses/claude_native/bridge.py` (input through send-keys and bracketed
paste) and `harnesses/claude_native/lifecycle.py` (the hook event set).

| Step | Mechanism |
| --- | --- |
| start | `claude --session-id <uuid> --settings <hooks.json>` in `tmux -L kurisu-claude new-session -c <cwd>`. `--settings` only adds hooks; the user's own settings are untouched. |
| ready | The `SessionStart` hook, not a sleep (measured: 0.5–0.8 s). |
| send | Bracketed paste, then Enter, confirmed by the `UserPromptSubmit` hook with retries. The spike showed that right after a resume the TUI can swallow the first Enter. |
| stream / done | Hooks append their stdin JSON to `events.jsonl` (`<binary> hook <file>`). `PostToolUse` maps to ACP `ToolCall`; `Stop` maps to `AgentMessageChunk(last_assistant_message)` with `StopReason::EndTurn`; `StopFailure` is an error. |
| history | The `transcript_path` from the hooks (`~/.claude/projects/<slug>/<uuid>.jsonl`). |
| resume | `claude --resume <uuid>` with the same hooks; the conversation memory was verified. |
| cancel | `Escape` sent to the pane. |
| permissions (Phase 1) | `Notification` (permission prompt) and `PreToolUse` hooks map to ACP `session/request_permission`; the answer is keyed into the TUI, or a `PreToolUse` hook decision is returned. |
| compaction (Phase 1) | `/compact` keyed into the TUI; `PreCompact` and `SessionStart(source=compact)` report it. |

It ships as its own binary (`claude-cli-acp`) that speaks **ACP v1 on stdio**,
so it can be released independently and used by any ACP client (Zed and
others) on a subscription without the Agent SDK. Kurisu links the same crate
in-process.

Hook docs: [Claude Code hooks][cc-hooks]. Terms basis: the user signs in to the
unmodified binary themselves ([legal and compliance][cc-legal]); see
`docs/terms-check.md` section 1.

### 2. Codex driver: official `codex app-server`

`codex app-server` is "the interface Codex uses to power rich clients (for
example, the Codex VS Code extension)". It speaks JSON-RPC 2.0 over stdio as
newline-delimited JSON, and the docs say to use it "when you want a deep
integration inside your own product: authentication, conversation history,
approvals, and streamed agent events" ([app-server docs][codex-app-server]).

| ACP concept | app-server |
| --- | --- |
| initialize | `initialize` (`clientInfo`), then the `initialized` notification |
| session/new | `thread/start {cwd, sandbox, approvalPolicy}` → `thread.id` |
| session/load | `thread/resume {threadId, excludeTurns}`. Verified in a **new** app-server process. |
| session/prompt | `turn/start {threadId, input:[{type:text}]}` |
| session/update | `item/agentMessage/delta` → `AgentMessageChunk`; `item/started` (commandExecution, fileChange, mcpToolCall) → `ToolCall` |
| end of turn | `turn/completed` with `turn.status`: completed → `EndTurn`, interrupted → `Cancelled`, failed → error |
| request_permission | server requests `item/commandExecution/requestApproval` and `item/fileChange/requestApproval` become ACP permission cards (tool names `Bash` / `Edit`, so the user's rules apply), answered with `accept` / `acceptForSession` / `decline` / `cancel`. `item/permissions/requestApproval` grants have a different answer shape and are still declined. |
| cancel | `turn/interrupt` (Phase 1 tracks the turn id) |
| credentials | Kurisu answers `account/chatgptAuthTokens/refresh` with an error. Tokens stay inside Codex's own managed login. |

One app-server child per kurisu server process, **spawned lazily** on the first Codex
session, so it costs no memory while idle. The app-server is marked
`[experimental]` in `codex --help`; we pin a tested Codex version range.
`codex app-server generate-json-schema` gives exact types for CI drift checks.

**Sandbox and approvals.** A Claude session runs with `--permission-mode default`
and no OS sandbox: it can read and write the whole filesystem and use the
network, and every tool call the user has not pre-approved goes through
Kurisu's approval cards. A Codex session gets the same reach by default —
`thread/start` with `sandbox: "danger-full-access"` and
`approvalPolicy: "on-request"` — so a Codex worker can run `git ls-remote`,
`gh` or a download exactly like a Claude one, and approval-gated calls still
become cards. The trade-off is real: an unsandboxed Codex process is not
stopped by the OS, so Kurisu's rules (`ku rule add`) are the only gate; add
them for anything you would not let it run. Narrow it with
`KURISU_CODEX_SANDBOX=workspace-write` (writes only inside the session folder
and `/tmp`, reads anywhere) or `KURISU_CODEX_SANDBOX=read-only`. In
`workspace-write` Kurisu also passes `sandbox_workspace_write.network_access =
true`, because a DNS-dead sandbox breaks `git` and `gh` just as badly as a
missing file permission; `thread/start`'s `sandbox` field carries only the
mode, not a policy. Every mode keeps `approvalPolicy: "on-request"`, so the
approval path above is unchanged.

**Fallback:** the interactive Codex TUI in tmux, with `-c notify=[...]` for
end-of-turn and the reply (`agent-turn-complete` carries `thread-id` and
`last-assistant-message`), and `codex resume <id>`. Verified by hand on
2026-11-06, including passing folder trust as a `-c projects."<dir>".trust_level`
override so `~/.codex/config.toml` is never written. It's kept only as a
fallback because app-server gives structured events and approvals.

**Reference check: T3 Code.** It drives Codex through an app-server wrapper (a
workspace package named `effect-codex-app-server`, plus Codex provider files).
For Claude it depends on `@anthropic-ai/claude-agent-sdk`
([apps/server/package.json][t3-pkg]). We follow it for Codex only; for Claude
it is exactly the Agent SDK path our terms check rules out.

### 3. Generic native ACP driver (design; Phase 2)

This driver spawns any ACP agent over stdio (Gemini CLI, OpenCode, Goose, Qwen
Code, Kiro and others; see the [ACP agents list][acp-agents]) and forwards
requests and updates 1:1, using the crate's client side
(`Client.v1().connect_with(...)`). It is nearly free because the driver trait
is already ACP-shaped. Per-agent auth stays with each CLI. The Claude Agent
SDK adapter (`claude-agent-acp`) is **not** offered for subscription users
([it is built on the Agent SDK][claude-agent-acp]); it may be allowed behind a
flag for API-key users.

## kurisu-mcp: live roster and peer tools (injected into every session)

Kurisu injects its own MCP server, `kurisu-mcp` (`kurisu mcp`), into **every**
session it starts. For Claude this is a per-session `--mcp-config <file>`
plus `--strict-mcp-config` and `--allowedTools mcp__kurisu-mcp`; the user's
`~/.claude.json` is never edited. For Codex, Phase 1 uses the per-thread
`config.mcp_servers` override. Identity comes from `KURISU_SESSION`, set per
session at launch, and every call is tagged with it, so messages are
attributed to their sender (`peer:<id>`). This **replaces the hand-maintained
AGENTS.md roster**: a new or compacted session calls `whoami` and `peers_list`.

| Tool | Purpose |
| --- | --- |
| `whoami` | own id, role, description, model, effort, folder, creator |
| `peers_list` / `peer_get` | every session: id, role, description, harness, model, effort, cwd, status, own status note, last activity, `created_by_session`, children |
| `session_create` | name, description, role (worker), harness (`claude` default, `codex` or `opencode`), model (default `claude-haiku-5-5`; Codex/OpenCode: its default), cwd |
| `session_send` | (to, msg): blocks until the peer's turn ends and returns its reply |
| `session_history` | (id, limit, before, after, search, roles, tools, max_chars): a page of any session's stored history, any harness |
| `session_handoff` | (id, max_chars): summary + newest messages and tool calls, to continue another session's work |
| `session_status` | one session |
| `session_update` | (id, {task, model, effort, description, role, name, lifecycle}) or (id, harness, model?) to switch harness (archive + same-name replacement seeded via session_handoff): **ancestors only** for other sessions; a session may change its own task/status/description/model/effort (model/effort from its next turn) but not its own lifecycle/role/name; a harness switch may also be its own (scheduled after its turn) |
| `set_my_description` / `set_my_status` | each session keeps its own entry current |
| `board_post` / `board_read` | shared team board (standup notes, decisions, blockers) |
| `transcribe` | (path): an audio file to text with the user's optional speech-to-text (`ku stt setup`); "not set up" otherwise |
| `send_file` | (path, caption?): send a file from the session's own folder to the paired Telegram chat (jpg/png as a photo, anything else as a document; 10 MB / 50 MB caps; `..` and paths outside the folder are refused); "not set up" when Telegram is off |
| `wake_at` / `wake_list` / `wake_cancel` | schedule a message to yourself or a descendant (`at`, optional cron `repeat`); delivered as a normal turn, survives restarts (missed wakes fire once, late) |

### Lineage and authority

* `created_by_session` is NULL for sessions the user creates (the Lead, via
  the CLI or a channel) and the creator's id for sessions created through
  `session_create`. This is a creation tree. It is lineage only: every session
  can still message every other as a peer.
* **Any ancestor** (parent, grandparent and so on) may modify a descendant
  with `session_update`: task (shown as its status note), model, effort,
  description, role, name, and lifecycle (`active`, `paused`, `archived`).
  Non-ancestors get `permission denied: <caller> is not an ancestor of
  <target>`. Only the user can make a session a lead, and sessions can create
  only workers.
* **A session may update itself** for task (the status note), description,
  model and effort; model/effort take effect from its next turn. Its own
  lifecycle, role and name stay refused (e.g. it cannot archive itself).
* **Self follows the replacement chain.** A harness switch archives the old
  row and creates a live replacement with the same name. The old session's
  MCP env still carries the old id, while `id: "<name>"` resolves to the
  replacement. Both are the same "self": the update is accepted and lands on
  the live tip (the session that will run next), rather than failing with
  `permission denied: lead is not an ancestor of lead`.
* **The user** (CLI or channel) may modify anything: `kurisu update <id> ...`.
* Model, effort and name changes take effect from the session's **next turn**:
  the CLI is closed and resumed with the new `--model`/`--effort`/identity
  (verified in M1: worker-a relaunched with `--effort low`, history intact).
  Pausing blocks sends; archiving also hides the session from `peers_list`.
* Every change is written to an `audit` table (actor, target, field, old,
  new, time) and shown by `kurisu audit`.

## CLI (Milestone 1)

`kurisu status | lead "<task>" | new | send | sessions | history | update |
audit | usage | stop | shutdown | serve`. Any command starts the server in the
background on demand, and `kurisu status` works even when it is down.
`kurisu status` reports each CLI's path, version and login state from the
CLI's own status command (`claude auth status --json`, `codex login status`),
keeping only the login flag and method. Every session uses
`claude-haiku-5-5` by default; the model is a per-session setting. Sessions
launch with `--permission-mode default` so the user's own default (auto mode
and its setup dialog) is not inherited, and `--disallowedTools Task,Agent`
because there are no sub-agents.

## Scheduler, events and channels

* **Scheduler:** a `schedules` table (cron expression, target session, prompt,
  for example "standup" to `lead` daily) and one tokio timer loop. This
  replaces Omnigent's scheduled tasks.
* **Events:** each driver's `SessionNotification` stream is appended to
  `events` and fanned out on a broadcast channel, and the socket gains a
  `subscribe` op.
* **Channels:** the Telegram bridge port (`docs/bridge-reuse.md`) moves from
  Omnigent's REST and events to `kurisu serve`'s socket. The ordering, outage
  retry, cards and attachments logic carries over. Approval cards answer ACP
  `session/request_permission`.

## Milestone 1 results (2026-11-06, `examples/m1-demo.sh`, live)

The Lead created worker-a and worker-b, found them with `peers_list`, asked
each a question (42 and Canberra), changed worker-a's description and effort
through `session_update`, and had worker-b try to modify the Lead. That was
denied: "permission denied: worker-b is not an ancestor of lead". It then
posted to the board and reported. The CLI observed all of it through
`sessions`, `history`, `send` (a direct user→worker turn after the effort
change) and `audit`.

* `kurisu` static binary: 2.1 MB. **Server idle RSS: 2.7–3.3 MB.**
* Haiku usage for the whole demo (3 sessions): 14 assistant messages; 28
  input, 1,374 output, 51,424 cache-write and 461,412 cache-read tokens; all
  `claude-haiku-5-5`. The cost is dominated by cached system prompts and tool
  definitions.

## Measurements (spike, Steam Deck, 2026-11-06)

| Item | Value |
| --- | --- |
| `kurisu` static musl binary (spike) | 1.97 MB (opt-level s, LTO, stripped); M1: 2.1 MB |
| `claude-cli-acp` static binary | 1.26 MB |
| **server idle RSS (spike)** | **2.0 MB** (`VmRSS` 2008–2092 kB, 1 thread) |
| server RSS after serving a Codex turn (spike) | 2.0 MB |
| `codex app-server` child RSS | 329 MB, only while Codex sessions are in use |
| one interactive `claude` CLI RSS (sampled) | 300–475 MB per live session |

Memory is dominated by the harness CLIs, not the server. Phase 1 therefore
needs **idle eviction**: close a session's CLI after N idle minutes and resume
it on the next message (resume is proven for both harnesses). It also needs a
cap on concurrently live sessions (`budget.max_concurrent_staff`).

## What happens to Phase 0

* **Dropped:** the Omnigent runtime adapter (`kurisu/runtime.py`) and every
  Omnigent dependency in the plan.
* **Superseded by Rust in Phase 1:** the Python `kurisu/` package (config,
  roster, router, approvals, memory). Its schemas and tests are the
  specification; port them to serde structs with the same validation rules,
  then delete the Python package.
* **Kept in Python:** `scripts/lint-paths.py`. It is a CI-only tool, adds
  nothing to runtime memory, and is simple to maintain.
* Nothing in the runtime spike was written in Python first. The only discarded
  work is shell prototypes used to learn the CLIs' behaviour.

## Terms notes (additions to `docs/terms-check.md`)

* Claude: unchanged. Only the interactive CLI under the user's own login; no
  Agent SDK; no claude.ai login offered; no credentials touched.
* Codex: the docs recommend API keys for "programmatic Codex CLI workflows,
  such as CI/CD jobs". Saved ChatGPT login is reused by default, and an
  app-server "ChatGPT managed" mode exists where "Codex owns the ChatGPT OAuth
  flow" ([auth docs][codex-auth], [app-server docs][codex-app-server]). One
  passage, read only as a summary and not confirmed verbatim, says app-server
  auth "has never been permitted for commercial or hosted services". Kurisu is
  neither: it is self-hosted for the user's own use. OpenAI's terms forbid
  sharing account credentials and, in an archived version, "automatically or
  programmatically extract[ing] data or Output" ([terms of use][oai-tou]; the
  live page returned 403 to our fetcher). **Verdict: OK with conditions.** Own
  login only, own machine only, no sharing, and an API key recommended for
  always-on or heavy use (same as Claude). The current terms wording should be
  confirmed in a browser before the public release.

[acp]: https://agentclientprotocol.com/
[acp-rs]: https://crates.io/crates/agent-client-protocol
[acp-agents]: https://agentclientprotocol.com/get-started/agents.md
[claude-agent-acp]: https://github.com/agentclientprotocol/claude-agent-acp
[cc-hooks]: https://code.claude.com/docs/en/hooks
[cc-legal]: https://code.claude.com/docs/en/legal-and-compliance
[codex-app-server]: https://learn.chatgpt.com/docs/app-server
[codex-auth]: https://learn.chatgpt.com/docs/auth
[t3-pkg]: https://github.com/pingdotgg/t3code/blob/main/apps/server/package.json
[oai-tou]: https://openai.com/policies/terms-of-use/

### OpenCode driver

The installed `opencode acp` speaks ACP v1 over stdio. Each peer owns a subprocess to isolate directory-scoped MCP identities; lifecycle, approvals and streaming feed the existing server paths. See [OpenCode capabilities, validation and Telegram support matrix](opencode.md).
