# Telegram bridge reuse inventory

Source: `anslwy/omnigent-telegram-bridge` at `074fbfe` (read-only; the live
bridge is the user's only line in and was not changed). Shape: one 3,873-line
`telegram-bridge.py` (159 functions, stdlib plus optional Pillow), a Bash
launcher and 13 test files (116 tests).

## Recommendation

**Start Kurisu's Telegram channel and Lead UI from the bridge. Don't rewrite.**
It already solves the hard, battle-tested parts: ordering under a busy session,
replaying after a crash, riding out network outages, question and approval
cards, and session switching. A rewrite would rediscover those bugs. The work
is a **port plus refactor**: split the single file into the Kurisu modules
below, swap module-level globals and `~/...` paths for injected config, and
put Omnigent calls behind `kurisu.runtime`. Copy the bridge's tests over with
the code, and they must keep passing at each step.

Do it as a **copy into Kurisu**, not a move: the live bridge keeps running
unchanged until Kurisu's channel has matched it in daily use. Then the user can
switch, and the bridge repo is archived.

## Feature → Kurisu module

| Bridge feature (functions) | Kurisu module | Port as-is / refactor |
| --- | --- | --- |
| Telegram transport: `tg_request`, `send_text`, `split_text`, `send_typing`, mention handling, `sync_bot_commands` | `kurisu.channels.telegram` | Port as-is. Inject the token from config instead of a global. |
| Owner gate: `allowed_sender`, `is_owner_dm`, `scope_for` | `kurisu.channels` (common) | Port. Make "owner only" the default and required in self-host mode. |
| Queuing and ordering: `enqueue_work`, `_worker_loop`, `drain_queued_work`, `begin/end_active`, busy dispatch, `/send-now` | `kurisu.channels.dispatch` | Refactor: replace the module globals (`WORK_QUEUE`, `ACTIVE_SESSIONS`, locks) with a `Dispatcher` object. Logic unchanged. |
| Restart handling: `record_pending`/`clear_pending`, `replay_pending_updates`, `initial_offset`, `mark_relayed`/`was_relayed` | `kurisu.channels.dispatch` + `kurisu.memory` (state store) | Refactor the state store: atomic JSON under the Kurisu data dir. Replay semantics stay as they are. |
| Outage retry: `is_network_error`, `take_outage`, `outage_notice_text`, retry budgets | `kurisu.channels.telegram` | Port as-is (19 tests). |
| Relay of turns: `_relay_turn`, `wait_until_idle`, `flush_turn_backlog`, `item_text` | `kurisu.runtime` (Omnigent adapter) + channel | Refactor: the adapter exposes `send`, `items_after` and `wait_idle`; the channel only formats. |
| Attachments: `attachment_from_message`, `tg_download_file`, `compress_image`, `omni_upload_file`, `deliver_attachment` | channel (download) + `kurisu.runtime` (upload) | Refactor the paths: `~/telegram-uploads` becomes `<data>/uploads`. |
| Question cards: `question_spec`, `post_question_card`, `handle_question_callback`, text answers | `kurisu.approvals` (model) + channel (render) | Refactor: card state and outcome go in `approvals`, so a web UI can reuse them; Telegram keeps only the rendering. 18 tests. |
| Approvals: `approvals_once`, `approval_prompt_text`, `handle_approval_callback`, `resolve_elicitation` | `kurisu.approvals` | Refactor as above. Add the Kurisu policy (single use, expiry, audit log). |
| Broadcast of staff results: `broadcast_once`, `_broadcast_loop` | `kurisu.roster` + channel | Refactor: broadcast only roster members' final messages to the Lead or user. |
| Session list and switch: `fetch_sessions`, `session_overview`, `/list`, `/use`, `/new`, `/id` | `kurisu.roster` (Lead UI) | Refactor: show roster roles by name first, raw sessions second. `/use` becomes "talk to role X" (still through the Lead by default). |
| `/catch-up` | `kurisu.roster` (standup) | Port. It's the seed of the daily standup. |
| `/status`, context usage, Pi-native fallback | `kurisu.runtime` | Port. The Pi-native reader stays an adapter detail. |
| `/compact` with watch | `kurisu.runtime` | Port as-is (8 tests). |
| `/stop` (interrupt) | `kurisu.runtime` | Port as-is. |
| `/model` with picker and filters | `kurisu.router` | Refactor: the router owns model choice (tiers, switching only between tasks). `/model` becomes a manual override for the selected role that is logged and respects the budget. |
| `/usage`: `official_usage`, `local_usage_estimate`, `format_usage` | `kurisu.router` (cost ledger) | Refactor. It reads Omnigent's status-line captures under `/tmp/omnigent-<uid>/…` and an optional `claude-monitor`, both internal paths, and a docstring names an IT Admin path. Move these behind the adapter as optional sources. |
| `/mcp` (Claude and Codex MCP toggles) | `kurisu.runtime` (per-role MCP) | **Refactor carefully.** It edits `~/.claude.json` directly. That file also holds Claude account data, and `docs/terms-check.md` (condition 1.2) forbids touching credentials. Use `claude mcp add/remove` or per-role `.mcp.json` in the role's folder instead, and never read the whole `~/.claude.json`. The `lb` preset at `~/.local/bin/lb` is user-specific: drop it or make it a configurable preset. |
| Config: `load_config` (`~/.config/telegram-bridge/env`), state (`~/.local/state/telegram-bridge`) | `kurisu.config` (`kurisu.yaml`) | Replace with the Kurisu schema and XDG paths. Keep env overrides for secrets only. |
| Launcher (`telegram-bridge run/start/stop/logs`) | `kurisu start` / `kurisu service install` | Replace. The pidfile/setsid approach is superseded by the foreground run plus an opt-in user service. |

## Things to remove or generalise while porting

* Absolute and user-specific paths: the IT Admin path in the `official_usage`
  docstring, `~/.local/bin/lb`, `~/telegram-uploads`, the default
  `OMNIGENT_WORKSPACE=~`.
* Module-level mutable globals (about 15). They block testing several channels
  in one process and running a web channel next to Telegram.
* Direct reads of harness config files, as described for `/mcp` above.
* Licence: the bridge repo has no LICENSE file. Its only author is the repo
  owner, so the owner must confirm that this code may be relicensed under
  Kurisu's Apache-2.0 when it is copied in.

## Order (Phase 1)

1. Copy the bridge and its tests into `kurisu/channels/telegram/_bridge_port.py`
   unchanged, plus a shim, and get its 116 tests green inside the Kurisu repo.
2. Pull out `Dispatcher` and the state store, keeping the tests green.
3. Put Omnigent calls behind `kurisu.runtime.OmnigentRuntime`, keeping the tests green.
4. Move cards to `kurisu.approvals`, and model and usage to `kurisu.router`.
5. Switch the config to `kurisu.yaml`, add the roster-aware Lead UI, and retire
   the shim.
