# Kurisu - pilot guest WhatsApp bridge

Kurisu is a single-guest WhatsApp assistant: a friend messages a **secondary**
WhatsApp account, signs in with Google (`/signin`), and then talks to a
sandboxed Pi agent with a **fixed DeepSeek** model. The friend never gets an
Omnigent credential, never picks a model, and never sees a session id.

```
WhatsApp (secondary account)
        │  whatsmeow (Go, this repo: transport/whatsmeow-gateway)
        ▼  NDJSON over a 0600 Unix socket
Broker (Python, this repo: broker/)
   ├── allowlist + Google device-code sign-in gate (default deny)
   ├── server-enforced user_id -> ONE session id (registry)
   ├── fixed model, fixed tools, bounded message sizes
   └── sandbox: rootless Podman (preferred) or systemd scope + bubblewrap
        ▼
Guest workspace  <guests>/pilot-001/workspace   (the only writable path)
```

## Quick start (no WhatsApp, no Google, no model call)

```bash
cd broker
uv venv .venv && uv pip install -e '.[dev]'

# 1. prove the sandbox limits are real on this machine
.venv/bin/python -m guest_broker.cli selftest --config ../config/broker.local.yaml

# 2. prove the /signin gate + persistence (fake Google flow, echo backend)
printf '/signin\nhello\n/signin\nhello kurisu\n' \
  | .venv/bin/python -m guest_broker.cli dev --config ../config/broker.dev.yaml --fake-signin
```

Second run of the same command prints `already signed in`, which is the
persistence check for the broker's own state.

## Tests

```bash
cd broker && .venv/bin/python -m pytest -q          # 106 tests
cd transport/whatsmeow-gateway && go test ./...     # policy/protocol/gateway
```

The sandbox tests are not mocks: they start transient scopes and read back what
the kernel did (OOM kill at `MemoryMax`, fork blocked at `TasksMax`, SIGKILL at
`RuntimeMaxSec`, read-only root, single writable workspace).

## Documents

| File | Contents |
| --- | --- |
| `docs/POC-RUNBOOK.md` | exact operator steps: Google Cloud/"Kurisu" client, QR pairing, friend `/signin`, persistence check |
| `docs/PERSISTENCE.md` | every persistent directory, what lives there, and the restart test |
| `docs/SECURITY.md` | threat model, enforcement table, what the guest can never reach |
| `docs/RUNTIME-DECISION.md` | Podman vs systemd-scope+bwrap, and why DinD is deferred |
| `docs/GUEST-CONTAINERS.md` | guest-initiated container use: prohibited options and the only acceptable path |
| `docs/REMAINING-STEPS.md` | remaining external blockers, precisely |

## Status in one line

Broker, sign-in gate, transport, sandbox and tests are implemented and exercised
locally; **the only things not done are the credential-dependent steps** (Google
OAuth client, WhatsApp QR pairing, real Omnigent session id), which are
documented step-by-step and intentionally not performed by this workspace.
