# OpenCode through ACP

Kurisu starts the installed `opencode acp` and speaks ACP v1 JSON-RPC over
stdio. It does not call OpenCode's native server API. Install/configure OpenCode
separately; Kurisu never installs it, copies credentials or performs login.

```sh
ku models opencode
ku models opencode --model opencode/glm-5.3-flash-free
ku new researcher --harness opencode --model opencode/glm-5.3-flash-free
ku send researcher 'Read AGENTS.md and inspect this project'
ku update researcher --effort high
ku update researcher --mode plan
ku history researcher --tools
ku update researcher --harness codex
```

Omitting `--model` uses OpenCode's configured default, which may be paid. The
free model above was available in OpenCode 2.2.6 at implementation time; consult
installed discovery and current provider pricing before testing. Kurisu does
not automatically fall back to a different model after a provider error.

Model and effort pickers use ACP config options from the installed agent. Effort
values vary by model; unsupported choices fail explicitly. `default` resets
OpenCode's effort. `default` permission mode uses upstream Build permissions;
`plan` selects the upstream Plan agent. Claude's `auto`, `acceptEdits` and
`bypassPermissions` modes are unavailable through this ACP interface. OpenCode
can allow tools itself; Kurisu handles permission requests that OpenCode emits,
including allow once, allow always, deny, cancellation and expiry. Kurisu rules
apply to those requests, not to upstream tool calls that need no permission.
Question forms/terminal auth are not advertised: upstream cancels questions it
cannot present; authenticate with `opencode auth login` on the computer.

Kurisu supplies its own session MCP server and enabled stdio/HTTP MCP servers
in ACP new/load requests. SSE MCP is explicitly unsupported upstream. AGENTS.md
is read by OpenCode in the working directory. Kurisu adds current team
instructions as assistant context each turn. Each Kurisu session gets its own
ACP process because upstream MCP registrations are scoped to a directory.

Saved OpenCode session IDs remain in Kurisu's database. After restart, Kurisu
loads the same ID and working directory; load replay is not appended to Kurisu
history a second time. Harness switching archives the previous peer, preserves
its history/folder and transfers callbacks, rules, MCP and wake associations
through Kurisu's existing handoff flow. Permissions that have no OpenCode
counterpart reset to upstream default when switching to OpenCode.

Cancellation uses ACP `session/cancel`. If the prompt does not finish within
the interrupt grace period, Kurisu kills and reaps that session’s isolated ACP
process, ends the turn as stopped, and reloads the saved session on the next
message. `scripts/test-stop-stuck.py target/debug/ku` covers an agent that
ignores cancellation, process termination, tool cleanup, and the next message.
Stopping/archiving detaches with
`session/close`, preserving upstream history. A disconnected agent restarts and
loads the saved ID on the next turn. Requests are bounded; a timeout closes the
ACP transport before another turn can reuse that agent. The default turn bound
is 1800 seconds (`KURISU_OPENCODE_TIMEOUT_SECS` overrides it).

`/compact` is sent literally through ACP only if the installed agent advertises
that command. Success requires the upstream completed compaction marker and a
successful prompt response. No summary or token reduction is invented. Provider
quota is unavailable through ACP: `/usage` lists installed OpenCode explicitly
with that limitation. Session token/cost counters that Kurisu cannot recover
are left unavailable. Image/document uploads are saved in the workspace and
routed through the existing attachment message path; audio uses configured
local transcription because OpenCode ACP ignores audio blocks.

## Web messages during a turn

The Web composer keeps Queue and Stop available while a turn is running.
Steer now uses the same send mode as Telegram: Claude can receive the message
in its current turn; harnesses without steering support queue it instead.
Message labels show queued, delivered (its turn worker started), or steered.
These labels are persisted with history and updated over the event stream.
A turn ending in error or being interrupted does not discard queued follow-ups.
Interrupt cleanup finishes before the next queued turn starts.

Regression commands after building `ku`:

```sh
python3 scripts/test-web-queue.py target/debug/ku
npm test --prefix crates/kurisu/web
# Optional installed-Claude/provider smoke test:
python3 scripts/test-claude-steering.py target/debug/ku
```

## Telegram command support matrix

All tests use a mock Telegram HTTP server, fake ACP, and fresh `/tmp` homes.
They never contact Telegram or use the production instance/port.

| Command / route | OpenCode behavior |
| --- | --- |
| `/start`, `/help` | Shared help; no harness rejection |
| `/new`, `/reset` | Preserve selected harness/model/permission mode; create a fresh peer |
| `/model`, model/harness/effort callbacks | Installed OpenCode shown; ACP models and model-specific efforts; switch uses existing confirmation/handoff |
| `/list`, selection/page callbacks, `/use`, `/id` | Shared roster and selection with OpenCode harness/model labels |
| `/status` | OpenCode CLI/version, actual model/effort/mode; unavailable metrics stay unknown |
| `/usage` | Installed-provider filtering; OpenCode quota explicitly unavailable |
| `/compact`, picker/page/confirmation callbacks | Advertised ACP compact command; completion marker required; clear error if unavailable |
| `/mode`, mode callbacks | Only upstream `default` / `plan`; unsupported modes fail explicitly |
| `/mcp`, settings callbacks | Shared per-session MCP state; stdio/HTTP supported, SSE rejected explicitly |
| `/stop`, stop callbacks | Shared relay cancellation plus ACP cancellation; partial/tool updates remain in history |
| `/catch_up`, `/catch-up` | Shared durable history, including OpenCode tool rows |
| `/send-now`, `/send_now` | Shared message queue; ACP has no steer capability, so next turn queues |
| `/schedule`, `/wakes`, cancel callbacks | Shared durable wakes and cancellation |
| `/browser`, `/web`, `/terminal`, approval callbacks | Shared remote access links/settings; browser remains opt-in |
| Plain text, replies, edits, group/topic routes | Shared session selection and turn queue |
| Documents/photos/video | Shared upload to workspace, then ACP prompt with local path/context |
| Voice/audio | Shared local transcription, then ACP text; unavailable transcription reported |
| Tool permission callbacks, allow/always/deny/expiry | ACP permission option IDs mapped to advertised options |
| Question-form callbacks | Not advertised to OpenCode; upstream cancels unsupported question forms |
| Restart / pending message delivery / peer callbacks | Shared durable state and callback recovery; saved upstream session ID reused |

## Validation

```sh
cargo test --workspace --offline
python3 scripts/test-opencode.py target/debug/ku
python3 scripts/test-opencode-telegram.py target/debug/ku
python3 scripts/lint-paths.py
cd crates/kurisu/web && npm run check && npm run build
```

The protocol fixture covers create/load/history, config/effort, streamed text,
reasoning and tool updates, permission responses, cancel/stop, provider errors,
timeout/EOF recovery, restart and harness handoff. Real CLI verification uses
only a model confirmed free in both installed discovery and the official
catalogue, with isolated OpenCode/Kurisu data. Compaction provider selection is
upstream-controlled, so live compaction is not exercised unless all models it
can choose are independently confirmed free.

References: [official ACP documentation](https://opencode.ai/docs/cli/acp/),
[official model catalogue and pricing](https://models.opencode.ai/providers/opencode/).
