# Runner authorization: contained by per-guest instances (live checks remain)

## Original finding (unchanged, installed Omnigent 0.20)

The installed runner's tunnel-binding credential is **not a tunnel-only,
session-scoped capability**. It can mint a short-lived bearer for the session
owner:

1. `omnigent/runner/_entry.py::_make_managed_mint_factory` builds
   `POST /v1/runners/{token_bound_runner_id(binding_token)}/token`, which mints the
   managed runner's owner JWT for the WebSocket tunnel and HTTP callbacks.
2. `omnigent/server/routes/runner_tunnel.py` resolves the runner's owner and calls
   `auth_provider.mint_runner_token(owner, ttl)`: an owner bearer.
3. `omnigent/runner/identity.py::strip_runner_auth_secrets` scrubs child env, which
   is not a process/UID boundary inside one container.

On a **shared** server that owner owns the operator's sessions, so the runner
(and any guest code that can read it) could administer them.

## What changed: the owner owns nothing but this guest

The credential is unchanged. What changed is what it can reach. Each guest now
gets a **dedicated Omnigent server instance** (`guest_broker/instance.py`,
`docs/ARCHITECTURE.md` "Dedicated per-guest instance") with its own config home,
data dir, SQLite DB, artifacts and logs, bound to `127.0.0.1:<unique port>` in
single-user mode. The guest's runner runs in the guest's `--network=none`
container. Its only route is a fixed Unix-socket relay to *that* port.

| Original acceptance item | Status |
| --- | --- |
| Runner credential cannot mint an owner bearer or list/create/control sessions | **Not changed, and no longer needed.** It can do all of that, but only on its own instance, whose DB holds only this guest's tree. The main Omni Lead server and other guests are unreachable: no route, separate DBs and separate ports (tested: `test_runner_plan_is_network_none_and_guest_mounts_only`, `test_two_guests_share_no_port_or_path`, `test_host_relay_reaches_only_the_instance_port`). |
| Authority bound to the root session and its descendants | **Solved by construction, pending a live check.** The instance contains nothing else. `instance acceptance` fails closed unless every session descends from the recorded root, runs on the root's runner and uses the fixed model (`check_session_tree`). |
| Guest tools cannot read the runner's env/memory/control channel | **Not solved, and accepted.** The runner and tools share the container. Whatever the guest steals is authority over its own instance only, which it already has through its agent. |
| All tools execute in that guest's container with no host or other-user paths | **Solved in the plan, pending a live check.** `runner_plan` + `assert_runner_plan_safe` cover this: `--network=none`, no host sockets, `--cap-drop=ALL`, and mounts limited to this guest's paths. |

## Residual risk that replaces the blocker

The instance **server** runs on the host, and the guest's runner can drive its API
as the owner. A server feature that executed guest-supplied code, or read host
files server-side, would reach the host. Mitigations in this pass:

* `instance serve` wraps the server in bubblewrap by default
  (`server_sandbox_plan`). It gets read-only system and install trees, a
  read-write mount of only its own instance dir, and a cleared environment. It
  has no operator home, no main `.omnigent`, no other guest and no `/run/user`.
  The network is shared because the server must bind host loopback.
* No server `--config` is passed, so no managed-sandbox provider or accounts
  admin is configured (`static_preflight` checks `config.yaml`).
* The model proxy rewrites every request's `model` to the fixed DeepSeek id, so
  changing a session's model through the owned API cannot change the model
  actually called by the runner.

## Live checks that remain (operator; not run here)

1. `guest-broker instance serve ... --sandbox bwrap --ro-path <omnigent tool dir> --ro-path <python>`
   starts and serves on `127.0.0.1:<port>` only (`instance acceptance` → `listener`).
2. The containerized `omnigent host --server http://127.0.0.1:<port>` connects
   through the relay in single-user mode without any credential, and it is the
   only host (`hosts` check).
3. The root session runs on that container runner. A child created by the agent
   inherits the runner and stays in the tree (`tree:*`, `runner:*`, `model:*`).
4. From inside the container: `curl http://127.0.0.1:6767` (main server port)
   and any LAN or Internet address fail, while the instance port works.
5. From inside the bwrap'd server: the operator home, the main `.omnigent` and
   other guests' dirs are absent.
