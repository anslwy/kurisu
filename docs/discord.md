# Discord DMs (Phase 2A)

Discord is an optional way to reach the existing Kurisu sessions. Each paired
person has an active session; explicitly linked Telegram and Discord identities
use that same binding and history. Telegram keeps its existing raw text,
3900-character chunk budget, cards and retry behavior.

## Create and invite the bot

1. Open the [Discord Developer Portal](https://discord.com/developers/applications),
   create an application, and open its **Bot** page. Create/reset its bot token.
   Keep the token out of source control and logs.
2. Use the bot/application `id` returned by authenticated `GET /users/@me`.
   Kurisu discovers it from that response rather than hardcoding an ID. Install
   the bot into a server shared with the family using the OAuth2 bot install URL:

   ```text
   https://discord.com/oauth2/authorize?client_id=APPLICATION_ID&permissions=101376&scope=bot%20applications.commands
   ```

   A human `discord.gg` invitation only lets a person join a server; it does not
   install a bot. Optionally append `&guild_id=GUILD_ID&disable_guild_select=true`
   to select the intended server. These permissions are View Channels, Send Messages, Attach Files and Read
   Message History. Administrator permission is unnecessary. Each family member
   must be able to open a DM with the bot; Discord privacy settings may require
   allowing DMs from the shared server.
3. **DM intents:** Kurisu requests `DIRECT_MESSAGES` (standard intent, bit 12).
   No privileged intent is required by this slice. Discord explicitly exempts
   bot DMs from `MESSAGE_CONTENT`. That privileged intent is needed for broader
   guild message content support, which is outside 2A. `GUILD_MEMBERS` and
   `GUILD_PRESENCES` are unnecessary. Do not enable privileged intents merely for
   pairing or bot DMs. See [Discord's intent documentation](https://docs.discord.com/developers/events/gateway#message-content-intent).

## Configure the selected Kurisu home

```sh
ku discord on --token-file /private/path/bot-token --guild YOUR_GUILD_ID
ku discord status
ku discord off
ku discord on
```

Configuration is stored only in `<KURISU_HOME>/discord.json`, atomically and
with mode 0600. Output, Debug formatting and transport errors redact the token.
The private token file must have mode 0600. `--token-stdin` also reads the
credential without putting it in argv. The supported `--token` argument can be
visible in local process listings/shell history, so prefer file/stdin input.
The guild ID controls the documented
target installation; configuration does not prove the bot has joined that guild,
and guild membership never substitutes for local pairing.

The channel starts/stops with the daemon and can be reloaded with these commands.
An invalid token/registration failure appears as `discord: error`, not as a
successful live connection. Enabling configuration alone does not prove a bot
round-trip. `KURISU_DISCORD_API` and `KURISU_DISCORD_GATEWAY` are injectable local
test endpoints; normal installations leave them unset.

## Pair each person locally

```sh
ku discord pair --person alice --user DISCORD_USER_ID
ku discord pair --person bob --user BOBS_DISCORD_USER_ID
```

Kurisu prints a one-use code that expires after five minutes. The person sends
`/start CODE` in the bot's DM. A wrong, expired or reused code cannot bind an
identity. `--user` binds the code to that exact Discord user ID; without it,
possession of the short-lived code authorizes redemption. Unpaired users,
bots, webhooks and guild messages never reach an agent session.

Code creation is local: it works before the bot has joined any guild and never
tries to create a Discord DM. Install the bot first, then the person opens its DM
and redeems the code. If installation takes more than five minutes, mint a new
code afterward. With zero mutual guilds, a human invite alone cannot establish
the bot-DM path.

A new person gets a normal Kurisu peer session. To select an existing session,
or link the current Telegram owner to Discord without copying history:

```sh
ku discord pair --person alice --session EXISTING_SESSION_ID --user DISCORD_USER_ID
ku discord pair --person owner --session TELEGRAM_SELECTED_SESSION_ID --user OWNER_DISCORD_USER_ID
```

Existing people must pair with their current active session. `/new` and `/use`
then change that person's binding across linked identities. Legacy Telegram
owner and chat/topic selections are imported without rewriting `telegram.json`
or the legacy Telegram state file. Local pairing is an administrator operation;
agents cannot mint pairing codes through their MCP caller identity.

## Commands and files

Global application commands are registered for `BOT_DM`, including `/start`,
`/help`, `/new`, `/use`, `/id`, `/list`, `/status`, `/model`, `/usage`, `/stop`
and `/send_now`. Text commands use the same dispatcher. `/model` opens the
harness/model/effort choices; `/model MODEL_ID` changes the current harness's
model. Harness changes use Kurisu's existing history-linked replacement.
`/list PAGE` uses zero-based pages of five sessions.

Send text or attach files in the DM. Downloads are size checked (25 MiB maximum
per attachment), use sanitized basenames, and reach the existing workspace
upload path. The agent's `send_file` tool uses the person/channel that originated
the session's request, following parent sessions for delegated work. It reports
success only after Discord returns an accepted message ID. Current outbound DM
attachments have a conservative 10 MiB limit; larger files fail explicitly.

`ku discord notify "message"` sends a notice to the paired Discord people and
fails when there are none or when delivery fails. It is a local-user operation.

Normal text has Discord's 2000-character limit. The shared scanner never cuts a
URL, inline code span or fenced code block. An indivisible token above the limit
is delivered as its exact UTF-8 text attachment. Markdown renders natively on
Discord; Telegram's payloads remain raw. Streaming edits coalesce queued deltas,
separate assistant messages/tool boundaries, retain a short tail in the correct
bubble (#77), and obey per-channel pacing and Discord's 429 Retry-After.

## Recovery and current limits

The Discord inbox and outbox are durable in `<KURISU_HOME>/channels.sqlite3`.
Server message recording and the source-event key are committed together in the
existing session store. Replayed inbound IDs cannot create duplicate user rows
or start another turn in the same daemon. Posted messages recover through the
existing startup/queued-turn machinery. Accepted outbound message IDs are kept
per destination; edits replay idempotently. A create-message whose receipt is
lost remains **uncertain** and is not automatically re-created. Blocked DMs and
failed sends return errors, remain recorded as failures, and produce explicit
delivery-error logs/fallback attempts.

Phase 2A reconnects with a fresh gateway Identify. **Gateway resume and history
backfill are not included**: messages sent while disconnected may be missed.
No exactly-once guarantee is possible for arbitrary platform outages or
uncertain network acceptance. Interaction acknowledgements are immediate;
persistent bot messages carry the conversation and model choices.

Approvals/questions, `/mode`, `/mcp`, `/schedule`, `/catch_up`, `/compact`,
`/terminal`, `/browser`, `/web`, STT/voice handling, guild mentions/threads and
gateway resume are reserved for a separately approved 2B scope. Audio files in
2A are ordinary attachments. Telegram's three audited reliability gaps remain
unchanged; see [the separate follow-up plans](telegram-reliability-followups.md).

Local tests use a fake WebSocket gateway, fake REST server and existing session
store without a Discord token. They do not prove actual bot authentication,
family-account DM access, invite/command propagation, live Discord limits,
real file playback or any live Discord round-trip. Those require a real token
and an explicitly chosen isolated verification home later.
