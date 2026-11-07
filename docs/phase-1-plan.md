# Phase 1 plan (revised after the M1 QA pass)

Goal: replicate our own lead + staff workflow on Kurisu for one user. That
means Telegram or web, approvals, async delegation, roles, a standup and
costs, and from install to a first delegated, QA-gated change in 30 minutes
or less.

Already done (M1 plus fixes): the `ku` CLI and `ku web`; the Claude driver
(tmux and hooks, resume); kurisu-mcp with a live roster; UUID ids and names;
lineage authority and audit; model validation; per-session model and effort.

Estimates assume one developer working with agents; ranges cover
CLI-behaviour surprises. QA's ranked suggestions are tagged **[QA#n]**.

| # | Work | Est. |
| --- | --- | --- |
| 1 | **Approvals [QA#1]:** Claude permission prompts (Notification/PreToolUse hooks) and Codex `requestApproval` become ACP `request_permission`; a status of `awaiting_approval`; approve or deny in `ku web` and `ku approvals`; pre-approvals per role (allowed tools and paths) as `--allowedTools` and settings; single-use, expiring and audited | 2–2.5 wk |
| 2 | **Async delegation [QA#2]:** non-blocking `session_send` (returns a turn id) plus `session_wait` and `session_inbox`; done, blocked (awaiting approval) and errored notifications to the sender session and to the user (web toast, CLI `ku watch`); streamed partial output in `ku web` (SSE) | 1.5–2 wk |
| 3 | **Session setup and role templates [QA#3, QA#5]:** `session_create` takes cwd, extra MCP servers, skills/plugins and a template; built-in Dev / QA / IT Admin templates (system prompt, tools, default model and effort, pre-approvals); defaults per role in `kurisu.yaml` (serde port of the Phase 0 schemas) | 1.5 wk |
| 4 | **Observability [QA#4, QA#7]:** `ku history --tail N`, a last-reply column in `ku sessions` and the web list, `peers_list` with the last reply; a cost view (per session and day, from transcripts, plus an estimate) in the CLI and web, with budget caps that pause new turns | 1 wk |
| 5 | **Scheduler and notes [QA#6]:** cron schedules (daily standup prompt to the Lead), a standard team notes file plus the board, a standup summary posted to the channel | 1 wk |
| 6 | **Roster export and backup [QA#8]:** `ku roster export` (an AGENTS.md replacement generated from the live store), `ku backup` / `ku restore` (encrypted with age, excluding CLI credentials) | 0.5–1 wk |
| 7 | **QA gate [QA#9]:** a gate verdict per branch: a QA session records pass/fail with notes against a repo and branch; the Lead (or a merge step) checks the verdict; shown in the web UI | 1 wk |
| 8 | **Codex in the CLI and web:** the Codex driver behind `--harness codex`, kurisu-mcp through `config.mcp_servers`, approvals through #1 | 1 wk |
| 9 | **Telegram channel:** port the bridge onto the server socket (dispatch, outage retry, cards, which carry approvals from #1, attachments, `/status`, `/usage`), owner-only | 1.5–2 wk |
| 10 | **Hardening and install:** idle eviction and a cap on live sessions; crash detection; `ku init` and `doctor`; an opt-in user service; release builds (Linux musl, macOS universal) and checksums; web UX polish (remaining QA items); SECURITY.md; a terms re-check | 1.5 wk |

**Total: about 13–16 weeks** (up from 8.5–10.5). The increase comes from QA's
items 1–9 (about 9–11 weeks); the M1 work retired about 2 weeks of driver and
peer-tool risk.

Suggested order, by what unblocks replicating our workflow: 1 → 2 → 3 → 4
→ 9 → 5 → 8 → 7 → 6 → 10. Items 1 and 2 are the minimum for real work; with
3 and 4 the team runs on its own; 9 restores the Telegram workflow.

Exit criteria:
* our own lead, Dev and QA workflow runs on Kurisu for a week, with approvals,
  async delegation, a standup and a QA gate;
* both drivers pass their live smoke tests and a permission round-trip;
* server idle RSS stays under 10 MB;
* a fresh machine reaches its first QA-gated change in 30 minutes or less;
* gitleaks and the path linter are clean;
* the fresh public repository is prepared, but not published without the
  user's go-ahead.

Fresh public repo (decided): when Phase 1 is done, create a new repository
from the current tree only (no history). Run gitleaks and the path linter
before the first push, and set the author identity to the user's chosen public
name.
