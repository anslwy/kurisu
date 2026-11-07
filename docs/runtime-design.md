# Kurisu runtime design (Omnigent dropped)

Status: design plus a validated spike (`rust/`, branch `pivot/runtime`).
Date: 2026-11-06.

## Decisions

* **Kurisu has its own lean runtime, written in Rust.** It is a single static
  binary (`kurisud`) built with tokio and SQLite (rusqlite, bundled), with no
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

| Field | Meaning |
| --- | --- |
| `name` | Unique slug (`lead`, `dev-app`, `qa`). This is what the Lead and the user address. |
| `harness` | `claude`, `codex`, or later `acp` (any native ACP agent). |
| `cwd` | Absolute working folder. |
| `harness_session_id` | The harness's own id: a Claude session uuid or a Codex thread id. It is **bound once** and a rebind is refused, so restarts reuse it and never fork history silently. |
| `model`, `effort` | Set per task by the router. They change only between tasks, to keep the prompt cache. |
| `mcp` | MCP servers for this session. Every session gets `kurisu-sessions`; the Lead's copy has all the tools. |
| `status` | `stopped`, `idle`, `working` or `error` (plus `waiting_permission` in Phase 1). |

**Store (SQLite, WAL):** the `sessions` table above, plus `messages(seq,
session, role, text, at)`. `role` is `user`, `agent` or `peer:<name>`, so every
peer message records who sent it. Phase 1 adds `events` (the ACP update log for
channels), `approvals` and `schedules`. Implemented in `rust/kurisud/src/store.rs`.

## Driver interface (ACP-shaped)

```rust
trait Driver {                       // rust/kurisud/src/driver.rs
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
| request_permission | server requests `item/commandExecution/requestApproval`, `item/fileChange/requestApproval` and `item/permissions/requestApproval`. The spike declines them; Phase 1 maps them to ACP permission cards. |
| cancel | `turn/interrupt` (Phase 1 tracks the turn id) |
| credentials | Kurisu answers `account/chatgptAuthTokens/refresh` with an error. Tokens stay inside Codex's own managed login. |

One app-server child per kurisud process, **spawned lazily** on the first Codex
session, so it costs no memory while idle. The app-server is marked
`[experimental]` in `codex --help`; we pin a tested Codex version range.
`codex app-server generate-json-schema` gives exact types for CI drift checks.

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

## Peer tools for the Lead (MCP)

`kurisud mcp` is an MCP stdio server (`kurisu-sessions`) attached to sessions.
It offers `session_list`, `session_create`, `session_send` (blocks until the
peer's turn ends and returns its reply), `session_history` and
`session_status`. Calls are forwarded to the server over its 0600 unix socket,
and the sender is recorded as `peer:<KURISU_SESSION>`. Verified end-to-end:
Lead MCP call → server → Codex peer → reply → history. The MCP layer is
hand-rolled (initialize, tools/list, tools/call) to stay small; `rmcp` is the
upgrade path if more of MCP is needed. Phase 1 adds a non-blocking
`session_send` mode, with `session_wait` and a done event to the Lead, so the
Lead can run peers in parallel. It also adds per-session tool scoping (staff
may not create sessions).

## Scheduler, events and channels

* **Scheduler:** a `schedules` table (cron expression, target session, prompt,
  for example "standup" to `lead` daily) and one tokio timer loop. This
  replaces Omnigent's scheduled tasks.
* **Events:** each driver's `SessionNotification` stream is appended to
  `events` and fanned out on a broadcast channel, and the socket gains a
  `subscribe` op.
* **Channels:** the Telegram bridge port (`docs/bridge-reuse.md`) moves from
  Omnigent's REST and events to `kurisud`'s socket. The ordering, outage
  retry, cards and attachments logic carries over. Approval cards answer ACP
  `session/request_permission`.

## Measurements (spike, Steam Deck, 2026-11-06)

| Item | Value |
| --- | --- |
| `kurisud` static musl binary | 1.97 MB (opt-level s, LTO, stripped) |
| `claude-cli-acp` static binary | 1.26 MB |
| **kurisud idle RSS** | **2.0 MB** (`VmRSS` 2008–2092 kB, 1 thread) |
| kurisud RSS after serving a Codex turn | 2.0 MB |
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
  nothing to runtime memory, and is simple to maintain. The legacy tree also
  stays Python.
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
