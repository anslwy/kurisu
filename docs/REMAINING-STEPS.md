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

## Blocker 3 - real guest Omnigent session id

`config/registry.local.yaml` currently contains the placeholder
`conv_pilot001_pending`.

```bash
.venv/bin/python -m guest_broker.cli provision \
  --config ../config/broker.local.yaml \
  --user pilot-001 --session-id conv_<real-id>
```

This needs whatever credentials create an Omnigent session. Those stay with the
operator; the guest and the transport have no path to them.

## Blocker 4 - rootless Podman on the broker host

**Needs:** running the broker directly on the host (not inside the Distrobox
container used here), with `podman info --format '{{.Host.Security.Rootless}}'`
returning `true`.

```bash
podman build -t kurisu/guest:1.0.0 -f guest-runtime/Containerfile .
# config: sandbox.mode: podman, sandbox.image: kurisu/guest:1.0.0
```

Then re-run `preflight` and `selftest` on that host. Until then the verified
runtime is `systemd-bwrap`.

## Blocker 5 - friend's WhatsApp number

Replace the placeholder in `config/allowlist.local.json` with the real JID
(`<cc><number>@s.whatsapp.net`), keep the file `0600`. An unknown sender is
denied and receives no reply at all.

## Optional hardening (not required for the PoC)

- Verify the Google ID token against JWKS and check `aud`/`iss`/`email_verified`
  instead of trusting userinfo alone.
- Move the guest workspace to a filesystem with project quotas so disk is a hard
  guarantee rather than a watchdog.
- Implement the Omnigent backend (`runtime.backend: omnigent`) so the guest runs
  as the registered Omnigent session instead of a local Pi process; the registry,
  mapping and gate already support it.
- Enforce `email_verified` and an allowlist of Google test-user addresses in
  addition to the WhatsApp allowlist.
