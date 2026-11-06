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

1. The broker cannot rely on token-level scoping. It enforces "one session id"
   in code (`OmnigentAPI` rejects any other id) and the operator must run the
   pilot against a dedicated Omnigent identity/project for the guest, so the token
   has nothing else to reach.
2. The admin token stays broker-only. It is never in the guest container, the
   runner container, the transport image, or Git.
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
4. The only way to get a single-session capability today is a **scoped gateway**
   (a sidecar that holds the credential and exposes just "post/read for session
   X" to the broker/runner, with its own allowlist). This PoC does not ship one.
   Until such a gateway exists and is verified, single-session restriction is an
   **external acceptance BLOCKER**, not a property of this code.
5. The runner's own host-API connection is the same story: managed runners get an
   **owner-scoped** bearer (`mint_runner_token`, `OMNIGENT_RUNNER_DELEGATED_AUTH`)
   plus a tunnel binding token. I found no evidence that either is restricted to
   one session, so a guest runner must be treated as able to act across that
   identity's sessions until the operator proves otherwise.

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
* `after` is the last consumed item id, persisted before/after each step, so a
  restart neither replays nor skips answers.
* `pending` is written *before* posting: a crash mid-turn resumes polling instead
  of posting the same message twice.
* A redelivered WhatsApp message id is answered from the persisted reply cache.

See `broker/tests/test_omnigent.py` for the fake-server tests covering contract,
relay, dedup, restart-resume, authorization, failure and timeout.
