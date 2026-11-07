# Kurisu

**A self-hosted AI team you run from the command line.** You talk to a **Lead**
session; it creates and messages **worker** sessions as peers. Each session is
the real, interactive CLI you already use (Claude Code today, Codex next),
running under **your own login** on your own machine. Kurisu is a small Rust
binary (about 2 MB, about 3 MB of RAM when idle) that starts, resumes and
connects those sessions.

> Status: **Milestone 1** (CLI, Claude driver, peer sessions through
> kurisu-mcp). Linux and macOS. Not yet: Telegram/web channels, Codex sessions
> in the CLI, permission prompts. See [Gaps](#gaps).

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
cargo install --path crates/kurisu        # puts `kurisu` in ~/.cargo/bin
# or: cargo build --release  &&  export PATH="$PWD/target/release:$PATH"
```

### 3. Check your setup

```bash
kurisu status
```

This shows the `claude` (and `codex`, if present) path, version and login
state, whether the Kurisu server is running, and the sessions Kurisu knows
about.

### 4. Give the Lead a task

```bash
kurisu lead "Create two workers with session_create, ask worker-a what 17+25 is and worker-b what the capital of Australia is, then summarise their answers."
```

The first command starts the Kurisu server in the background. The Lead is an
interactive `claude` session. It discovers peers with `peers_list`, creates
workers with `session_create` and messages them with `session_send`, all
through the `kurisu-mcp` server that Kurisu injects into every session. Every
session uses **`claude-haiku-5-5`** by default, to keep testing cheap.

### 5. Watch and talk

```bash
kurisu sessions                 # who exists: role, creator, model, effort, status, description
kurisu history lead             # the Lead's conversation (user / agent / peer:<id>)
kurisu history worker-a
kurisu send worker-a "And 17 * 2? Just the number."
kurisu update worker-a --effort low --description "arithmetic specialist"
kurisu audit                    # who changed what
kurisu usage                    # token usage from the CLIs' transcripts
```

To watch a session's live terminal:

```bash
kurisu attach worker-a          # detach with Ctrl-b d (don't type into it mid-turn)
```

### 6. Run the Milestone 1 demo

```bash
KURISU="$(command -v kurisu)" examples/m1-demo.sh
```

The demo uses a fresh `KURISU_HOME` under `/tmp`. The Lead creates two workers,
finds them with `peers_list`, asks each one a question, changes one worker's
description and effort with `session_update`, and has the other worker try to
modify the Lead, which is denied. It then prints sessions, histories, the
audit log and token usage, and shuts down. Set `KEEP=1` to leave it running.

### 7. Stop and clean up

```bash
kurisu shutdown                 # stops the server and every claude it started (transcripts are kept)
```

Kurisu's data (SQLite store, per-session hook files, workspaces) lives in:

* macOS: `~/Library/Application Support/kurisu`
* Linux: `${XDG_DATA_HOME:-~/.local/share}/kurisu`
* or wherever `KURISU_HOME` points.

Delete that folder to forget all sessions. Claude's own transcripts stay in
Claude Code's usual place (`~/.claude/projects/`).

## How it works

* `kurisu` is one binary. `kurisu serve` (started on demand) owns a SQLite
  store and a unix socket. Every other subcommand is a client.
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
* The Codex driver exists (`kurisu smoke codex <dir>`), but Codex sessions are
  not wired into the CLI and kurisu-mcp yet.
* No Telegram or web channel yet. The Telegram bridge port is planned
  (`docs/bridge-reuse.md`).
* Each live `claude` uses about 300–500 MB. Stop sessions you don't need
  (`kurisu stop <id>`) or run `kurisu shutdown`.

## Repository layout

| Path | What |
| --- | --- |
| `crates/kurisu` | The `kurisu` CLI and server |
| `crates/claude-cli-acp` | The Claude driver, plus a standalone ACP agent |
| `examples/m1-demo.sh` | Milestone 1 demo |
| `docs/` | Design, terms check, plans |
| `scripts/lint-paths.py` | CI hard-coded path linter |
| `legacy/` | Frozen earlier work: guest-hosting pilot (Python/Go), Phase 0 Python skeleton |

## Licence

Apache License 2.0, see `LICENSE`. "Claude" and "Claude Code" are Anthropic
trademarks. Kurisu is independent and not affiliated with or endorsed by
Anthropic, OpenAI, Databricks, Meta or Telegram.
