# Kurisu

**Your AI team in one chat.** Kurisu is a small self-hosted program that runs
Claude Code, Codex and OpenCode for you, on your own machine with your own configured providers,
and lets you run all of it from one conversation: in the terminal, a local web
page, or Telegram.

[![ci](https://github.com/anslwy/kurisu/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/anslwy/kurisu/actions/workflows/ci.yml)

## Why Kurisu

1. **Talk to one AI, not ten.** You only talk to Kurisu (the lead). It creates and manages your
   sessions for you (e.g. what directory should be the workspace, what MCPs / skills should use). No more babysitting ten concurrent sessions
   yourself. Kurisu knows who owns which branch so you don't have to handle 10 different merge conflicts all by yourself.
2. **Native intelligence.** Opus on Claude Code and Astra on Codex, each
   running in its own native CLI, so you get the best intelligence for your
   money. You can bring your existing subscriptions and start right away.
3. **Multiple CLIs.** Kurisu handles the chat session history across different CLIs. It can offboard one session worker and onboard it to a different harness with the same task. So you don't have to wait for one harness's usage limit to reset before continuing the work.
4. **Self improving.** Kurisu knows the exact commit it was built from (`ku --version` or /version in telegram) and can rebuild itself. For example you can ask Kurisu to a new slash command you want in Telegram and redeploy itself.
5. **Cost savings** Kurisu can assign tasks to session workers with a different model, reasoning effort and MCP server based on the task. So you don't have to pay a frontier model for a simple task that can be done by a cheaper one, or to pay the input token for a MCP server that's not useful in the task.
6. **Lean and efficient** Kurisu is written in Rust to stay lean in memory so it can run along side with different CLIs in a modest hardware.
7. **Always On** Kurisu is designed to running continuously. Turns cut off by a deploy resume automatically, and sessions resume on their next message.

## How it works

- **A Lead and its peers.** You talk to the **Lead** (Kurisu), a long-lived session. It
  creates **worker** sessions for each job like **Dev** or **QA**, messages them and reports back.
  Every session runs the installed `claude`, `codex` or `opencode` CLI under your
  login. Each worker has its own working directory, harness, model, MCPs and skills.
- **kurisu-mcp.** Kurisu gives every session a small set of tools: who exists
  (`peers_list`), create, message, read and hand over sessions, schedule
  wake-ups (a daily standup, "remind me in 30 minutes"), and a shared board.
- **One history, any CLI.** Every message, reply and tool call is stored in
  Kurisu's own SQLite database. "Look at session X and continue from there"
  works across Claude Code, Codex and OpenCode, and across a harness switch.
- **You stay in control.** Tool calls that need permission come to you as
  approval cards (Telegram, web, or `ku approve`); rules pre-approve the safe
  ones; everything is audited.

## Install

You need **tmux** and **one** of Claude Code (`claude`, logged in) or Codex
(`codex login`).

```bash
curl -fsSL https://raw.githubusercontent.com/anslwy/kurisu/main/scripts/install.sh | sh
```

The installer fetches the prebuilt binary for Linux (x86_64, arm64) or macOS
(Apple silicon, Intel), checks its SHA-256 and puts `ku` in `~/.local/bin`.
Prebuilt releases are not published yet: until they are, build from source.

**From source** (Rust and a C compiler; macOS: `xcode-select --install`,
Debian/Ubuntu: `build-essential`):

```bash
git clone https://github.com/anslwy/kurisu.git && cd kurisu
cargo install --path crates/kurisu
```

## Telegram (Recommend)

```bash
ku telegram setup
```

It walks you through it: create a bot with @BotFather (`/newbot`), paste the
token, then send `/start` to your bot so Kurisu finds your chat. Only that chat
can use the bot. From then on you talk to the Lead from your phone: replies,
approval and question cards, photos and files, `/list`, `/model` (pick the
harness, then the model), `/usage`, `/compact`, `/stop`, and the final replies
of every session.


## CLI

```bash
ku status                       # your CLIs, versions and logins
ku lead "Create two workers, ask one for 17+25 and the other for the capital of Australia, then summarise."
ku sessions                     # the team
ku history lead                 # the Lead's conversation
ku web                          # open the running instance's web chat (one-time link)
```

New sessions use whichever CLI is installed and logged in (Claude Code first).
Pick one with `--harness claude|codex|opencode`; move a session later with
`ku update <name> --harness codex`. The full CLI tour (approvals, rules, MCP
servers, the web UI) is in [docs/guide.md](docs/guide.md).

## Optional add-ons

Nothing here runs until you turn it on.

- **Voice notes.** With a working `stt` command (a local faster-whisper tool,
  or any transcriber via `ku stt use --command "…"`), voice notes become text.
- **Live Browser.** A real, shared Chrome for every session through the `lb`
  tools ([live-browser](https://github.com/anslwy/live-browser)). Send
  `/browser` in Telegram to watch or take over from your phone.
- **Remote terminal.** Send `/terminal` in Telegram for a one-time link to a
  shell in your phone's browser.
- **Web chat.** Send `/web` for a one-time link to a chat UI: every session's
  history as bubbles, live updates, and a box to reply.

`/terminal`, `/browser` and `/web` can be turned on from the phone: an approval card
asks first. They listen on your Tailscale address (or your LAN), never on the
open internet, and every link shows a pairing code you approve in Telegram
before anything opens. Install Tailscale on the computer and the phone to use
them away from home.

## Small

Kurisu itself uses about **9 MB of RAM** with Telegram and the add-ons on (our
previous stack used about 2.6 GB). Each live `claude` or `codex` takes what it
always takes; Kurisu stops idle ones on request (`ku stop`, `ku shutdown`).

## Terms

Kurisu drives the real, unmodified interactive CLIs (Claude Code in a private
tmux, Codex through its app server) under your own login, exactly as you would.
It never uses the Agent SDK, never reads or stores your credentials, and never
edits `~/.claude.json` or `~/.codex/config.toml`. See
[docs/terms-check.md](docs/terms-check.md).

OpenCode is supported through the installed CLI’s ACP interface. See [OpenCode setup, capabilities and Telegram command matrix](docs/opencode.md).
