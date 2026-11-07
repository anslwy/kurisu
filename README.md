# Kurisu

**A self-hosted AI team you run from the command line.** You talk to a **Lead**
session; it creates and messages **worker** sessions as peers. Each session is
the real, interactive CLI you already use (**Claude Code or Codex**, or both),
running under **your own login** on your own machine. Kurisu is a small Rust
binary (about 2 MB, about 3 MB of RAM when idle) that starts, resumes and
connects those sessions.

> Status: **alpha**. CLI `ku`, local web UI `ku web`, an optional built-in
> Telegram channel, Claude Code and Codex sessions (switchable per session),
> approvals for Claude tool calls, a scheduler, and `ku import omnigent`.
> Linux and macOS. See [Gaps](#gaps).

The CLI is installed as both **`ku`** and `kurisu` (the same binary). Every
session has a permanent **UUID** and a **name** (unique among live sessions).
Anywhere a command takes a session, you can give either.

## Quickstart (macOS and Linux)

### 1. Prerequisites

| | macOS | Linux |
| --- | --- | --- |
| tmux | `brew install tmux` | `sudo apt install tmux` (or your distro's package) |
| Rust (stable) | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` | same |
| C toolchain (bundled SQLite, TLS) | `xcode-select --install` | `sudo apt install build-essential` (Fedora: `sudo dnf install gcc`; Arch: `sudo pacman -S base-devel`) |
| **At least one** agent CLI, logged in | Claude Code (`claude`, log in once) and/or Codex (`codex`, `codex login`) | same |

You need **one** of Claude Code or Codex, not both. Check the login:

```bash
claude auth status --text     # Claude Code
codex login status            # Codex
```

New sessions use whichever CLI is installed and logged in (Claude Code if
both). Pick one explicitly with `--harness claude|codex`, or set a default with
`KURISU_DEFAULT_HARNESS` or `{"default_harness": "codex"}` in
`~/.kurisu/config.json`. If the chosen CLI is missing, Kurisu says so at once.

No C compiler and no sudo? `cargo` can use [zig](https://ziglang.org) as the C
compiler: install zig, then `export CC="zig cc"` before building.

Kurisu never reads, stores or forwards your credentials. It runs the
unmodified `claude` binary in a private tmux server (through Claude Code
hooks) and `codex app-server` for Codex.

### 2. Install

**Prebuilt binary (the default, once releases are published):**

```bash
curl -fsSL https://raw.githubusercontent.com/anslwy/kurisu/main/scripts/install.sh | sh
```

It picks the right build (Linux x86_64/arm64 static musl, macOS arm64/x86_64;
Windows later), checks its SHA-256 and installs `ku` into `~/.local/bin`. You
then only need tmux and a logged-in agent CLI; Rust and a C compiler are needed
only to build. The binary knows the commit it was built from (`ku --version`)
and can rebuild itself: see `ku source` below.

**Build from source** (needs the Rust and C toolchain rows above):

```bash
git clone https://github.com/anslwy/kurisu.git
cd kurisu
git checkout pivot/runtime
cargo install --path crates/kurisu        # installs `ku` and `kurisu` into ~/.cargo/bin
# or: cargo build --release  &&  export PATH="$PWD/target/release:$PATH"
```

### 3. Check your setup

```bash
ku status
```

This shows the `claude` and `codex` paths, versions and login state (from the
CLIs' own status commands), whether the Kurisu server is running, and the
sessions Kurisu knows about.

### 4. Give the Lead a task

```bash
ku lead "Create two workers with session_create, ask worker-a what 17+25 is and worker-b what the capital of Australia is, then summarise their answers."
```

The first command starts the Kurisu server in the background. The Lead is an
interactive `claude` session (or a Codex thread, on a Codex-only machine or with
`--harness codex`). It discovers peers with `peers_list`, creates
workers with `session_create` and messages them with `session_send`, all
through the `kurisu-mcp` server that Kurisu injects into every session. Every
Claude session uses **`claude-haiku-5-5`** by default, to keep testing cheap;
Codex sessions use your Codex default model (`~/.codex/config.toml`).

#### Codex only

No Claude Code? Install Codex and log in (`codex login`), then the same steps
work as is: `ku lead "…"` starts a Codex Lead, and its workers are Codex
sessions too. `ku new <name> --harness codex --model <id>` picks a model;
`ku update <name> --harness claude|codex` moves a session to the other CLI
later (it keeps its history). Codex tool calls are not routed through Kurisu
approvals yet (see [Gaps](#gaps)).

### 5. Watch and talk

```bash
ku sessions                     # name, short id, role, creator, model, effort, status, description
ku history lead                 # the Lead's conversation (user / agent / peer:<name>)
ku history worker-a             # or: ku history <uuid>
ku send worker-a "And 17 * 2? Just the number."
ku update worker-a --effort low --description "arithmetic specialist"
ku update worker-a --name math  # renames; the UUID never changes
ku audit                        # who changed what
ku usage                        # token usage from the CLIs' transcripts
```

To watch a session's live terminal:

```bash
ku attach worker-a              # detach with Ctrl-b d (don't type into it mid-turn)
```

### 6. Use the web UI

```bash
ku web                          # prints: open: http://127.0.0.1:8787/#token=<random>
```

Open the printed URL, including the `#token=…` part. The layout is a team chat:
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

The UI binds `127.0.0.1` only. Every API call needs the token, and requests
with a foreign `Host` header are refused. Ctrl-C stops the web UI; sessions
keep running until `ku shutdown` (which also ends `ku web`). Options: `--port 8787` (`0` picks a free
port) and `--host` (a non-loopback host prints a warning: anyone with the
token controls your agents).

#### For testers: a separate test data dir and a fixed token

```bash
export KURISU_HOME=/tmp/kurisu-qa                 # isolated store, sessions and workspaces
export KURISU_WEB_TOKEN=qa-test-token-0123456789  # optional fixed token (16+ characters)
ku web --port 8787
# open http://127.0.0.1:8787/#token=qa-test-token-0123456789
# ... test ...
ku shutdown                                       # stop the sessions (same KURISU_HOME)
rm -rf /tmp/kurisu-qa                             # forget everything
```

Every `ku` command, including `ku web`, uses the `KURISU_HOME` of its own
shell, so export it in each terminal you use. Haiku is the default model, so
test turns are cheap; each live session is still a real `claude` process
(about 300–500 MB).

#### Install it as an app (PWA)

In Chrome or Edge, use "Install Kurisu", or "Add to Home Screen" on a phone.
The service worker caches only the app shell, and data always comes live from
the API. Installing needs a secure context: `127.0.0.1` or `localhost` works,
and for a phone use HTTPS (for example `tailscale serve`). The "Connect
manually" link on the token screen takes a server URL and token, for a remote
Kurisu.

#### HTTP API

The web UI uses only the versioned API at `/api/v1`: REST plus a live
server-sent-events stream with token auth. See [`docs/api.md`](docs/api.md).

#### Developing the web UI

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

### 7. Approvals

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
See [docs/api.md](docs/api.md#approvals) for the rules' exact semantics.

#### MCP servers

Sessions get only `kurisu-mcp` unless you add more. They are stored in Kurisu
(your `~/.claude.json` is never touched) and apply from the session's next
turn:

```bash
ku mcp add dev github --command "npx -y @modelcontextprotocol/server-github"
ku mcp add dev docs --url https://example.com/mcp
ku mcp dev                      # list (env values hidden)
ku mcp disable dev github       # or enable / rm
```

### 8. Run the Milestone 1 demo

```bash
examples/m1-demo.sh             # uses `ku` from PATH (or KURISU=/path/to/ku)
```

The demo uses a fresh `KURISU_HOME` under `/tmp`. The Lead creates two workers,
finds them with `peers_list`, asks each one a question, changes one worker's
description and effort with `session_update`, and has the other worker try to
modify the Lead, which is denied. It then prints sessions, histories, the
audit log and token usage, and shuts down. Set `KEEP=1` to leave it running.

### 9. Stop and clean up

```bash
ku shutdown                     # stops the server and every claude it started (transcripts are kept)
```

Kurisu's data (SQLite store, per-session hook files, workspaces) lives in:

* macOS: `~/Library/Application Support/kurisu`
* Linux: `${XDG_DATA_HOME:-~/.local/share}/kurisu`
* or wherever `KURISU_HOME` points.

Delete that folder to forget all sessions. Claude's own transcripts stay in
Claude Code's usual place (`~/.claude/projects/`).

## How it works

* `ku` / `kurisu` is one binary. `ku serve` (started on demand) owns a SQLite
  store and a unix socket. Every other subcommand, and `ku web`, is a client.
* Sessions have a UUID `id` (permanent; used for lineage, history and audit)
  and a `name` (unique among non-archived sessions; renamable, reusable after
  archiving).
* **Claude driver** (`crates/claude-cli-acp`): it runs `claude --session-id
  <uuid>` in a private tmux server. `SessionStart`, `UserPromptSubmit` and
  `Stop` hooks signal ready, submitted and done. Resume uses `claude --resume
  <uuid>`. The crate is also a standalone **ACP agent** binary
  (`claude-cli-acp`), so ACP clients can drive your own Claude Code
  subscription without the Agent SDK.
* **kurisu-mcp** is injected per session (`--mcp-config`, never your
  `~/.claude.json`; Codex sessions get it as a thread config override). It
  provides `whoami`, `peers_list`, `session_create`, `session_send`,
  `session_history`, `session_handoff`, `session_update`, `set_my_status`,
  scheduled wakes (`wake_at`, `wake_list`, `wake_cancel`: "wake me in 30m", or
  a daily 06:00 standup via cron; also `ku wake`), the shared board, and
  `transcribe` when speech-to-text is available. Sessions are peers.
  `created_by_session` records lineage, and only ancestors (or you) can modify
  a session.
* Kurisu stores every session's messages, replies, tool calls and compaction
  summaries in its own database, so any session can "look at session X and
  continue from there" whatever harness either uses (see `docs/api.md`,
  Session history), including across harness switches.
* **Telegram (optional, built in):** `ku telegram setup` connects a bot to the
  Lead from inside the ku server: commands, cards, queue, photos, files, voice
  and broadcasts, ported from the Python Telegram bridge. Setup walks you
  through it: create a bot with @BotFather (`/newbot`), paste its token, then
  send `/start` to your bot so Kurisu can find your chat id (only that chat
  may use the bot). `ku telegram off|on|status`.
* **Remote terminal (optional):** `ku terminal enable --listen <tailscale-or-LAN-ip>:8790`,
  then `/terminal` in Telegram gives a one-time link to a shell in your phone's
  browser. Opening it shows a pairing code; you approve it on a Telegram card
  (codes must match) before anything runs. See `docs/api.md`.
* **Rebuild it yourself:** `ku --version` shows the exact commit; `ku source
  get|check|build|install` rebuilds Kurisu from it (so you can ask the Lead to
  add a feature to Kurisu), keeping the previous binary and falling back to it;
  `ku source reset` goes back to the shipped one.
* **Speech-to-text (optional):** voice notes are transcribed when a working
  `stt` command is on your PATH (or `~/.local/bin`). `stt` is a separate local
  tool (faster-whisper with a warm daemon) from the Telegram bridge repo
  (`tools/stt/stt`); `ku stt setup` runs its one-time model download. Or use
  any transcriber you have: `ku stt use --command "whisper-cli -f {file}"` or
  `ku stt use --mcp "CMD ARGS" --tool NAME`. `ku stt off` turns it off.
  Without it, voice files are passed through as files.
* Design: [`docs/runtime-design.md`](docs/runtime-design.md). Terms check:
  [`docs/terms-check.md`](docs/terms-check.md). Plan:
  [`docs/phase-1-plan.md`](docs/phase-1-plan.md).

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
| `examples/m1-demo.sh` | Milestone 1 demo |
| `docs/` | Design, terms check, plans |
| `scripts/lint-paths.py` | CI hard-coded path linter |
| `legacy/` | Frozen earlier work: guest-hosting pilot (Python/Go), Phase 0 Python skeleton |

## Licence

Apache License 2.0, see `LICENSE`. "Claude" and "Claude Code" are Anthropic
trademarks. Kurisu is independent and not affiliated with or endorsed by
Anthropic, OpenAI, Databricks, Meta or Telegram.
