# Telegram reliability: three separate follow-up PRs

These changes are deliberately excluded from Discord Phase 2A. Telegram's
current observable behavior, payloads and existing tests are preserved.

1. **Persist inbound update before advancing Telegram offset.** At base
   `cd0ff59d83`, `telegram/bridge.rs:2751` saves the next offset before the
   corresponding pending update is durably recorded. A separate PR should add
   an atomic inbox/offset boundary and crash-injection tests without changing
   first-boot semantics accidentally.
2. **Do not retry ambiguous sends in the outer text delivery wrapper.**
   `telegram/tg.rs:272` retries all first errors even though the inner transport
   treats lost responses as uncertain. A separate PR should preserve safe
   pre-delivery retries while preventing duplicate bubbles after lost receipts;
   test the entire outer send path and fallback behavior.
3. **Update recorded history after stream-tail edits.**
   `telegram/bridge.rs:486` edits the visible bubble but does not amend the
   recorded outgoing Telegram-history text. A separate PR should reconcile
   history with accepted edits and test `/telegram_history` after #77 tails.

Each PR needs explicit approval of the behavior change and its own Telegram
regression tests. This document neither implements nor approves those fixes.
