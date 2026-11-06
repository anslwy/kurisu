# Kurisu architecture: who runs what

```
WhatsApp (secondary account)
   │
   ▼
┌──────────────────────────────────────────┐
│ whatsmeow-gateway (Go)                    │  transport only
│  · QR pairing, allowlist, default deny    │  no Pi, no Node, no model SDK
│  · strict NDJSON to the broker            │  no model/Omnigent credentials
└──────────────────┬───────────────────────┘
                   │ Unix socket, 0600
                   ▼
┌──────────────────────────────────────────┐
│ Kurisu broker (Python)                    │  identity + policy + relay
│  · allowlist, Google /signin binding      │
│  · registry: one user -> ONE session id   │  holds the operator token
│  · POST /v1/sessions/{id}/events          │  (broker-only, from the env)
│  · GET  /v1/sessions/{id}/items?after=    │  cursor + dedup state
└──────────────────┬───────────────────────┘
                   │ HTTPS/HTTP + Bearer (operator token)
                   ▼
┌──────────────────────────────────────────┐
│ Omnigent host + runner (operator's side)  │  agent execution
│  · Pi, Node, the fixed DeepSeek model     │  MUST be sandboxed per guest
│  · session history, workspace, toolchain  │  (cgroup/container isolation)
└──────────────────────────────────────────┘
```

## Dedicated per-guest instance (pilot architecture, implemented)

The runner credential problem (`docs/RUNNER-AUTH-BLOCKER.md`) is contained by
giving every guest its **own Omnigent server instance**. Any owner bearer the
runner can mint then owns only that guest's session tree.

```
WhatsApp ─▶ whatsmeow-gateway (Go) ─unix─▶ broker ─unix─▶ scoped gateway
                                                       │ http://127.0.0.1:<port>  (fixed per guest,
                                                       ▼  fixed root session id)
              ┌─────────── host ─────────────────────────────────────────────┐
              │ instance server  (bwrap; HOME/CONFIG/DATA under              │
              │   <guests_root>/<user>/instance; SQLite + artifacts + logs;  │
              │   bind 127.0.0.1:<port>; single-user; no --config)           │
              │        ▲ tcp 127.0.0.1:<port>  (only target the relay knows)  │
              │ HostRelay  <user>/instance/io/omnigent.sock (0600)            │
              └────────▲─────────────────────────────────────────────────────┘
                       │ unix socket, mounted read-only at /run/kurisu-io
              ┌────────┴──── guest container (--network=none) ───────────────┐
              │ instance-relay.py: 127.0.0.1:<port> -> /run/kurisu-io/omnigent.sock
              │                    127.0.0.1:11434  -> /run/kurisu-io/model-proxy.sock
              │ omnigent host --server http://127.0.0.1:<port>  (runner + all tools)
              │ mounts: /workspace, /home/guest, ~/Downloads, /opt/toolchains,
              │         /var/lib/kurisu  (all under <guests_root>/<user>, rw)    │
              └──────────────────────────────────────────────────────────────┘
```

**Network path, completely:** the container has no network interface except its
own loopback. Its two egress paths are Unix sockets in a read-only mount. (1)
`omnigent.sock` → `HostRelay` → `127.0.0.1:<this guest's port>`; the target is
fixed when the relay starts and never taken from the client. (2)
`model-proxy.sock` → the broker's allowlisting model proxy, which rewrites
`model` to `ollama/deepseek-v4.1-flash:cloud`. Nothing reaches the main Omni Lead
server (6767), the Telegram bridge, another guest's port or any LAN/Internet host.

| Property | Enforcement | Test |
| --- | --- | --- |
| Separate config/DB/artifacts/logs/history | `InstanceSpec` paths under `<guests_root>/<user>/instance`, `server_plan` sets `OMNIGENT_CONFIG_HOME`/`OMNIGENT_DATA_DIR`/`HOME`, explicit `--database-uri`/`--artifact-location` (dedicated server, never the main `local_server.pid`) | `test_server_plan_is_dedicated_private_and_clean`, `test_two_guests_share_no_port_or_path` |
| Main Omnigent state untouched | provisioning refuses any path inside or containing `~/.omnigent`, `$OMNIGENT_CONFIG_HOME`, `$OMNIGENT_DATA_DIR` | `test_paths_must_not_overlap_main_omnigent_state` |
| Private bind, no admin credential | host must be the literal `127.0.0.1` (single-user default); `0.0.0.0`/LAN/`::` refused; operator env (tokens, `OMNIGENT_SERVER_URL`) never forwarded | `test_only_ipv4_loopback_bind_is_accepted`, `test_listener_check` |
| Unique ports | deterministic hash + probe in `27100-27999`, persisted in `instance.json`; reserved 6767/11434 + `--reserve-port`; another guest's port refused | `test_port_allocation_is_deterministic_and_unique`, `test_provision_refuses_port_owned_by_another_guest` |
| Runner/tools in the guest container only | `runner_plan` + `assert_runner_plan_safe` | `test_runner_plan_*` |
| Broker targets only this instance + root | `runtime.omnigent.instance_user` requires `base_url == http://127.0.0.1:<port from instance.json>`; gateway refuses unless registry id == recorded root; tokenless client only for `http://127.0.0.1` | `test_config_pins_broker_to_the_guest_instance`, `test_unauthenticated_api_only_for_ipv4_loopback_instance` |
| Restart identity | `instance.json` (0600) records the root id; `ensure_root_session` reuses it, refuses to silently create a new one when it is missing (`--recreate` is operator-only) | `test_root_session_created_once_then_reused_across_restarts`, `test_missing_recorded_root_is_refused_not_silently_recreated` |
| Fixed model | `FIXED_MODEL_ID` in state (others rejected on load), `model_override` on root, model proxy rewrite, acceptance `model:*` | `test_state_file_is_validated_on_load`, `test_session_tree_fails_closed` |

Operator commands: `guest-broker instance provision|plan|preflight|serve|runner|root-session|acceptance`
(see `docs/POC-RUNBOOK.md` §2b). `serve` and `runner` are foreground only.

## Responsibility split (the point of this document)

| Concern | Where it lives | Never in |
| --- | --- | --- |
| WhatsApp account, QR pairing, allowlist | `transport/whatsmeow-gateway` | broker, runner |
| Identity/sign-in, session mapping, relay | `broker/` | transport image, guest |
| Operator API token | broker process environment | transport image, guest, Git |
| Model id (fixed DeepSeek) | **the Omnigent session**, set by the operator | WhatsApp messages, broker requests |
| Pi, Node, model SDK, agent tools | **the Omnigent runner** | transport image, broker |
| Session history, workspace, toolchains | Omnigent runner (+ persistent host paths) | transport container |
| Relay cursor / dedup / restart state | broker `runtime/state/relay.json` (0600) | transport, guest |

Consequences that are enforced in code:

* The broker has exactly one backend in the PoC: `omnigent`. It posts to the
  session id from the operator-owned registry and to nothing else
  (`omnigent_api.OmnigentAPI._url` rejects any other id).
* The transport cannot influence session, model, workspace or command: those
  fields are protocol errors (`protocol.FORBIDDEN_FIELDS`).
* The broker never sends a model name: the fixed model is a property of the
  Omnigent session. `runtime.model` is informational metadata for the runbook.
* A local Pi process still exists (`backend: pi-cli`) but is **development only**
  and must be opted into with `runtime.allow_local_pi: true`. It is not the PoC
  path and is documented as a shortcut.

## Hosted orchestration vs container-local tool execution

These are two different things and the PoC must satisfy both:

| | Hosted (Omnigent API) | Container-local (guest's own container) |
| --- | --- | --- |
| Owns | session record, model selection, history, relay surface | shell, file tools, workspace, downloads, binaries, local state |
| Where | operator's Omnigent host | **the guest's own container** |
| Must never | execute the guest's shell/tools on a shared runner | read host/other-user files, mount host sockets, hold the admin token |

So: the broker posts a message to the bound session, and the **session's runner**
must be the guest's own container. Routing the guest's tool execution to a shared
administrative runner is prohibited: that runner sees the operator's files,
credentials and other sessions.

The guest container gets exactly one persistent area (its workspace with
downloads/binaries/state) and nothing else: no host home, no other user's
sessions, no container/DBus/systemd socket, no sudo, no host services, no
network beyond the model endpoint. The broker never passes the admin token into
that container (enforced: `sandbox.extra_env` may not contain
`runtime.omnigent.token_env`, tested).

## Omnigent auth: honest limitations

What the code shows today (read from the installed Omnigent 0.20 package, no
account access, nothing executed):

* Access is **per user + level** (`LEVEL_READ`/`WRITE`/`MANAGE`/`OWNER`); routes
  authorize with `_require_access_and_level(user_id, session_id, ...)`.
* Managed runners can be minted a bearer that acts **as the session owner**
  (`AuthProvider.mint_runner_token`), plus a runner *tunnel binding* token. That
  is **owner-scoped, not session-scoped**.
* I found **no documented token that is scoped to exactly one session**. A token
  that can post/list one session generally can reach the owner's other sessions.

Consequences, stated plainly:

1. In legacy `direct` mode the broker holds the owner token and enforces one
   session id in code only; that does not satisfy the guest authorization
   boundary. In `scoped_gateway` mode the broker instead receives only the local
   gateway secret. The gateway owns the operator token and exposes just the root.
2. The operator token stays gateway-only in scoped mode. It is never intentionally
   placed in the broker, guest container, runner container, transport image, or
   Git. Legacy direct mode is not accepted for the isolated guest deployment.
3. A **separate Omnigent identity** is a per-*user* boundary, not a per-session
   one. Concretely, a dedicated guest identity:
     * DOES isolate: the guest's sessions/history/agents from the operator's
       personal sessions, and from any other user's;
     * DOES NOT isolate: session-to-session within that identity. Whoever holds
       that identity's token (or its runner's owner-scoped bearer) can still
       **list, create and control every session owned by that identity** - there
       is no documented one-session scope.
   So a dedicated identity is necessary but **not sufficient** for
   "cannot enumerate/control others".
4. The scoped gateway below supplies that broker-side capability. Its fake-upstream
   and real Unix-socket tests cover the root boundary, but live runner acceptance
   remains blocked by the separate credential issue in item 5.
5. The runner's own host-API connection is not solved by broker-side scoping.
   Managed runners can use their tunnel binding token to mint an **owner-scoped**
   bearer (`mint_runner_token`, `OMNIGENT_RUNNER_DELEGATED_AUTH`). Environment
   scrubbing does not establish a process/UID boundary inside the container.
   See `docs/RUNNER-AUTH-BLOCKER.md` for the exact installed-source evidence. On a
   shared server this is a blocker. With the dedicated per-guest instance above,
   the owner owns only the guest's tree, so the issue is contained. The live
   checks listed there remain.

## Runner isolation requirement (not optional)

Calling a *shared administrative* runner for guest turns is **not acceptable**:
that runner sees the operator's files, credentials and other sessions. The
provisioned guest session must be executed by a runner that is isolated per
guest, with the same guarantees the broker used to enforce locally:

* its own cgroup/container (memory, CPU, PID, runtime, IO limits);
* only the guest workspace writable; no host home, no container socket, no
  DBus/systemd socket, no operator credentials;
* no network path except the model endpoint;
* one runner (or one sandbox) per guest session, torn down or reset between
  turns according to the operator's policy.

The broker cannot verify this from the outside, so it is an **operator
requirement documented here and in `docs/SECURITY.md`**, not something the broker
can silently assume. `docs/REMAINING-STEPS.md` lists the exact provisioning step.

## Relay contract (broker <-> Omnigent)

| Call | Purpose |
| --- | --- |
| `POST /v1/sessions/{id}/events` | post one user turn (`type: message`, `input_text`) |
| `GET /v1/sessions/{id}/items?after=<cursor>&order=asc` | read new conversation items |
| `GET /v1/sessions/{id}` | optional probe: session exists, runner online |

* Auth: `Authorization: Bearer <operator token>`; 401/403 -> typed error.
* The assistant's text is taken only from items whose role is `assistant`/`agent`;
  unknown shapes yield no text (never a guess).
* **baseline + gating.** Before posting, the broker snapshots the newest item id
  into `pending.baseline_cursor`. Only items after it are considered, and an
  assistant item is accepted only once a *new user item* (our turn) has been
  observed - or a documented post/item correlation id, when the host provides
  one. Without one, an unmatched correlation id means "uncertain", never
  "fall back to the newest assistant text".
* **one atomic commit.** Answer selection writes nothing. Cursor, seen ids, the
  reply cache and clearing `pending` are committed in a single state update, so a
  crash before that write leaves `pending` intact and the restart re-selects the
  same item. A crash after the write is a clean "answered" state.
* **at-most-once, not exactly-once.** `pending` is tied to one inbound message
  id. The same id may resume; a different id gets an explicit pending/busy
  outcome. A definitive rejection (400/401/403/404) clears the marker; a timeout,
  network error, 5xx, 409 or 429 keeps it in "possibly posted" state and is
  **never auto-reposted**. A dedicated single-writer session plus operator
  reconciliation for an unresolved turn is the documented PoC invariant - the
  protocol cannot promise exactly-once.
* **namespace.** Relay dedup keys are `sender_jid|msg_id`, so two senders can
  never collide in the per-session cache.

See `broker/tests/test_omnigent.py` for the fake-server tests covering contract,
relay, dedup, restart-resume, authorization, failure and timeout.

## Sub-sessions / delegation

**With a dedicated instance (`instance_user` set): allowed, contained.** The
guest's agent creates children on its own instance. Installed-source evidence:
`server/routes/_sessions/orchestration.py` ("Inherit runner affinity from the
parent session") and `_sessions/helpers.py::_authorize_bundled_parent_and_inherit_runner` give a child
the parent's `runner_id`, which is the container runner. Because the instance DB
holds nothing else, a child cannot be parented outside the guest tree.
`allow_sub_sessions: true` is accepted only together with `instance_user`.
Broker-created children through the scoped gateway stay **disabled** (501). The
precise acceptance check is `guest-broker instance acceptance`. It lists every
session in the instance (`kind=any`, archived included) and fails unless each
one descends from the recorded root (`tree:*`), reports the root's `runner_id`
(`runner:*`, a missing field fails) and uses the fixed model (`model:*`). Until
an operator has seen it pass live, set `allow_sub_sessions: false`.

### Shared-server mode (legacy, DISABLED)

WhatsApp Kurisu may eventually need child sessions for long tasks. Intended path:

* children are created **only** as descendants of the guest's root session;
* they execute in the **same guest container** with the same fixed model and the
  same workspace restrictions;
* resource, count and depth quotas apply per root;
* the guest may inspect/control **only its own descendants**, never unrelated
  sessions.

This is **disabled** in the PoC (`runtime.omnigent.allow_sub_sessions: true` is
rejected at config load). Current blockers, stated honestly:

1. Direct mode holds an owner-scoped token, so global creation is not an
   acceptable guest capability. The scoped gateway has no live child creator.
2. Server-enforced parent/root checks for child creation are not exposed as a
   scoped capability in the installed Omnigent version (no verified
   `parent_session_id` authorization gate for an owner token).
3. The gateway's fake parent/depth/count map is not persistent or server-verified,
   and configured runner affinity is not verified against the root. These are
   deliberately non-live test hooks, not evidence that delegation is safe.

Acceptance criteria before enabling: scoped gateway issues child creation for the
root only; server rejects a child whose parent is not the guest root; quotas are
enforced server-side; enumeration is filtered to descendants; tests cover
attempts to create a sibling/root/unrelated child and to read a non-descendant.

## Scoped access layer (implemented; live acceptance pending)

**Choice: scoped gateway, not a second identity.** A second Omnigent identity
isolates *users* but still lets that identity list/create/control every session it
owns, and it would change the live account setup. The gateway is smaller and
safer: it is the only holder of the operator token, exposes one root session plus
its own descendants, and needs no server/account changes.

Modes (`runtime.omnigent.mode`):

| Mode | Who holds the token | Broker can reach |
| --- | --- | --- |
| `direct` (default) | broker process | any session that token owns (pinned in code only) |
| `scoped_gateway` | gateway process only | the guest root via `ScopedClient`, which refuses any other id |

Gateway surface (Unix socket 0600 + shared secret from `gateway_secret_env`):

```
GET  /v1/guest/session
POST /v1/guest/message     {text, msg_id}
GET  /v1/guest/items?after=&limit=&order=asc|desc
GET  /v1/guest/children
POST /v1/guest/children    {title}          # 501 until children are enabled
```

Enforced there, not by prompts: a fixed root id (a `session_id` in the body is a
400), no global list/create/delete/agent routes (404), and test-only
parent/depth/count policy. Configured runner affinity is forwarded only to an
injected fake creator; it is **not** verified against the root and no live child
creation exists.

Children stay **disabled**: `allow_sub_sessions: true` is rejected at config
load, the shipped gateway always answers 501, and no creator is wired to the host.
The injectable creator exists only to test the policy logic; there is no supported
operator switch that enables live creation.

Run the foreground gateway in one operator terminal (only this process gets the
Omnigent operator token), then run the broker separately with only the shared
secret and its Google sign-in secret:

```bash
# terminal 1: scoped gateway
export OMNIGENT_OPERATOR_TOKEN='<gateway-only token>'
export KURISU_GW_SECRET='<local random shared secret>'
.venv/bin/python -m guest_broker.cli gateway --config config/broker.local.yaml

# terminal 2: broker, deliberately without OMNIGENT_OPERATOR_TOKEN
export KURISU_GW_SECRET='<same local shared secret>'
.venv/bin/python -m guest_broker.cli preflight --config config/broker.local.yaml --probe-api
.venv/bin/python -m guest_broker.cli run --config config/broker.local.yaml
```

The gateway command loads only config + registry, not Google/sign-in state or a
local agent backend. Both commands stay in the foreground and install nothing.

Acceptance calls (operator; run only after the dedicated runner blocker is
resolved):

```bash
S=<state>/gateway.sock; T=$(printf 'Bearer %s' "$KURISU_GW_SECRET")
curl -sS --unix-socket "$S" -H "Authorization: $T" http://x/v1/guest/session
curl -sS --unix-socket "$S" -H "Authorization: $T" 'http://x/v1/guest/items?limit=5'
curl -sS --unix-socket "$S" -H "Authorization: $T" \
     -H 'Content-Type: application/json' -d '{"text":"ping","msg_id":"acc1"}' \
     http://x/v1/guest/message
curl -sS --unix-socket "$S" -H "Authorization: $T" \
     -H 'Content-Type: application/json' -d '{"session_id":"conv_other"}' \
     http://x/v1/guest/message           # expect 400, no upstream call
```

Operator-verified, not code-verified: the runner really is the guest's own
container, and the token exists nowhere else on the host.
