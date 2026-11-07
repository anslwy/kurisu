# Kurisu

**Your AI team in one chat.** Kurisu is a small self-hosted program that runs
Claude Code and Codex for you, on your own machine and your own subscriptions,
and lets you run all of it from one conversation: in the terminal, a local web
page, or Telegram.

[![ci](https://github.com/anslwy/kurisu/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/anslwy/kurisu/actions/workflows/ci.yml)

## Why Kurisu

1. **Talk to one bot.** You only talk to Kurisu. It creates and manages your
   sessions (sub-bots) for you. No more babysitting ten concurrent sessions
   yourself.
2. **Native intelligence.** Opus on Claude Code and GPT-6 Astra on Codex, each
   running in its own native CLI, so you get the best intelligence for your
   money: your existing subscriptions, no API keys.
3. **Multiple CLIs.** Claude Code and Codex. Continue any session on the other
   harness and it picks up the history.

## How it works

- **A Lead and its peers.** You talk to the **Lead**, a long-lived session. It
  creates **worker** sessions for each job, messages them and reports back.
  Every session is a real, interactive `claude` or `codex` running under your
  login. Sessions are peers, not sub-agents, and each keeps its own context.
- **kurisu-mcp.** Kurisu gives every session a small set of tools: who exists
  (`peers_list`), create, message, read and hand over sessions, schedule
  wake-ups (a daily standup, "remind me in 30 minutes"), and a shared board.
- **One history, any CLI.** Every message, reply and tool call is stored in
  Kurisu's own SQLite database. "Look at session X and continue from there"
  works across Claude Code and Codex, and across a harness switch.
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

Every binary knows the exact commit it was built from (`ku --version`) and can
rebuild itself: `ku source get | check | build | install` clones that commit,
checks the prerequisites, builds, and swaps the new binary in, keeping the old
one as a fallback (`ku source reset` goes back to the shipped one). So you can
ask the Lead to add a feature to Kurisu itself.

## Quickstart

```bash
ku status                       # your CLIs, versions and logins
ku lead "Create two workers, ask one for 17+25 and the other for the capital of Australia, then summarise."
ku sessions                     # the team
ku history lead                 # the Lead's conversation
ku web                          # the same in a local web page
```

New sessions use whichever CLI is installed and logged in (Claude Code first).
Pick one with `--harness claude|codex`; move a session later with
`ku update <name> --harness codex`. The full CLI tour (approvals, rules, MCP
servers, the web UI) is in [docs/guide.md](docs/guide.md).

## Telegram

```bash
ku telegram setup
```

It walks you through it: create a bot with @BotFather (`/newbot`), paste the
token, then send `/start` to your bot so Kurisu finds your chat. Only that chat
can use the bot. From then on you talk to the Lead from your phone: replies,
approval and question cards, photos and files, `/list`, `/model` (pick the
harness, then the model), `/usage`, `/compact`, `/stop`, and the final replies
of every session.

## Optional add-ons

Nothing here runs until you turn it on.

- **Voice notes.** With a working `stt` command (a local faster-whisper tool,
  or any transcriber via `ku stt use --command "…"`), voice notes become text.
- **Live Browser.** A real, shared Chrome for every session through the `lb`
  tools ([live-browser](https://github.com/anslwy/live-browser)). Send
  `/browser` in Telegram to watch or take over from your phone.
- **Remote terminal.** Send `/terminal` in Telegram for a one-time link to a
  shell in your phone's browser.

`/terminal` and `/browser` can be turned on from the phone: an approval card
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

## More

- [docs/guide.md](docs/guide.md): the CLI tour, approvals, rules, MCP servers, web UI, known gaps
- [docs/api.md](docs/api.md): the HTTP API and every feature in detail
- [docs/runtime-design.md](docs/runtime-design.md): how it is built

Status: alpha, Linux and macOS. Apache License 2.0 (`LICENSE`). "Claude" and
"Claude Code" are Anthropic trademarks; Kurisu is independent and not
affiliated with or endorsed by Anthropic, OpenAI or Telegram.
