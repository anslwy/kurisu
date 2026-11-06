# Remaining steps and external blockers

Everything credential-dependent is listed here with the exact command or click
path. None of it was executed by this workspace, by design.

## Done and verified locally (no credentials)

- Broker: allowlist, registry (one session per user), strict protocol, audit,
  per-user rate limit, single in-flight turn, reply truncation.
- Kurisu `/signin`: Google device-code flow, default-deny gate, 1:1 binding of
  `sub <-> wa_jid <-> pilot-001`, expiring/single-use pending states, tokens kept
  in broker-only `0600` state.
- Sandbox: transient cgroup v2 scope + bubblewrap with verified kernel
  enforcement (memory OOM, CPU quota, `TasksMax`, `RuntimeMaxSec`, `RLIMIT_FSIZE`,
  read-only root, one writable workspace, isolated net namespace).
- Podman sandbox plan (hardened argv) + capability probe + container policy, all
  argv/unit tested. Not executed here: no Podman inside this container.
- whatsmeow gateway: QR pairing command (never run), secondary-account
  confirmation, allowlist enforcement, strict protocol, broker handshake with
  account pinning; Go tests pass; binary builds (24 MB).
- Persistence of broker state across restarts, demonstrated with the fake flow.

## Blocker 1 - Google OAuth client ("Kurisu")

**Needs:** Google Cloud access with permission to create a project.

1. Create project **Kurisu**.
2. OAuth consent screen: **External**, app name **Kurisu**, Publishing status
   **Testing**, add the friend's Google address under **Test users**.
3. Credentials -> Create credentials -> OAuth client ID -> application type
   **TVs and Limited Input devices** (this is the device flow; **no redirect URI
   is required**, so no public callback server).
4. Put the client id in `config/broker.local.yaml` (`auth.google_client_id`) and
   the secret in the broker environment as `KURISU_GOOGLE_CLIENT_SECRET`.
5. `guest-broker preflight` must show the secret as `set`.

Without this, `auth.enabled` cannot be turned on for a real run. The flow logic
itself is tested against a fake transport (`broker/tests/test_auth.py`).

## Blocker 2 - secondary WhatsApp account pairing

**Needs:** physical phone holding the secondary account.

```bash
cd transport/whatsmeow-gateway
./bin/whatsmeow-gateway pair --config ../../config/gateway.local.json --confirm-secondary
# scan the terminal QR: WhatsApp -> Settings -> Linked devices
./bin/whatsmeow-gateway run  --config ../../config/gateway.local.json
```

Do not use the primary account. The broker pins the first account it sees and
refuses any other, and refuses if the paired account is an allowlisted guest.

## Blocker 3 - provision the guest session + a sandboxed runner

The broker calls the Omnigent API, so the guest session must exist and be run by
an isolated runner:

1. create **one** session whose agent is the guest Pi agent, with the model fixed
   to `ollama/deepseek-v4.1-flash:cloud` **on the session** (the broker never
   sends a model);
2. run that session in the **guest's own container**: the Omnigent host owns the
   session/model/history, while all shell/file/tool execution must happen inside
   that container. Give it only its own persistent workspace (downloads,
   binaries, local state): no host home, no other users' files or sessions, no
   container/DBus/systemd socket, no sudo, no host services, no network beyond the
   required endpoints. A shared administrative runner is not acceptable. The
   scoped gateway now restricts the broker's root-session API, but it does not
   solve managed-runner credentials; see `docs/RUNNER-AUTH-BLOCKER.md`;
3. start the scoped gateway in a separate foreground operator process with
   `OMNIGENT_OPERATOR_TOKEN` and `KURISU_GW_SECRET` set:
   `.venv/bin/python -m guest_broker.cli gateway --config config/broker.local.yaml`.
   The broker gets only `KURISU_GW_SECRET`, not the operator token;
4. probe through the scoped socket, without a direct token or message:
   `.venv/bin/python -m guest_broker.cli preflight --config config/broker.local.yaml --probe-api`.
   The probe reports runner/model metadata but cannot prove isolation;
5. record the id with `guest_broker provision --user pilot-001 --session-id <id>`.

## Blocker 3b - real guest Omnigent session id

`config/registry.local.yaml` currently contains the placeholder
`conv_pilot001_pending`.

```bash
.venv/bin/python -m guest_broker.cli provision \
  --config ../config/broker.local.yaml \
  --user pilot-001 --session-id conv_<real-id>
```

This needs whatever credentials create an Omnigent session. Those stay with the
operator; the guest and the transport have no path to them.

## Blocker 4 - per-guest container runner (external acceptance)

The guest's runner must be its own Docker container (see Blocker 3 and
`docs/ARCHITECTURE.md`). The broker cannot verify it: `GET /v1/sessions/{id}`
returns runner metadata only. `preflight` reports it as unverified, and relayed
results are recorded as `isolation=operator-attested`, never `sandboxed=true`.

Until an operator demonstrates (a) one container per friend with the filesystem
and network restrictions above, and (b) that its runner/tunnel credential cannot
mint an owner bearer or enumerate/control sessions and is unreachable from guest
tools, this remains an **acceptance blocker**, not a completed guarantee. The
scoped broker gateway is implemented and fake-tested, but the installed managed
runner can mint an owner bearer from its binding token; see the source evidence
in `docs/RUNNER-AUTH-BLOCKER.md`.


## Dedicated per-guest instance: live steps (replace Blockers 3/3b/4 for the pilot)

Implemented and unit-tested: provisioning, unique ports and paths, config/path
validation, server and runner plans, the bwrap server confinement, the relay,
root-session reuse and acceptance checks (`broker/tests/test_instance.py`).
Not run here (no live container, server or credential was used). Exact operator
steps are in `docs/POC-RUNBOOK.md` §2b:

1. Build `<IMG>` from `guest-runtime/Containerfile.runner` with pinned versions.
2. `instance provision` → `instance preflight` (only `root-session` may fail).
3. `instance serve` (terminal A) and `instance runner` (terminal B).
4. Confirm the container's `omnigent host` connects tokenless in single-user
   mode through the relay. **Unproven**; if it needs a credential, stop and
   report back instead of adding one.
5. `instance root-session` → `provision --session-id <id>`; then gateway + broker.
6. `instance acceptance` must pass all checks, including after a child session
   exists, before setting `allow_sub_sessions: true`.
7. Negative checks from inside the container: the main server port (6767), the
   LAN and the Internet fail. From inside the bwrap'd server, the operator home,
   the main `.omnigent` and other guests are absent. See
   `docs/RUNNER-AUTH-BLOCKER.md`.

## Guest identity / open sign-in

The PoC open-sign-in examples do not require the friend's WhatsApp number in
advance. Google consent-screen Testing mode must contain only the intended friend;
the broker enforces one bound JID per guest. Closed mode instead requires the
real JID in `config/allowlist.local.json` (`0600`) and denies every unknown sender.

## Optional hardening (not required for the PoC)

- Verify the Google ID token against JWKS and check `aud`/`iss`/`email_verified`
  instead of trusting userinfo alone.
- Move the guest workspace to a filesystem with project quotas so disk is a hard
  guarantee rather than a watchdog.
- Enforce `email_verified` and an allowlist of Google test-user addresses in
  addition to the WhatsApp allowlist.
