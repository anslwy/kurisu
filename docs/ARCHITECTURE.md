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
