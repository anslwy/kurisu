# Kurisu guide

The day-to-day reference behind the [README](../README.md): watching and
talking to sessions, the web UI, approvals and rules, MCP servers, the demo,
and clean-up. The HTTP API and every option are in [api.md](api.md).

## Watch and talk

```bash
ku sessions                     # name, short id, role, creator, model, effort, status, description
ku history lead                 # the Lead's conversation (user / agent / peer:<name>)
ku history worker-a             # or: ku history <uuid>
ku send worker-a "And 17 * 2? Just the number."
ku update worker-a --effort low --description "arithmetic specialist"
ku update worker-a --name math  # renames; the UUID never changes
ku audit                        # who changed what
ku version --json               # running server build; --version describes this CLI
ku usage                        # token usage from the CLIs' transcripts
```

To watch a session's live terminal:

```bash
ku attach worker-a              # detach with Ctrl-b d (don't type into it mid-turn)
```

## Use the web UI

```bash
ku web                          # one-time link to the running instance's /web chat
```

`ku web` asks the running server for a one-time `/web` link (the same flow as
`/web` in Telegram) and opens it in your browser. The link uses the direct
listener address rather than the public URL, prints the Tailscale address when
the instance is bound to one (so you can open it from a phone), and expires in
5 minutes. The first page asks for approval in Telegram. If no server is
running, `ku web` says so instead of starting one.

To run the older standalone web UI instead, opt in explicitly:

```bash
ku web --serve [--port 8787] [--host 127.0.0.1]
```

The standalone UI binds `127.0.0.1` by default and prints `open:
http://127.0.0.1:8787/#token=<random>`. Open that URL, including the `#token=…`
part. The layout is a team chat:
* **Sidebar:** each session is a channel (`# name`), nested under the session
  that created it (or grouped by role), with status dots, unread badges and an
  Archived section. `Ctrl/Cmd+K` jumps to a session by name.
* **Channel:** a live message list. You, the session and its peers (for
  example `lead`) each have their own avatar, and a peer message links back to
  that peer's channel. System events appear inline, errors are highlighted,
  and a "<name> is working…" line shows while a turn runs. Markdown and code
  are rendered.
* **Details panel:** topic, model, effort, created by and children; rename,
  pause, stop and archive; the audit log.
* **Approvals:** a session's permission prompt shows as a card in its channel
  (Allow once, Always allow in its folder, Deny), with a banner in the
  sidebar.
* **＋** gives the Lead a task or creates a session.
* On a phone the sidebar becomes a drawer. Light and dark mode.

The standalone UI (`ku web --serve`) binds `127.0.0.1` only. Every API call needs
the token, and requests with a foreign `Host` header are refused. Ctrl-C stops
the web UI; sessions keep running until `ku shutdown` (which also ends it).
Options: `--port 8787` (`0` picks a free port) and `--host` (a non-loopback host
prints a warning: anyone with the token controls your agents).

### For testers: a separate test data dir and a fixed token

```bash
export KURISU_HOME=/tmp/kurisu-qa                 # isolated store, sessions and workspaces
export KURISU_WEB_TOKEN=qa-test-token-0123456789  # optional fixed token (16+ characters)
ku web --serve --port 8787
# open http://127.0.0.1:8787/#token=qa-test-token-0123456789
# ... test ...
ku shutdown                                       # stop the sessions (same KURISU_HOME)
rm -rf /tmp/kurisu-qa                             # forget everything
```

Every `ku` command, including `ku web`, uses the `KURISU_HOME` of its own
shell, so export it in each terminal you use. Haiku is the default model, so
test turns are cheap; each live session is still a real `claude` process
(about 300–500 MB).

### Install it as an app (PWA)

In Chrome or Edge, use "Install Kurisu", or "Add to Home Screen" on a phone.
The service worker caches only the app shell, and data always comes live from
the API. Installing needs a secure context: `127.0.0.1` or `localhost` works,
and for a phone use HTTPS (for example `tailscale serve`). The "Connect
manually" link on the token screen takes a server URL and token, for a remote
Kurisu.

### HTTP API

The web UI uses only the versioned API at `/api/v1`: REST plus a live
server-sent-events stream with token auth. See [`docs/api.md`](api.md).

### Developing the web UI

The UI is a Svelte 5 + Vite app in `crates/kurisu/web`. Node is only needed to
change the UI. The built files in `crates/kurisu/web/dist` are committed and
embedded in the binary, so `cargo install` needs no Node.

```bash
cd crates/kurisu/web
npm ci
npm run dev        # http://localhost:5173, proxies /api to a running `ku web` (KURISU_DEV_API=http://127.0.0.1:8787)
npm run check      # svelte-check (types)
npm run build      # writes dist/; then rebuild the Rust binary and commit dist/
```

CI rebuilds the UI and fails if the committed `dist/` differs from a fresh
build.

## Approvals

Sessions run in Claude's `default` permission mode. When one wants a tool
that needs permission (Bash, file edits, …), the request comes to you instead
of a hidden terminal dialog:

```bash
ku approvals                    # pending requests: id, session, age, "Bash: touch x.txt"
ku approve 4a01                 # allow once (any unique id prefix)
ku approve 4a01 --always        # ...and allow that tool for this session inside its folder
ku deny 4a01 --reason "not now"
ku approvals --all              # recent decisions too (who decided: you, rule#N, or expiry)
```

Pre-approve what a role or a session may do without asking:

```bash
ku rule add --role worker --tool Bash --in-cwd             # Bash, but only inside the session's folder
ku rule add --session dev --tool Edit --in-cwd
ku rule add --role worker --tool Bash --command-prefix "git status"
ku rule add --role worker --tool WebFetch --deny            # deny rules win
ku rules
ku rule rm 3
```

Unanswered requests are denied after 10 minutes (`KURISU_APPROVAL_TTL`, in
seconds). Every request and decision is audited (`ku audit <name>`).
See [docs/api.md](api.md#approvals) for the rules' exact semantics.

### MCP servers

Sessions get only `kurisu-mcp` unless you add more. They are stored in Kurisu
(your `~/.claude.json` is never touched) and apply from the session's next
turn:

```bash
ku mcp add dev github --command "npx -y @modelcontextprotocol/server-github"
ku mcp add dev docs --url https://example.com/mcp
ku mcp dev                      # list (env values hidden)
ku mcp disable dev github       # or enable / rm
```

## Run the Milestone 1 demo

```bash
examples/m1-demo.sh             # uses `ku` from PATH (or KURISU=/path/to/ku)
```

The demo uses a fresh `KURISU_HOME` under `/tmp`. The Lead creates two workers,
finds them with `peers_list`, asks each one a question, changes one worker's
description and effort with `session_update`, and has the other worker try to
modify the Lead, which is denied. It then prints sessions, histories, the
audit log and token usage, and shuts down. Set `KEEP=1` to leave it running.

## Stop and clean up

```bash
ku shutdown                     # stops the server and every claude it started (transcripts are kept)
```

Kurisu's data (SQLite store, per-session hook files, workspaces) lives in:

* macOS: `~/Library/Application Support/kurisu`
* Linux: `${XDG_DATA_HOME:-~/.local/share}/kurisu`
* or wherever `KURISU_HOME` points.

Delete that folder to forget all sessions. Claude's own transcripts stay in
Claude Code's usual place (`~/.claude/projects/`).

## Gaps

* Approvals cover the Claude driver. Codex approval requests are declined
  (so Codex runs in its workspace-write sandbox without asking). Only the user
  answers approvals; a Lead cannot approve for its workers.
* A failed turn (for example a rate limit) stays in the CLI's context. Kurisu
  tells the session not to answer it with the next message, and returns a
  `notice` to the sender; it does not retry the failed message.
* `session_send` blocks until the peer replies, so the Lead talks to workers
  one at a time.
* Codex sessions have no `/compact` and no plan-limit figures; `ku usage`
  shows their tokens from Codex's own session files.
* `ku web` and Telegram get live events (messages, status, new sessions), but
  no partial output while a turn runs: a reply appears when the turn ends.
* After upgrading `ku`, run `ku shutdown` once so the background server
  restarts on the new binary.
* Data from before the UUID change (M1 dev builds) is not migrated: Kurisu
  refuses to open it, so delete that `KURISU_HOME`.
* Each live `claude` uses about 300–500 MB. Stop sessions you don't need
  (`ku stop <name>`) or run `ku shutdown`.

## Repository layout

| Path | What |
| --- | --- |
| `crates/kurisu` | The `ku`/`kurisu` CLI, server, HTTP API and web server |
| `crates/kurisu/web` | Web UI source (Svelte 5 + Vite); `dist/` is the embedded build |
| `crates/claude-cli-acp` | The Claude driver, plus a standalone ACP agent |
| `examples/m1-demo.sh` (in the repo root) | Milestone 1 demo |
| `docs/` | Design, terms check, plans |
| `scripts/lint-paths.py` | CI hard-coded path linter |
| `legacy/` | Frozen earlier work: guest-hosting pilot (Python/Go), Phase 0 Python skeleton |


Telegram `/version` and the built-in MCP `version` tool query the running server over its socket. They report package version, full/short git SHA, dirty flag, build profile/target and build time (UTC Unix seconds in JSON). JSON also includes server start time (UTC Unix seconds) and monotonic uptime seconds. `ku version [--json]` uses the same response and fails if no server is running; `ku --version` still describes the local executable. The `ping` response includes this server metadata under `build`.
