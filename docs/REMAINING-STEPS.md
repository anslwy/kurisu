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
   model endpoint. A shared administrative runner is not acceptable;
   a dedicated Omnigent identity for the guest is **necessary but not
   sufficient**: it isolates the guest's sessions from the operator's, but that
   identity can still list/create/control every session it owns. Single-session
   restriction requires a scoped gateway (not shipped here) and is an **external
   acceptance blocker**; see docs/ARCHITECTURE.md "auth limitations";
3. export the operator token the broker will use:
   `export OMNIGENT_OPERATOR_TOKEN='<token>'` (broker process only - never in the
   transport image, the guest, or Git);
4. verify reachability without sending a message:
   `curl -sS -H "Authorization: Bearer $OMNIGENT_OPERATOR_TOKEN" \
      http://127.0.0.1:6767/v1/sessions/<session-id> | head -c 200`
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
and network restrictions above, and (b) either a session-scoped credential or a
scoped gateway, this remains an **acceptance blocker**, not a completed guarantee.


## Blocker 5 - friend's WhatsApp number

Replace the placeholder in `config/allowlist.local.json` with the real JID
(`<cc><number>@s.whatsapp.net`, digits without `+` preferred), keep the file `0600`. An unknown sender is
denied and receives no reply at all.

## Optional hardening (not required for the PoC)

- Verify the Google ID token against JWKS and check `aud`/`iss`/`email_verified`
  instead of trusting userinfo alone.
- Move the guest workspace to a filesystem with project quotas so disk is a hard
  guarantee rather than a watchdog.
- Add a scoped gateway in front of the Omnigent API so the broker/runner hold a
  single-session capability instead of an owner-scoped token (see
  docs/ARCHITECTURE.md "auth limitations").
- Enforce `email_verified` and an allowlist of Google test-user addresses in
  addition to the WhatsApp allowlist.
