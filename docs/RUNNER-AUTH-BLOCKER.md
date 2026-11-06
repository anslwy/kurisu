# Runner authorization blocker (installed Omnigent 0.20)

## Finding

The installed runner's tunnel-binding credential is **not a tunnel-only,
session-scoped capability**. It can mint a short-lived bearer for the session
owner, so it must not be exposed to an untrusted guest or treated as unable to
administer sessions.

Source inspection only; no server, credential or live runner was accessed:

1. `omnigent/runner/_entry.py::_make_managed_mint_factory` builds
   `POST /v1/runners/{token_bound_runner_id(binding_token)}/token`. Its documented
   purpose is to mint a managed runner's owner JWT, used for both the WebSocket
   tunnel and HTTP callbacks. The fallback path can install this factory when no
   user credential was resolved.
2. `omnigent/server/routes/runner_tunnel.py`'s token route checks
   `token_bound_runner_id(token) == runner_id`, resolves the managed runner's
   owner, then calls `auth_provider.mint_runner_token(owner, ttl)` (or the
   accounts-mode equivalent). The result is explicitly an owner bearer.
3. `omnigent/runner/identity.py::strip_runner_auth_secrets` removes environment
   variables from spawned children. That is useful, but it is not a process/UID
   boundary: it does not prove that arbitrary guest code cannot inspect a runner
   process or its in-memory minted bearer when both run in the same container.

## Consequence for Kurisu

The scoped gateway in this repository restricts the **broker's** session/items/
message API. It does not by itself change this managed-runner credential path.
Docker packaging alone also does not isolate same-container processes.

Therefore live guest-runner acceptance remains **blocked** until an operator or a
future server change proves all of the following, with negative tests:

* the runner credential cannot call the owner-token mint endpoint or any session
  list/create/control/model-administration route;
* runner authority is bound to the assigned runner/root session, including every
  permitted descendant;
* guest tools cannot read the runner's environment, process memory, files,
  descriptors or control channel (a same-UID `/proc` check is required);
* all tools execute inside that friend's dedicated container and cannot reach
  host/other-user paths, sockets or services.

No safe tunnel-only configuration was established in this bounded pass. Changing
global Omnigent authentication or provisioning live resources was prohibited, so
this is intentionally reported as a blocker rather than hidden behind a prompt or
an `operator-attested` label.

Sub-session creation remains disabled; its fake parent/depth/count checks do not
resolve this runner credential blocker.
