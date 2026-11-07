# Phase 1 plan (revised for the Rust runtime)

Goal: from install to a first delegated, QA-gated change in 30 minutes or
less, single user, Telegram first. Estimates assume one developer working with
agents; ranges cover CLI-behaviour surprises.

| # | Work | Est. |
| --- | --- | --- |
| 1 | **Driver hardening (claude-cli-acp):** permission prompts to ACP `request_permission` (Notification/PreToolUse hooks), cancel, `/compact`, model and effort flags, trust/onboarding dialog detection with a clear error, crash detection (pane dead → `error`), idle eviction and lazy resume | 1.5–2 wk |
| 2 | **Codex driver:** approvals to ACP permission cards, `turn/interrupt`, model and effort per turn, app-server restart and backoff, schema drift test against `generate-json-schema`, pinned version range | 1 wk |
| 3 | **kurisu core:** config and roster in serde (port of the Phase 0 Python schemas and their tests); events table and broadcast `subscribe`; approvals table (single use, expiry, audit); concurrency cap; graceful shutdown that kills children | 1–1.5 wk |
| 4 | **Peer tools:** non-blocking `session_send` with `session_wait` and done notifications to the Lead; per-role tool scoping; kurisu-mcp for Codex threads (`config.mcp_servers`); Claude injection done in M1 | 1 wk |
| 5 | **Router and scheduler:** per-task tier to model and effort (switching only between tasks), struggle escalation, cost and usage ledger; cron schedules (standup) | 1 wk |
| 6 | **Telegram channel:** port the bridge onto kurisu's socket (dispatch, outage retry, cards, attachments, `/status`, `/usage`), owner-only gate; Apache-2.0 relicence (assumed approved) | 1.5–2 wk |
| 7 | **Install and operations:** `kurisu init` (check that the claude and codex CLIs exist, choose a channel, roster template), `doctor`, opt-in user service, `update` with backup, static release builds and checksums, a 30-minute test on a fresh VM | 1 wk |
| 8 | **Docs and security:** trusted local mode statement, SECURITY.md, terms re-check (OpenAI terms in a browser), README quick start | 0.5 wk |

**Total: about 8.5–10.5 weeks.** That is close to the earlier estimate for
"our own runtime" (2–3 months) minus the Claude driver risk, which the spike
has now retired.

Exit criteria:
* both drivers pass their live smoke tests (new, prompt, kill, resume, prompt)
  and a permission round-trip;
* kurisu idle RSS stays under 10 MB;
* a fresh machine reaches its first QA-gated change in 30 minutes or less;
* gitleaks and the path linter are clean;
* the fresh public repository is prepared, but not published without the
  user's go-ahead.

Fresh public repo (decided): when Phase 1 is done, create a new repository
from the current tree only (no history). Run gitleaks and the path linter
before the first push, and set the author identity to the user's chosen public
name.
