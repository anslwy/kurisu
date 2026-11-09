# Discord: DMs and private guild conversations

Discord is an optional channel for the existing Kurisu server. A locally paired
person has a DM session. Explicitly linked Telegram and Discord identities share
that person's DM selection and history. Guild conversations get separate sessions
and private reply threads. Telegram's existing raw payloads, 3900-character
budget, cards and retry behavior remain in its adapter.

## Install and configure

Create a bot in the [Developer Portal](https://discord.com/developers/applications)
and install it into the intended guild with the `bot` and `applications.commands`
scopes. A human `discord.gg` invite does not install a bot. Kurisu discovers the
application ID from authenticated `GET /users/@me` during registration.

The bot needs View Channel, Send Messages, Attach Files and Read Message History.
Guild text conversations additionally need Create Private Threads and Send
Messages in Threads. Administrator and Guild Members access are unnecessary.
The bot creates private threads with `invitable=false` and adds only the exact
paired author. Discord administrators and thread moderators remain trusted:
their Discord permissions can override thread privacy or add other members.

Enable **Message Content** in the Developer Portal for ordinary guild text.
Identify requests intents **37377**: `GUILDS` (1), `GUILD_MESSAGES` (512),
`DIRECT_MESSAGES` (4096), `MESSAGE_CONTENT` (32768). It never requests
`GUILD_MEMBERS` or enumerates guild/thread members. See the official
[intent](https://docs.discord.com/developers/events/gateway#gateway-intents) and
[thread](https://docs.discord.com/developers/topics/threads) documentation.

Configure only the intended home:

```sh
ku discord on --token-file TOKEN_FILE --guild GUILD_ID
ku discord status
ku discord off
ku discord on
```

`<KURISU_HOME>/discord.json` is written atomically with mode 0600. Keep the home
private too. Token files must be mode 0600; `--token-stdin` avoids exposing a
credential in argv. Prefer either over `--token`. Diagnostics and transport
errors redact the bot credential. Configuration reloads the channel in that home;
it does not enable Discord in another instance.

`KURISU_DISCORD_API` and `KURISU_DISCORD_GATEWAY` are local fixture overrides;
normal installations leave them unset. Remote Gateway URLs must use Discord's
secure Gateway hosts; local WebSocket overrides are restricted to loopback.
Channel `running` reports task liveness, not proof of a READY receipt or a human
conversation. A fatal authentication/intent close stops the task with an error.

## One active home per bot

A bot must have one authoritative Kurisu home. Two homes using the same bot
receive the same events but can have different pairing databases: one can reject
a sender while an older paired home answers with a model or its session roster.
Stop the scratch home's Discord channel with `ku discord off` before enabling
that bot in live. Its sessions and pairings remain intact. Use distinct bots for
concurrent homes; an older binary without the shared lease must remain off.

Kurisu acquires a private per-bot advisory lock in the user's shared data
registry, outside `KURISU_HOME`, before registering commands or starting an inbox
worker/Gateway. A second home fails closed. Stop joins ingress and relay tasks
before releasing the lock. The lock contains only bot ID, owner home and PID,
never a token. This coordinates homes sharing that user data directory; it is
not a distributed lock across hosts or an OS security boundary.

Unpaired ordinary DM messages get only the pairing notice. Unpaired
slash/component interactions get that notice ephemerally. These events are
durably rejected before they can enter the worker queue, even if pairing
happens later. Valid `/start CODE`
in a DM remains the explicit pairing exception.

## Pair people locally

```sh
ku discord pair --person alice --user DISCORD_USER_ID
ku discord pair --person bob --user BOBS_DISCORD_USER_ID
ku discord pair --person owner --session TELEGRAM_SESSION_ID --user OWNER_USER_ID
```

Pairing is a local administrator operation. It creates no Discord channel or
message and works before installation. The printed code lasts five minutes and
is one use. The person must open the bot DM and send `/start CODE`; codes cannot
be redeemed in a guild. `--user` restricts redemption to that exact Discord user;
without it, possession of the code authorizes redemption. Unpaired guild authors,
bots, webhooks and system messages cannot start an agent turn. Guild membership
never substitutes for pairing.

`--session SESSION_ID` explicitly grants an existing, unowned session to a person.
A session already owned by another person cannot be paired or selected. Existing
people pair with their current DM session. Legacy Telegram selections are imported
without rewriting the old files. Legacy sessions selected by multiple people are
quarantined rather than assigned to one of them; the local operator must resolve
the conflicting bindings before they can be used through Discord.

## Guild and thread behavior

Only the configured guild admits ingress. A paired author's message in a guild
text channel selects that person's private Kurisu thread for that parent channel.
A different person gets a different thread and session. Another parent channel
gets another conversation. Messages in a bot-owned private thread return to that
thread's session. Public threads, forum posts, unknown private threads and another
person's thread are rejected. No private reply falls back to a public parent.
Text that a human posts publicly remains public; bot replies and session output
use the private route.

Thread IDs and invitation receipts are durable. Restart can retry an idempotent
member invitation without creating another thread. An uncertain thread-create
receipt reserves the conversation and refuses another create. The local operator
must reconcile that `discord_threads` row with Discord before retrying; automatic
retries must not create duplicate or incorrectly shared threads.

`/new` creates another session in the current conversation. `/use` and `/list`
show only sessions owned by that person and eligible for that DM or private
thread. A guild session cannot be selected in a different thread or DM. Old
sessions retain their conversation binding, including after a harness switch.

## Commands, approvals and files

The 13 application commands are registered for guild and bot-DM contexts:
`/start`, `/help`, `/new`, `/use`, `/id`, `/list`, `/status`, `/model`, `/usage`,
`/stop`, `/send_now`, `/web`, `/terminal`. Pairing remains DM-only. Text commands
use the same dispatcher. `/list PAGE` uses zero-based pages of five sessions.
`/model` offers harness/model/effort choices; changes use the existing
history-linked replacement. Members' `/usage` is session-scoped. The locally
paired `owner` can inspect host plan usage and use `/web` and `/terminal`.

Approval/question and queue-release cards bind their person, destination and
session. Approve/deny grants are one time; buttons cannot grant persistent or
session-wide rules. Stale model, approval or release controls cannot act on
another session or person. Interactions acknowledge before slow thread creation
or agent work; persistent messages carry the conversation.

Attachments are size checked (25 MiB download, 10 MiB outbound), use sanitized
basenames and enter the selected workspace's upload path. `send_file` follows
owned delegation to the originating destination. A member's file with no private
Discord route fails instead of falling back to the Telegram owner's chat. Success
requires an accepted Discord message ID. `ku discord notify TEXT` is a local
operator broadcast to paired DMs.

Text obeys Discord's 2000-character limit without cutting URLs or code spans.
Indivisible oversize text becomes an exact UTF-8 attachment. Streaming retains
message identities, coalesces edits and obeys pacing/429 Retry-After.

## Resume and missed-message recovery

`<KURISU_HOME>/channels.sqlite3` stores Gateway session/sequence, raw receipts,
channel watermarks, pending history pages, person bindings, inbox and outbox.
A Gateway sequence advances only in the transaction that durably records its
receipt. HTTP dispatch and history recovery run outside the WebSocket read loop.

Reconnect uses the advertised resume URL, session ID and last committed sequence.
Gateway replay reuses event IDs. Invalid sessions and close codes 4007/4009 clear
the checkpoint and re-identify; authentication/intent/sharding fatal codes stop
instead of retrying forever. See Discord's
[resume protocol](https://docs.discord.com/developers/events/gateway#resuming).

A fresh READY schedules backfill for tracked channels and DMs. History pages are
fetched newest to oldest, stored before their page cursor advances, then released
oldest first per channel. Concurrent live receipts and REST/Gateway replay share
one event key; inbox and server source-event dedupe prevent another user row or
turn. Recovery resumes unfinished pages after restart and retains failed requests
for retry. Rejections and pending recovery counts remain visible in delivery
health. Legacy DM inbox IDs seed initial watermarks; newly discovered guild text
channels start at their current message ID rather than replaying old history.

History recovery requires View Channel and Read Message History. Deleted or
inaccessible messages, never-observed DMs/channels and events outside Discord's
resume/history retention cannot be recovered. Empty history can also mean missing
history permission. This is not an unconditional exactly-once delivery guarantee.
A lost outbound receipt remains uncertain and is not blindly re-sent.

## Trust boundary and verification

Discord requests and regular paired sessions' MCP calls enforce person ownership
for session lists, direct history/control, model changes, approvals, uploads and
files. Delegated sessions inherit ownership. New person-created workspaces use
separate private directories. Global boards, owner Telegram history, account-wide
reports and administrative operations are unavailable to member callers. The
local host operator and the explicitly paired `owner` remain trusted; that
operator's existing MCP privileges are retained.

These checks are application authorization. They do not isolate host processes,
filesystem access, credentials or the local control socket. Untrusted people
running arbitrary host tools still need separate OS/container boundaries and
credentials. This change does not configure a family installation or impose
model/spend caps.

Local tests use fake HTTP/WebSockets, restartable SQLite and the real session
store, without reading a real token or launching a human account. A separate
scratch credential/Gateway probe can establish protocol authentication and resume;
it does not prove guild/private-thread conversation, human approvals or file
round-trips. Live enable and human checks require a separately chosen home and
operator action. No deployment is implied by merging or testing this code.

STT/voice and additional commands remain outside this slice. Telegram's three
reliability follow-ups remain separate; see
[the follow-up plans](telegram-reliability-followups.md).
