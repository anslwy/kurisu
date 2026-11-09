# Remote MCP OAuth 2.1 Design (`connect <service>`)

Target UX: (1) user asks to connect a service; (2) agent discovers the provider's remote MCP URL via web search; (3) Kurisu starts OAuth; (4) Kurisu posts the authorize URL into chat (Telegram/Discord/web); (5) user authorizes on their phone; (6) the server appears in `/mcp` to inspect, disable, and re-enable.

## A. Existing Pieces (Verified `file:line`) & Smallest Gap for Step 6

- **Store**: `session_mcp` (`crates/kurisu/src/store.rs:174-181`, harness-switch copy at `451-452`) stores `{command,args?,env?}` or `{type,url,headers?}` + `enabled`. CRUD lives in `mcp_list` (`1177-1183`), `mcp_set` (`1186-1206`), `mcp_disable_all` (`1209-1211`), `mcp_switched_off` (`1215-1219`), `mcp_remove` (`1221-1223`).
- **Server & Harnesses**: `validate_mcp` (`crates/kurisu/src/server.rs:440-469`) already accepts `http`/`sse` with `headers`; `redact_mcp` (`472-482`) masks `env`/`headers` as `"<set>"`; `opencode_session` (`812-831`) and `OpenCodeDriver::session` (`crates/kurisu/src/acp.rs:323-324`) pass `mcpServers` to ACP; `claude_opts` (`crates/kurisu/src/server.rs:978-985`) writes `<session_dir>/mcp.json` (`0600`); `codex_extra` (`1011-1030`) only wires `kurisu-mcp` and `lb`.
- **CLI, Telegram, Web**: `ku mcp [add|enable|disable|rm]` (`crates/kurisu/src/main.rs:508-539`) lacks `--header` and `--client-id`. Telegram `/mcp` (`crates/kurisu/src/telegram/bridge.rs:2319-2366, 2467-2474`, `crates/kurisu/src/telegram/backend.rs:360-448`) renders owner-only Enable/Disable cards. Web routes `/api/v1/sessions/{id}/mcp` (`crates/kurisu/src/web.rs:245-256, 580-581`) exist, but `crates/kurisu/web/src/` has no MCP UI.
- **Smallest Gap**: (1) `server.rs:3041-3043` blocks agent callers from adding/connecting an MCP server (`mcp_connect` tool/op needed for step 3); (2) `mcp_list` / `/mcp` only track `enabled`+`loaded`, lacking OAuth state/expiry and `Authorize`/`Re-authorize`/`Remove` actions; (3) runtime injection of `Authorization: Bearer <token>` into `claude_opts`, `opencode_session`, and `codex_extra` without storing secrets in SQLite.

## B. Auth Design (MCP OAuth 2.1 — Decided)

1. **Probe & Protected-Resource Discovery (RFC 9728)**: On `mcp_connect` / `ku mcp add --url`, Kurisu probes `<url>`. HTTP 2xx sets auth state `none`. HTTP `401` parses `WWW-Authenticate` for `resource_metadata="<url>"`, else fetches `<origin>/.well-known/oauth-protected-resource<path>` then `<origin>/.well-known/oauth-protected-resource`.
2. **AS Metadata (RFC 8414)**: From `authorization_servers[0]` (or `<mcp_origin>` if protected-resource metadata 404s per MCP OAuth 2.1 fallback), fetch `<issuer>/.well-known/oauth-authorization-server<path>`, `<issuer>/.well-known/oauth-authorization-server`, then `<issuer>/.well-known/openid-configuration`. Require `S256` in `code_challenge_methods_supported` when present.
3. **Client Registration (RFC 7591 DCR)**: If `registration_endpoint` is present, POST `{"client_name":"Kurisu","redirect_uris":[...],"grant_types":["authorization_code","refresh_token"],"response_types":["code"],"token_endpoint_auth_method":"none"}` and persist `client_id` (+ optional `client_secret`). If DCR is absent, use `--client-id` (`--client-secret`); if neither is available, enter `failed("AS lacks dynamic registration; pass --client-id")`.
4. **PKCE S256 & CSRF State**: Generate 32 CSPRNG bytes via two `uuid::Uuid::new_v4()` -> 43-char `base64::engine::general_purpose::URL_SAFE_NO_PAD` `code_verifier`; `code_challenge = URL_SAFE_NO_PAD(sha256(verifier))` (`code_challenge_method=S256`); 128-bit random `state` (10 min TTL); include `resource=<mcp_url>` (RFC 8707).
5. **Token Refresh & 401 Retry**: Before session launch (`now + 60s >= expires_at`) or when a pre-turn probe to `<mcp_url>` returns `401`, POST `grant_type=refresh_token` to `token_endpoint`, persist rotated tokens atomically, update `toolset_fingerprint` (`server.rs:871-882`) so the harness restarts with the new header, and retry the probe **once**. If refresh returns `400`/`401` or the single retry still returns `401`, transition to `needs-auth` and notify chat without crashing the turn.
6. **Dependencies**: **Zero new crates in `Cargo.toml`**. Reuse `reqwest` (with `reqwest::Url` for URL/query encoding), `base64`, `uuid`, `serde`/`serde_json`, `axum`, `tokio`, `chrono`, `libc`, plus a 50-line FIPS 180-4 `sha256(&[u8]) -> [u8; 32]` in `crates/kurisu/src/oauth.rs`.

## C. Phone Callback Reachability (Comparison & Decision)

| Option | Pros | Cons |
| --- | --- | --- |
| **(i) RFC 8628 Device Code** | Zero callback/redirect reachability needed; phone opens `verification_uri_complete` while Kurisu polls. | Rarely offered by MCP OAuth 2.1 servers (Notion, Linear, Sentry mandate Auth Code + PKCE). |
| **(ii) `/web` Listener over Tailscale** (`remote.rs:38, 189-240`) | One-tap browser redirect on phone when tailnet + HTTPS MagicDNS (`public_url`) is active. | Rejected by strict ASes if plain `http://100.x.y.z` is used; fails when phone is off-tailnet or `/web` is off. |
| **(iii) Loopback + Chat Paste-back** (`http://127.0.0.1:<port>/oauth/callback`) | Universally accepted by RFC 8252 / OAuth 2.1 DCR without TLS; works from any phone network and auto-completes on local desktop. | On phone, browser lands on unreachable `127.0.0.1` and user copies the address-bar URL back into chat. |

- **Decision & Fallback Order**: Pick **(iii) Loopback + Chat Paste-back** as the universal baseline, with automatic upgrade in order **(i) -> (ii) -> (iii)**:
  1. **(i) Device Code** if AS metadata advertises `device_authorization_endpoint`.
  2. **(ii) Tailscale `/web` callback** (`<public_url>/api/v1/oauth/callback`) when `/web` is enabled with an `https://` `public_url`.
  3. **(iii) Loopback + Paste-back**: register `http://127.0.0.1:<port>/oauth/callback` (listening locally) and post the authorize link to chat with instructions to paste the final `http://127.0.0.1.../oauth/callback?code=...&state=...` redirect URL back into Telegram/Discord/Web chat, where Kurisu intercepts it before message storage/LLM dispatch and exchanges the code.
- **When Provider Supports None** (no DCR + no `--client-id`, or rejects loopback redirect URI): set state `failed(reason)` and surface the exact `--client-id` or `--header` command in chat and `/mcp`.

## C2. P2b (Delivered) — Paste-back UX

Owner decision (2026-11-09): no tailnet, no `web.json` `public_url`, no web UI. The listener stays
loopback and login must work from any phone on any network, exactly like logging into `agy` remotely.

- **Redirect URI**: always `http://127.0.0.1:<web port>/api/v1/oauth/callback` (an explicit
  `redirect_uri` request field still overrides). It is never derived from `/web` config.
- **Chat-ready text**: `mcp_connect` returns `{authorize_url, message, state, client_id}`; `message`
  is relayed as-is — the authorize link plus "your browser will then land on a page that won't load;
  copy the whole address bar and send it back here".
- **Agent-callable callback**: `mcp_callback` accepts an authenticated agent caller (its `caller`
  session id) when the pending flow belongs to that session or to a session it created. The loopback
  web route stays caller-less and host-trusted. Any other caller gets `permission denied`.
- **Paste input**: `url` (also `redirect_url`, `text`, `paste`) takes the whole copied address bar, a
  bare query string, or bare `code=…&state=…`; percent-decoding happens once in the `url` query
  parser. An `error=` paste returns `authorization was not granted: <error> (<description>)`.
- **No provider-specific code**: no hostnames, no per-server branches; everything comes from discovery.
- **Single use**: success and expiry delete `pending-<server>.json`; a malformed paste and a refused
  caller leave it untouched, so a legitimate paste can still follow. Replaying a consumed state
  finds no pending flow.
- **Tests**: `mcp_oauth_pasteback_by_agent_session` (chat-ready text, loopback redirect, malformed
  paste, foreign caller refused, successful paste exchange, single-use replay) and the
  `callback_paste_*` unit tests; the existing mismatch/expired test and the web route test stay green.

## C3. P3 — Runtime Credentials

- Before each turn, refresh tokens that expire within 60 seconds. Otherwise probe the MCP URL
  with its current bearer token; a `401` triggers one refresh grant and one probe retry.
- Refresh requests are form-encoded and include `client_id`, `resource`, and the refresh token.
  Persist rotation atomically before retrying; keep the previous refresh token when omitted.
- Token files are `0600` under a `0700` directory. Enabled credentials are copied privately on
  harness switch. Disable/remove takes precedence over an in-flight refresh.
- Claude and ACP (OpenCode/agy) receive runtime `headers`; Codex receives `http_headers`.
  Static Authorization headers take precedence. Only the resulting fingerprint is stored in
  SQLite; a new bearer token invalidates the running harness's toolset.
- Rejected refresh credentials (`400`/`401`) or another `401` on retry clear credentials to a
  private `needs-auth` marker. Omit that server from launch and include a reconnect notice in
  the turn's reply. Transient refresh failures retain credentials for the next turn and omit
  the server for the current turn. Probes read only response status, including streaming MCPs.
- Acceptance: `cargo test -p kurisu --bin ku server::oauth_tests::` exercises real loopback
  MCP/AS endpoints, rotation/retry limits, all harness payloads, permissions, redaction,
  cleanup, and concurrent disable/remove without contacting an external provider.

## D. Token Storage & Redaction

- **Location & Permissions**: `$KURISU_HOME/sessions/<session_id>/mcp-oauth/<server>.json`, written via `crate::telegram::write_private` (`0600` file, `0700` parent dir). `session_mcp.config` in SQLite keeps only `{type, url, headers?}` without OAuth secrets.
- **Contents**: `{issuer, token_endpoint, client_id, client_secret?, access_token, refresh_token?, expires_at?, scope?, updated_at}`.
- **Lifecycle & Redaction**: Survives daemon restarts; copied on session harness switch (`store.rs:451`); unlinked by `ku mcp rm` / `mcp_remove`. Never logged (`Debug` prints `[REDACTED]`); paste-back messages carrying `?code=` are consumed without logging or storing in `messages`; `redact_mcp` (`server.rs:472`) and `mcp_list` expose only `{state, expires_at, error}` and `"<set>"`.

## E. State Machine & Telegram `/mcp` Card Actions

- **States**:
  - `none`: stdio server or remote server returning 2xx without auth.
  - `needs-auth`: remote server returned `401` on probe or refresh failed (`invalid_grant`).
  - `authorizing`: active PKCE/device flow with `{authorize_url, expires_at}` (10 min TTL).
  - `authorized`: valid `access_token` present (`expires in Nh`, or `no expiry`).
  - `expired`: `now >= expires_at` prior to refresh completion.
  - `failed(reason)`: discovery, DCR, state verification, or token exchange error.
- **Transitions**: `add/connect` -> `none` (2xx) | `needs-auth` (401) -> `authorizing` (on Authorize / `mcp_connect`) -> `authorized` (valid callback/paste-back) | `failed(reason)` (bad state/timeout/AS error); `authorized` -> `expired` (TTL elapsed) -> `authorized` (refresh 200) | `needs-auth` (refresh 400/401).
- **Telegram Card Actions & Owner-Only Rule**:
  - Non-owner chats see read-only `/mcp` status lines (with `authorize_url` omitted).
  - Only the owner DM (`is_owner_dm(&ctx.chat_id)`, `telegram/bridge.rs:2334, 2467`) sees `authorize_url` and inline buttons: `🔑 Authorize <name>` (`needs-auth`/`expired`/`failed`), `🔄 Re-authorize <name>` (`authorizing`/`authorized`), `⛔ Disable <name>` / `✅ Enable <name>`, and `🗑 Remove <name>`.

## F. Test Suite (Unit + In-Process Fake AS, Zero Network)

1. `oauth_metadata_discovery_rfc9728_and_fallback`: `WWW-Authenticate` `resource_metadata` header + `.well-known` resolution + 404 origin fallback.
2. `oauth_dcr_absent_requires_client_id`: missing `registration_endpoint` without `--client-id` -> `failed(...)`; succeeds when `--client-id` is set.
3. `oauth_pkce_s256_vector_rfc7636`: verifies inline FIPS 180-4 `sha256` + `URL_SAFE_NO_PAD` against RFC 7636 Appendix B vector.
4. `oauth_callback_rejects_state_mismatch_and_expired_state`: tampered or expired `state` in callback/paste-back is rejected without calling `token_endpoint`.
5. `oauth_refresh_on_401_retries_once_and_rotates_token`: expired/401 token triggers `refresh_token` grant, writes rotated `0600` file, updates fingerprint, retries once.
6. `oauth_refresh_failure_transitions_to_needs_auth_without_crash`: `invalid_grant` on refresh sets `needs-auth` cleanly without failing the turn.
7. `oauth_tokens_and_headers_redacted_everywhere`: verifies `mcp_list`, `ku mcp`, `Debug`, and logs never leak tokens, secrets, codes, or `--header` values.
8. `oauth_e2e_fake_as_in_process`: in-process `axum` MCP+AS (`401` -> discovery -> DCR -> authorize URL -> paste-back -> `0600` token -> `/mcp` `authorized` -> `rm` deletes token).

## G. Phased Implementation Plan

| Phase | Scope | Files | ~LOC | Tests |
| --- | --- | --- | --- | --- |
| **P1** | Store/CLI `--header`, `--client-id`, `AuthState` in `mcp_list`, `codex_extra` MCP wiring | `main.rs`, `store.rs`, `server.rs` | ~110 | CLI flag parsing, `redact_mcp`, `mcp_list` auth state, Codex MCP config |
| **P2** | 401 probe, RFC 9728/8414 discovery, DCR, PKCE S256, callback + paste-back, `mcp_connect` | `oauth.rs` (new), `server.rs`, `web.rs`, `mcp.rs` | ~280 | Tests 1, 2, 3, 4 (discovery, DCR absent, PKCE vector, state mismatch) |
| **P3** | `0600` token store, harness header injection, pre-turn refresh + 401 single retry, `rm` cleanup | `oauth.rs`, `paths.rs`, `server.rs` | ~160 | Tests 5, 6, 7, 8 (refresh on 401, refresh fail -> `needs-auth`, redaction, E2E) |
| **P4** | Telegram `/mcp` cards (`Authorize`/`Re-authorize`/`Remove`), paste-back intercept, Web UI + `ku mcp` | `telegram/backend.rs`, `telegram/bridge.rs`, `main.rs`, `web/src/App.svelte`, `web/src/lib/api.ts` | ~170 | Owner-only `/mcp` card actions, paste-back intercept, `ku mcp` output |
| **P5** | Discord `/mcp` command, owner-only control cards, Discord paste-back intercept | `discord/gateway.rs`, `channels/dispatch.rs`, `channels/dispatch/cards.rs`, `discord/tests.rs` | ~90 | Discord `/mcp` owner vs member, card button dispatch, paste-back exchange |

## H. Explicitly Out of Scope for v1

- **Built-in provider URL directory**: Agent web search (step 2) discovers MCP URLs; a static registry inside Kurisu goes stale.
- **Per-person tokens on shared sessions**: MCP servers and OAuth credentials belong to the session and are managed only by the paired owner.
- **Confidential client auth (`private_key_jwt`, mTLS) & custom scope pickers**: MCP OAuth 2.1 targets public clients with PKCE `S256`.
- **Mid-turn HTTP reverse-proxying of MCP traffic**: Harnesses connect directly to `<url>` with injected `Authorization` headers; Kurisu refreshes pre-turn and relaunches on rotation.
