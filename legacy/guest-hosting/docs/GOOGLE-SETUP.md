# Kurisu: programmatic Google setup (what is scriptable, what is not)

Goal: a Google OAuth client of type **"TVs and Limited Input devices"** so Kurisu
can use the OAuth 2.0 **device authorization grant** (`/signin`) with **no public
callback URL and no public server**.

> Nothing in this document was executed against a Google, Firebase, or GCP
> account. No project was created, no credential was read, no remote
> configuration was changed. Commands are listed for the operator to run.

## TL;DR

| Step | Scriptable? | How |
| --- | --- | --- |
| Sign in to Google tooling | yes | `npx firebase-tools login` (already done by the operator) |
| Create the GCP project "Kurisu" | yes | `firebase projects:create` (Firebase Management API) |
| Enable needed APIs | yes | `gcloud services enable ...` (needs gcloud) |
| **Consent screen (brand + test users)** | **no public API** | Cloud Console |
| **Device-flow OAuth client (client id + secret)** | **no public API** | Cloud Console |
| Verify the client really supports device flow | yes | one `POST https://oauth2.googleapis.com/device/code` |
| Store the secret for the broker | yes | shell env `KURISU_GOOGLE_CLIENT_SECRET` |

The two console-only steps are the ones that matter, and they are console-only
for a documented reason (below), not because we did not look hard enough.

## 1. Scriptable: project creation

```bash
# uses the login you already have; pick a globally unique id
npx --yes firebase-tools@15 projects:create kurisu-<unique-suffix> --display-name "Kurisu"

# confirm (read-only)
npx --yes firebase-tools@15 projects:list
npx --yes firebase-tools@15 apps:list --project kurisu-<unique-suffix>
```

`projects:create` provisions the GCP project and attaches Firebase resources. That
is all Kurisu needs from Firebase; the pilot does **not** use Firebase
Authentication (see §5).

## 2. Scriptable: enable APIs

```bash
gcloud config set project kurisu-<unique-suffix>
gcloud services enable oauth2.googleapis.com
gcloud services enable openidconnect.googleapis.com   # userinfo endpoint
```

The device flow itself needs no extra API beyond the OAuth endpoints; enabling
these two is a convenience so quota/attribution is visible in the project.

## 3. Console-only: consent screen and device-flow client

**Consent screen** (APIs & Services -> OAuth consent screen):
- User type **External**, app name **Kurisu**
- Publishing status **Testing**
- Scopes: `openid`, `.../auth/userinfo.email`, `.../auth/userinfo.profile`
- **Test users**: add the friend's Google address (Testing mode only lets listed
  users complete the flow)

**OAuth client** (Credentials -> Create credentials -> OAuth client ID):
- Application type: **TVs and Limited Input devices**
- Name: `Kurisu broker`
- Result: client id + client secret. **No redirect URI is requested or needed.**

### Why these two are not scriptable (verified reasoning, not a guess)

1. **OAuth client creation has no public API.** The Cloud Console calls an
   internal/allowlisted service (`clientauthconfig.googleapis.com`). There is no
   documented, generally-available `gcloud`/REST method to create an OAuth client
   id of any type. The public `oauth2.googleapis.com` API issues tokens; it does
   not manage clients.
2. **The `gcloud alpha iap ...` commands are a different object.** They create an
   **IAP** brand and an **IAP** OAuth client (web type, used to protect IAP
   resources). They do **not** produce a "TVs and Limited Input devices" client,
   and an IAP client cannot run the device authorization grant. Using them here
   would silently give you a client that fails at `/signin`.
3. **Consent-screen test users** are part of the same console surface; there is no
   public API for the external-testing user list.

So: everything around the client is scriptable, the client itself is a
three-click console step. Do not substitute an IAP client or a Firebase web
client - both are incompatible with the device flow.

## 4. Scriptable: verify the client actually supports device flow

```bash
export KURISU_GOOGLE_CLIENT_ID='1234567890-abc.apps.googleusercontent.com'
curl -sS https://oauth2.googleapis.com/device/code \
  -d "client_id=${KURISU_GOOGLE_CLIENT_ID}" \
  -d "scope=openid email profile"
```

Expected: JSON containing `device_code`, `user_code`, `verification_url`,
`expires_in`, `interval`.

If you instead get `invalid_client` / `unauthorized_client`, the client is the
wrong type (usually a web client or an IAP client) - recreate it as
"TVs and Limited Input devices".

This request creates a short-lived device code only; it changes no configuration.

## 5. Firebase Authentication: a different product, not a substitute

Firebase Authentication can do "Sign in with Google", but:
- it uses the auto-provisioned **web** OAuth client with authorized redirect URIs;
- the flow is browser-redirect based, so it needs either a reachable callback or a
  custom token exchange written by us;
- there is no device-code path (`firebase` CLI configures providers, not device
  clients).

Using it would change the pilot's UX (no "open this URL and type this code") and
add a callback endpoint - the exact thing the device flow avoids. It is listed
here so the distinction is explicit; it is **not** implemented.

Identity Platform (`gcloud identity-platform`) has the same limitation: it
configures providers/tenants for app sign-in, not device-flow clients.

## 6. Hand the secret to the broker (never to Git or the guest)

```bash
# broker process environment only
export KURISU_GOOGLE_CLIENT_SECRET='<client secret>'
```

`config/broker.local.yaml`:

```yaml
auth:
  enabled: true
  google_client_id: 1234567890-abc.apps.googleusercontent.com
  google_client_secret_env: KURISU_GOOGLE_CLIENT_SECRET
  mappings_path: ../runtime/state/auth-mappings.json
  tokens_path: ../runtime/state/auth-tokens.json
  pending_ttl_seconds: 1800
```

Then `guest-broker preflight --config config/broker.local.yaml` must report the
secret as `set` and return `READY`.

## 7. Helper script

`scripts/kurisu-google-setup.sh` prints every scriptable step and refuses to run
anything without `--execute` **and** an explicit project id. It never reads or
prints credentials, never lists projects, and never touches the console-only
steps.
