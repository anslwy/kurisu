# Terms check for the self-host pivot

Date: 2026-11-06. Scope: Kurisu as software that each user installs and runs on
their own machine, for themselves, with their own accounts. The project hosts
nothing, resells nothing and never sees users' credentials.

This is an engineering reading of public documents, not legal advice. Pages
were read on the date above; neither Anthropic page shows a last-updated date.
Re-check before any public release and after each upstream terms change.

## Summary

| Item | Verdict |
| --- | --- |
| Claude Code driven by Kurisu with the user's own login | **OK with conditions** |
| Omnigent as a dependency (and an installer that fetches it) | **OK with conditions** |
| whatsmeow library | **OK** (MPL-2.0) |
| WhatsApp as a channel | **OK with conditions**: optional, off by default, with a ban-risk warning |
| Telegram bot as a channel | **OK with conditions** |

No item is a blocker for Phase 0.

## 1. Claude Code (headless / orchestrated use)

Sources: [Claude Code legal and compliance][cc-legal],
[Agent SDK overview][sdk], with background in [The Register, 2026-02-20][reg].

What the documents say, quoted:

* Licence split. "Consumer Terms of Service - for Free, Pro, Max, and Ultra
  users" and "Commercial Terms of Service - for Team, Enterprise, and Claude API
  users" (legal page, *License*).
* Credentials. "OAuth authentication is intended exclusively for purchasers of
  Claude Free, Pro, Max, Ultra, Team, and Enterprise" and is "designed to support
  ordinary use of Claude Code and other native Anthropic applications." Also:
  "Anthropic does not permit third-party developers to offer Claude.ai login into
  their own applications … or to route requests through Free, Pro, Max, or Ultra
  plan credentials on behalf of their users", and "developers may not collect,
  store, or intermediate Claude.ai credentials or session tokens". It then says
  this does not "prevent an end user from signing in to the unmodified Claude Code
  binary with their own Claude subscription" (*Authentication and credential use*).
* Products that ship Claude Code. "The Claude Code binary must not be
  modified"; customers "may not remove, disable, or restrict any authentication
  method built into it"; "Customers may not pay for, resell, or intermediate
  Claude usage on their end users' behalf"; "Each end user must authenticate with
  their own Anthropic API key, Claude subscription plan credentials" (*Can
  customers offer Claude Code in their products?*).
* Usage limits. "Advertised usage limits for Pro, Max, and Ultra plans assume
  ordinary, individual usage of Claude Code and the Agent SDK" (*Acceptable use*).
* Agent SDK. "Unless previously approved, Anthropic does not allow third party
  developers to offer claude.ai login or rate limits for their products,
  including agents built on the Claude Agent SDK." The SDK is governed by the
  Commercial Terms. For other languages the docs say to "run the CLI as a
  subprocess with the `-p` flag and `--output-format json`."
* Branding. "Claude Code" or "Claude Code Agent" may not be used in a product name.

**Verdict: OK with conditions.** The self-host model matches the explicit
allowance: the end user signs in to the unmodified binary with their own
subscription or API key. Conditions Kurisu must meet, enforced in code where
possible:

1. Drive only the user's installed, unmodified `claude` CLI (or Omnigent's
   harness, which drives it). Never bundle, patch or redistribute the binary.
2. Never implement a claude.ai login flow and never read, copy, store, proxy or
   transmit Claude OAuth or session tokens. The user runs `claude login`
   themselves, and `kurisu init` only checks that the CLI reports logged in.
3. Do not build Kurisu on the Agent SDK with subscription OAuth. If an SDK path
   is ever added, it must use the user's own API key (Commercial Terms).
4. Never run one user's credential for another person. The frozen
   guest-hosting design (one operator account serving guests) is **not**
   compatible with subscription credentials and must stay frozen unless each
   guest brings their own key.
5. Usage. A 24/7 lead plus several staff on a Pro/Max plan may exceed "ordinary,
   individual usage". Provide budgets and caps, default to conservative
   concurrency, and document that heavy or always-on use should use an API key
   under the Commercial Terms.
6. Name the product "Kurisu". Don't use "Claude Code" in its name; "works with
   Claude Code" in descriptions is fine.

Open point: no page directly addresses cron, CI or multi-agent orchestration with
subscription credentials. Condition 5 is our conservative reading, not a quoted
rule.

## 2. Omnigent

Source: the installed package metadata, `omnigent-0.20.0.dist-info` (`Author:
Databricks, Inc.`, `License :: OSI Approved :: Apache Software License`, with
`LICENSE` = Apache License 2.0 and a `NOTICE` file).

**Verdict: OK with conditions.** Apache-2.0 allows depending on it, fetching it
in an installer and redistributing it. Conditions:

1. Preferred: the installer fetches Omnigent from its official distribution
   channel at a pinned, tested version range, so Kurisu does not redistribute it.
2. If Kurisu ever vendors or bundles Omnigent (for example, in a container
   image), include its LICENSE and NOTICE and mark any modified files
   (Apache-2.0 §4).
3. Don't use Databricks or Omnigent trademarks in a way that implies
   endorsement (Apache-2.0 §6).
4. Check the official distribution channel and URL before Phase 1. This memo
   relied on installed metadata, not on a registry page.

## 3. whatsmeow and WhatsApp

Sources: [whatsmeow repository][wm] and [LICENSE][wm-lic] (Mozilla Public
License 2.0); [WhatsApp Terms of Service][wa-tos] (effective January 4, 2021,
US/Canada page; the EEA/UK terms are separate and were not read).

* whatsmeow licence. MPL-2.0 is file-level copyleft. Linking it is fine.
  Modified whatsmeow files must stay MPL-2.0 and source-available. **OK.**
* WhatsApp terms. Under *Acceptable Use* → *Harm To WhatsApp Or Our Users*, users
  may not "directly, indirectly, through automated or other means, access, use,
  copy, adapt" the services in prohibited ways, including "(a) reverse engineer,
  alter, modify, create derivative works from, decompile, or extract code from our
  Services". *Legal And Acceptable Use* bars "bulk messaging, auto-messaging,
  auto-dialing". WhatsApp "may take action with respect to your account,
  including disabling or suspending your account". The terms don't name
  unofficial clients, but whatsmeow is an unofficial, reverse-engineered client
  of the multi-device web API, and bot replies are automated messaging.

**Verdict for the WhatsApp channel: OK with conditions.** It may ship only as:

1. an optional plugin, off by default and never part of the default install;
2. with a plain warning in `init` and the docs that it uses an unofficial client,
   may break WhatsApp's terms and can get the account banned. Recommend a
   secondary number;
3. limited to the owner's own one-to-one chats. No bulk messaging, no
   contacting third parties, no group automation;
4. kept free of any Meta/WhatsApp branding that implies endorsement.

The official WhatsApp Business Platform is the compliant alternative. It does
not fit "personal self-host" well and is out of scope for Phase 0.

## 4. Telegram

Source: [Telegram Bot Platform Developer Terms of Service][tg].

The self-hosting user creates their own bot through BotFather, so the user is
the "developer" under these terms. Relevant points:
* developers act as independent third parties;
* an accessible privacy policy is required, or Telegram's standard bot policy
  applies by default;
* data collection beyond what the service needs is restricted, and using data to
  build datasets or train AI models is prohibited;
* user data must be encrypted at rest;
* violations can end the bot or the account.

**Verdict: OK with conditions.**

1. The bot only answers the owner's chat ID, which also keeps everyone else's
   data out.
2. Store Telegram content only as needed (session history, attachments) and
   document where it lives. Encrypted backups already exist, but local stores at
   rest are a Phase 1 item.
3. Don't use chat data for training, and don't scrape groups or channels.
4. Ship a short default privacy note the user can adopt for their bot.

## Re-check triggers

* Any change to the Anthropic legal page or Agent SDK terms.
* Adding a hosted, shared or multi-user mode (condition 1.4).
* Bundling Omnigent or a Claude CLI in an image (conditions 1.1 and 2.2).
* Enabling WhatsApp by default (never).

[cc-legal]: https://code.claude.com/docs/en/legal-and-compliance
[sdk]: https://code.claude.com/docs/en/agent-sdk/overview
[reg]: https://www.theregister.com/software/2026/02/20/anthropic-clarifies-ban-on-third-party-tool-access-to-claude/5014546
[wm]: https://github.com/tulir/whatsmeow
[wm-lic]: https://github.com/tulir/whatsmeow/blob/main/LICENSE
[wa-tos]: https://www.whatsapp.com/legal/terms-of-service
[tg]: https://telegram.org/tos/bot-developers
