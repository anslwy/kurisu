# Kurisu

**A self-hosted AI team you run from the command line.** You talk to a **Lead**
session; it creates and messages **worker** sessions as peers. Each session is
the real, interactive CLI you already use (Claude Code today, Codex next),
running under **your own login** on your own machine. Kurisu is a small Rust
binary (about 2 MB, about 3 MB of RAM when idle) that starts, resumes and
connects those sessions.

> Status: **Milestone 1** (CLI `ku`, local web UI `ku web`, Claude driver, peer
> sessions through kurisu-mcp). Linux and macOS. Not yet: Telegram, Codex
> sessions in the CLI, permission prompts. See [Gaps](#gaps).

The CLI is installed as both **`ku`** and `kurisu` (the same binary). Every
session has a permanent **UUID** and a **name** (unique among live sessions).
Anywhere a command takes a session, you can give either.

## Quickstart (macOS and Linux)

### 1. Prerequisites

| | macOS | Linux |
| --- | --- | --- |
| tmux | `brew install tmux` | `sudo apt install tmux` (or your distro's package) |
| Rust (stable) | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` | same |
| C toolchain (for bundled SQLite) | `xcode-select --install` | `sudo apt install build-essential` |
| Claude Code | install it, then run `claude` once and log in | same |

Check that Claude Code is logged in:

```bash
claude auth status --text
```

Kurisu never reads, stores or forwards your Claude credentials. It runs the
unmodified `claude` binary in a private tmux server and talks to it through
Claude Code hooks.

### 2. Build and install

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

This shows the `claude` (and `codex`, if present) path, version and login
state, whether the Kurisu server is running, and the sessions Kurisu knows
about.

### 4. Give the Lead a task

```bash
ku lead "Create two workers with session_create, ask worker-a what 17+25 is and worker-b what the capital of Australia is, then summarise their answers."
```

The first command starts the Kurisu server in the background. The Lead is an
interactive `claude` session. It discovers peers with `peers_list`, creates
workers with `session_create` and messages them with `session_send`, all
through the `kurisu-mcp` server that Kurisu injects into every session. Every
session uses **`claude-haiku-5-5`** by default, to keep testing cheap.

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

Open the printed URL, including the `#token=…` part. The page lets you:
* see the session list (name, role, created by, model, effort, status, description);
* open a session and watch its history live (polls every 1.5 s);
* send it a message;
* give the Lead a task, which starts it if needed;
* update a session's model, effort or description;
* stop its CLI.

The UI binds `127.0.0.1` only. Every API call needs the token, and requests
with a foreign `Host` header are refused. Ctrl-C stops the web UI; sessions
keep running until `ku shutdown`. Options: `--port 8787` (`0` picks a free
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

### 7. Run the Milestone 1 demo

```bash
examples/m1-demo.sh             # uses `ku` from PATH (or KURISU=/path/to/ku)
```

The demo uses a fresh `KURISU_HOME` under `/tmp`. The Lead creates two workers,
finds them with `peers_list`, asks each one a question, changes one worker's
description and effort with `session_update`, and has the other worker try to
modify the Lead, which is denied. It then prints sessions, histories, the
audit log and token usage, and shuts down. Set `KEEP=1` to leave it running.

### 8. Stop and clean up

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
  `~/.claude.json`). It provides `whoami`, `peers_list`, `session_create`,
  `session_send`, `session_history`, `session_update`, `set_my_status` and the
  shared board. Sessions are peers. `created_by_session` records lineage, and
  only ancestors (or you) can modify a session.
* Design: [`docs/runtime-design.md`](docs/runtime-design.md). Terms check:
  [`docs/terms-check.md`](docs/terms-check.md). Plan:
  [`docs/phase-1-plan.md`](docs/phase-1-plan.md).

## Gaps

* **Permission prompts are not surfaced yet.** Sessions run in Claude's
  `default` permission mode, and only the kurisu-mcp tools are pre-allowed. If
  a session tries another tool (Bash, file edits), its turn waits on a prompt
  that nobody answers. Attach with tmux to answer it, or stop the session.
  Phase 1 turns these into approval requests.
* `session_send` blocks until the peer replies, so the Lead talks to workers
  one at a time.
* The Codex driver exists (`ku smoke codex <dir>`), but Codex sessions are
  not wired into the CLI and kurisu-mcp yet.
* No Telegram channel yet (`docs/bridge-reuse.md`). `ku web` sends wait for
  the whole turn, and its history view polls; there is no streaming of
  partial output yet.
* Data from before the UUID change (M1 dev builds) is not migrated: Kurisu
  refuses to open it, so delete that `KURISU_HOME`.
* Each live `claude` uses about 300–500 MB. Stop sessions you don't need
  (`ku stop <name>`) or run `ku shutdown`.

## Repository layout

| Path | What |
| --- | --- |
| `crates/kurisu` | The `ku`/`kurisu` CLI, server and web UI |
| `crates/claude-cli-acp` | The Claude driver, plus a standalone ACP agent |
| `examples/m1-demo.sh` | Milestone 1 demo |
| `docs/` | Design, terms check, plans |
| `scripts/lint-paths.py` | CI hard-coded path linter |
| `legacy/` | Frozen earlier work: guest-hosting pilot (Python/Go), Phase 0 Python skeleton |

## Licence

Apache License 2.0, see `LICENSE`. "Claude" and "Claude Code" are Anthropic
trademarks. Kurisu is independent and not affiliated with or endorsed by
Anthropic, OpenAI, Databricks, Meta or Telegram.
