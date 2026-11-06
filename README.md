# Kurisu

**A self-hosted AI team you manage from one chat.** You talk only to a **Lead**.
The Lead delegates to named, long-lived **Staff**: a Dev per repository, a QA
black-box tester that gates merges, an IT Admin, or roles you define. It
runs on your own machine, with your own Claude Code login, through a Telegram
bot or a local web UI.

> **Status: Phase 0 (restructuring).** Nothing here is installable yet. The
> core package is a validated skeleton. The working pieces are the frozen
> guest-hosting pilot under `legacy/` and the Telegram bridge that Phase 1
> ports (`docs/bridge-reuse.md`).

## New direction (2026-11-06)

Kurisu started as a service that one operator hosts for WhatsApp guests. It is
now an **easy self-host product**: each person installs it for themselves.
Nobody hosts anything for anyone else.

* **Single user, trusted local mode by default.** Your staff act with your
  user's permissions on your machine. Per-role rootless-Podman sandboxes are an
  opt-in (Phase 2).
* **Your own Claude Code.** You install Claude Code and run `claude login`
  yourself, with your own subscription or API key. Kurisu drives the unmodified
  CLI through Omnigent and never reads, stores or forwards Claude credentials.
  See `docs/terms-check.md` for the conditions this follows.
* **Channels.** Telegram first, then a localhost web UI. WhatsApp stays
  available as an optional channel that is off by default, because it uses an
  unofficial client and can get the account banned.
* **Cost-aware routing.** Each task gets a tier (mechanical, normal or hard)
  with a matching model and effort. A worker that struggles is escalated for
  its next task. Models never change mid-task, which keeps the prompt cache.
* **Approvals.** Push, deploy, spending and outbound messages need your tap on
  a single-use, expiring approval card, and every decision is logged.
* **Memory and backups.** A shared AGENTS.md, role session identity that
  survives restarts, and nightly backups encrypted to your own `age` key.

Target for Phase 1: from install to your first delegated, QA-gated change in
30 minutes or less on any 64-bit Linux or macOS machine with 4 GB RAM.

## Layout

| Path | What |
| --- | --- |
| `kurisu/` | Core package: `channels`, `roster`, `router`, `runtime` (Omnigent adapter), `approvals`, `memory`, `backup`, `sandbox`, `config`, `cli` |
| `examples/kurisu.example.yaml`, `examples/roster.example.yaml` | Config schemas with placeholders |
| `tests/` | Core tests |
| `docs/terms-check.md` | Claude Code, Omnigent, WhatsApp and Telegram terms, each with a verdict |
| `docs/bridge-reuse.md` | How the Telegram bridge becomes Kurisu's channel and Lead UI |
| `legacy/guest-hosting/` | The frozen guest-hosting pilot (tag `pilot-freeze`), tests still green |
| `scripts/lint-paths.py`, `.github/workflows/ci.yml` | Hard-coded path linter, gitleaks, test jobs |

## Try the skeleton

```bash
python -m venv .venv && .venv/bin/pip install -e '.[test]'
.venv/bin/python -m pytest -q
cp examples/*.example.yaml /path/to/conf/   # rename to kurisu.yaml / roster.yaml, fill placeholders
.venv/bin/kurisu check --config /path/to/conf/kurisu.yaml
```

## Roadmap

* **Phase 0 (this branch):** terms check, freeze the pilot, core skeleton, CI
  hygiene, licence.
* **Phase 1:** port the Telegram bridge under its own tests; Lead, Dev and QA
  roster with the merge gate; approval cards; AGENTS.md memory; standup;
  routing with budgets; `kurisu init`, `doctor`, `start`, `update` and backups.
* **Phase 2:** per-role Podman sandbox profiles (from `legacy/`), automatic
  tier-up, role templates, web UI.
* **Phase 3:** more channels, including WhatsApp as an opt-in plugin.

## Licence

Apache License 2.0, see `LICENSE`. "Claude" and "Claude Code" are Anthropic
trademarks. Kurisu is an independent project and is not affiliated with or
endorsed by Anthropic, Databricks, Meta or Telegram.
