# Kurisu PoC runbook (operator)

Everything below is an **operator** action. Nothing here was executed by the
implementation workspace: no account was paired, no message was sent, no Google
credential was read, and no host service was started.

---

## 0. What you need

| Item | Notes |
| --- | --- |
| Steam Deck / host with rootless Podman | `podman info --format '{{.Host.Security.Rootless}}'` must print `true` |
| A **secondary** WhatsApp account | the transport account. Never the primary account and never the guest's own number |
| The friend's WhatsApp number | goes into the allowlist as `<cc><number>@s.whatsapp.net` (digits only; a leading `+` is accepted and stripped) |
| A Google Cloud project | named **Kurisu** |
| The guest workspace | `<guests_root>/pilot-001/workspace` |

---

## 1. Google Cloud: project, consent screen, device-flow client

Programmatic vs console-only steps are itemised in `docs/GOOGLE-SETUP.md`
(project creation and API enablement are scriptable; the consent screen, test
users and the device-flow client are console-only because no public API exists).
The console walkthrough below is the same content in narrative form.

The device flow is chosen on purpose: **it needs no public callback URL and no
public server.** The broker polls Google's token endpoint; nothing has to be
reachable from the internet.

1. <https://console.cloud.google.com/projectcreate> - project name **Kurisu**
   (project id will look like `kurisu-<random>`; keep it).
2. **APIs & Services -> OAuth consent screen**
   - User type: **External**
   - App name: **Kurisu**
   - Support email: your address; developer contact: your address
   - Scopes: `openid`, `.../auth/userinfo.email`, `.../auth/userinfo.profile`
   - **Publishing status: Testing**, and add every tester under **Test users**
     (the friend's Google address). Testing mode is correct for a pilot: the
     client stays unverified and only listed test users can sign in.
3. **APIs & Services -> Credentials -> Create credentials -> OAuth client ID**
   - Application type: **TVs and Limited Input devices**  ← this is the device
     flow client. Do **not** create a "Web application" client: that needs a
     redirect URI, which is what we are avoiding.
   - Name: **Kurisu broker**
   - Result: a client id (`...apps.googleusercontent.com`) and a client secret.

**Required redirect URI: none.** If you ever switch to the web-server flow you
would need a public HTTPS callback such as
`https://<your-host>/oauth2/callback` reachable from Google, plus the exact
redirect URI registered in the client - which is why the device flow is used.

4. Put the secret only in the broker process environment:

```bash
export KURISU_GOOGLE_CLIENT_SECRET='<client secret>'   # broker process only
```

The client id goes into `config/broker.local.yaml`:

```yaml
auth:
  enabled: true
  google_client_id: 1234567890-abc.apps.googleusercontent.com
  google_client_secret_env: KURISU_GOOGLE_CLIENT_SECRET
  mappings_path: ../runtime/state/auth-mappings.json
  tokens_path: ../runtime/state/auth-tokens.json
  pending_ttl_seconds: 1800
```

Never commit the secret, and never place it in the guest workspace, the guest
prompt, or the WhatsApp transport config.

---

## 2. Guest image (rootless Podman)

Build once on the host (not required for the local echo demo):

```bash
podman build -t kurisu/guest:1.0.0 -f guest-runtime/Containerfile .
```

Podman mode is **experimental in this PoC** (argv-tested only, no container was
started here). It also needs the Pi path *inside the image*
(`/usr/local/bin/pi` for `guest-runtime/Containerfile`), not the host path:

```yaml
runtime:
  pi_path: /usr/local/bin/pi        # container path, not /run/media/.../.local/bin/pi
```

Then point the broker at it:

```yaml
sandbox:
  mode: podman
  image: kurisu/guest:1.0.0
  podman_path: /usr/bin/podman
```

The plan the broker builds for a guest turn is (see `broker/guest_broker/sandbox.py`):

```
podman run --rm --userns=keep-id --read-only
  --tmpfs /tmp:rw,size=<limits.tmpfs_size>,mode=1777
  --cpus <cpu_quota> --memory <memory_max> --memory-swap <memory_max>
  --pids-limit <tasks_max> --cap-drop=ALL --security-opt no-new-privileges
  --network=none --ipc=none --log-driver=none
  --ulimit fsize=...:... --ulimit nofile=...:... --ulimit core=0:0
  --workdir <workspace> --volume <workspace>:<workspace>:rw
  --volume <guest-io>:/run/guest-io:ro
  <image> pi --model ollama/deepseek-v4.1-flash:cloud ...
```

Because `--network=none` is set, the only network path is the model proxy socket
(`/run/guest-io/model-proxy.sock`) which the broker allowlists to `/v1/` on the
local model endpoint.

> This workspace runs inside a Distrobox container that has no Podman binary and
> no Podman socket, so the Podman plan is **argv-tested only** here. The
> `systemd-bwrap` mode is the one exercised end-to-end on this machine. Both are
> covered by `docs/RUNTIME-DECISION.md`.

---

## 3. Secondary-account WhatsApp pairing (operator, once)

```bash
cd transport/whatsmeow-gateway
go build -o bin/whatsmeow-gateway ./cmd/whatsmeow-gateway

# state dir must exist with mode 0700 and holds the device credentials
mkdir -p ../../runtime/whatsapp && chmod 700 ../../runtime/whatsapp

./bin/whatsmeow-gateway status --config ../../config/gateway.local.json
./bin/whatsmeow-gateway pair   --config ../../config/gateway.local.json --confirm-secondary
```

`pair` prints a QR code **in the terminal only** (never a file). Scan it on the
phone that holds the **secondary** account: WhatsApp -> Settings -> Linked
devices -> Link a device.

Refusals built into the binary: it will not run as root, it will not pair
without `--confirm-secondary`, it will not re-pair an existing device, and it
will not start if the allowlist is empty or if the paired account is itself an
allowlisted guest identity.

Then start it:

```bash
./bin/whatsmeow-gateway run --config ../../config/gateway.local.json
```

It connects to the broker socket, sends a `hello` with the account JID and the
allowlist fingerprint, and the broker pins that account on first use. A second,
different account is then refused - which is what keeps the primary account out.

---

## 4. Start the broker

```bash
cd broker
export KURISU_GOOGLE_CLIENT_SECRET='<client secret>'
.venv/bin/python -m guest_broker.cli preflight --config ../config/broker.local.yaml
.venv/bin/python -m guest_broker.cli run       --config ../config/broker.local.yaml
```

`run` stays in the foreground and installs nothing. Preflight must print
`READY`; it refuses to run when a limit cannot be enforced.

Provision the single guest session id (operator, once - this is the only way a
session id enters the system):

```bash
.venv/bin/python -m guest_broker.cli provision \
  --config ../config/broker.local.yaml --user pilot-001 --session-id conv_<real-session-id>
```

---

## 5. The friend's test

1. Friend messages the **secondary** number.
2. Bot replies: *"this WhatsApp account is not verified yet... Send /signin"*.
   Everything except `/signin` is refused, including prompt-injection attempts
   such as `/signin <instructions>` (the command match is exact).
3. Friend sends **`/signin`**. Bot replies with:
   ```
   1) Open https://www.google.com/device on any device
   2) Enter code: XXXX-XXXX
   ```
4. Friend opens the URL, enters the code, approves with the Google account that
   is on the consent screen's **test users** list.
5. Friend sends **`/signin`** again. The broker polls Google (interval honoured,
   `slow_down` respected), fetches the OIDC identity, and binds:
   `sub -> wa_jid -> pilot-001` in `runtime/state/auth-mappings.json` (0600).
   Google tokens are written to `runtime/state/auth-tokens.json` (0600) and are
   **never** sent to the guest, the transport, or the audit log.
6. Friend now sends normal messages and gets sandboxed answers from the fixed
   DeepSeek model.

Pending codes expire (`auth.pending_ttl_seconds`, and Google's own `expires_in`,
whichever is smaller), are single-use, and are dropped on denial/expiry.

---

## 6. Persistence check after disposal/restart

```bash
# 1. note the session id and one workspace file
grep session_id config/registry.local.yaml
echo hello > /run/media/.../guests/pilot-001/workspace/persist-probe.txt

# 2. dispose the guest runtime (container is --rm per turn; to force it:)
podman ps --all | grep guest- || true
podman image inspect kurisu/guest:1.0.0 >/dev/null   # image stays

# 3. restart the broker and the gateway (Steam Deck reboot is equivalent)
# 4. friend sends one message - no /signin needed
```

Expected after restart:

| Thing | Expected |
| --- | --- |
| `/signin` | `already signed in as ...` - no new device code |
| guest session id | identical (`registry.local.yaml` untouched) |
| Pi history | continues in `<workspace>/.agent-state/sessions` |
| `persist-probe.txt` | still present in the workspace |
| WhatsApp pairing | still valid - no QR needed |
| toolchains | not reinstalled; `runtime/toolchains`, Go/Node caches and `runtime/whatsapp` are host-mounted |

See `docs/PERSISTENCE.md` for the directory table.

---

## 7. Known external blockers (cannot be resolved in this workspace)

1. **Google OAuth client** - needs a Google Cloud project ("Kurisu"), the Testing
   consent screen with test users, and a "TVs and Limited Input devices" client
   id + secret. Without it `auth.enabled` cannot be turned on for real (the code
   path is tested with a fake transport).
2. **Secondary WhatsApp pairing** - needs the phone holding that account. QR
   pairing is implemented but deliberately not executed here.
3. **Real Omnigent session id** - `config/registry.local.yaml` currently holds
   `conv_pilot001_pending`. Create the guest session with the operator's Omnigent
   credentials and run `guest_broker provision` (step 4).
4. **Rootless Podman on the broker host** - not available inside the Distrobox
   container used for this implementation, so Podman execution must be verified
   on the host itself (argv is already tested).
5. **Friend's real WhatsApp number** - required for the allowlist entry.
