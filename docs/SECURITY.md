# Kurisu security model

## Trust boundaries

| Component | Trust | Can it see credentials? |
| --- | --- | --- |
| WhatsApp transport (`transport/whatsmeow-gateway`) | untrusted input to the broker | no: it holds only the secondary account's device store |
| Guest (Pi inside the sandbox) | untrusted | no: no Google token, no Omnigent token, no transport store, no registry |
| Broker (`broker/`) | trusted, operator-run | yes: Google tokens, registry, allowlist, audit |
| Host | operator | yes |

The broker treats every frame from the transport as hostile. The guest treats
every prompt as hostile. Neither can widen its own privileges by asking.

## Requirement -> enforcement -> evidence

| Requirement | Enforcement | Evidence |
| --- | --- | --- |
| Guest uses Pi with a **fixed DeepSeek** model; no model selection | broker builds a fixed argv (`--provider ollama --model deepseek-v4.1-flash:cloud`); the guest's own Pi config lists **only** that model; config rejects globs/lists | `tests/test_core.py::test_config_rejects_model_lists`, `tests/test_sandbox.py::test_plan_contains_every_limit` |
| Server-enforced `pilot-001` -> **exactly one** session id | scalar in `registry.local.yaml`; duplicate session ids rejected; workspace path fixed to `guests/pilot-001/workspace`; only `guest-broker provision` can write it | `test_registry_binds_exactly_one_session`, `test_registry_refuses_one_session_for_two_users`, `test_provision_is_operator_only_and_refuses_rebind` |
| No arbitrary session ids from the transport | transport has no field for it; a frame containing `session_id`/`user_id`/`model`/`workspace`/`command` is a **hard protocol error** | `test_protocol_rejects_session_selection_fields`, `test_inbound_cannot_smuggle_a_session_id` |
| No global list/use, no cross-user control | broker exposes no list/session API at all; one connection per gateway; account pinned on first use; a second account is refused | `test_handshake_refuses_a_different_account_after_pinning` |
| No administrative Omnigent credentials reachable by guest or transport | neither component imports or reads an Omnigent token; `runtime.omnigent.token_env` is only consulted by the (unimplemented, fail-closed) Omnigent backend | `backend.py::OmnigentBackend` fails closed with operator steps |
| Guest confined to `guests/<user>/workspace` | bwrap: read-only system binds, one `--bind` of the workspace, `--remount-ro /`; Podman: `--read-only` + one `:rw` volume | `test_plan_has_exactly_one_writable_host_bind`, `test_only_the_workspace_is_writable`, `test_podman_plan_is_hardened` |
| Real CPU/memory/PID/disk/runtime limits | cgroup v2 (`MemoryMax`, `MemorySwapMax=0`, `CPUQuota`, `TasksMax`, `RuntimeMaxSec`, `IOWeight`) + rlimits (`RLIMIT_FSIZE/NOFILE/NPROC/CORE`) applied in-namespace + workspace disk watchdog | `test_memory_limit_is_enforced_by_the_kernel` (OOM kill), `test_task_limit_is_enforced`, `test_runtime_limit_kills_a_long_turn`, `test_file_size_rlimit_blocks_large_writes`; `preflight` reports `kernel OOM-killed the guest at MemoryMax` |
| Directory permissions/prompts are **not** the boundary | prompts are advisory only; the sandbox is the boundary; config refuses `sandbox.mode: none` for a real run | `test_plan_refuses_mode_none`, `cli.cmd_run` refusal path |
| Default deny before verification | only the exact command `/signin` is accepted; every other message is refused before the guest runs | `test_unverified_identity_is_denied_before_the_guest_runs`, `test_signin_command_matching_is_exact` |
| Verified Google `sub` -> WhatsApp identity -> `pilot-001` | binding is 1:1 in both directions; a second WhatsApp identity cannot claim the same `sub`; an identity cannot switch accounts | `test_binding_binds_one_sub_to_one_whatsapp_identity`, `test_binding_refuses_identity_switch` |
| Pending states expire | `expires_at` from Google, capped by `pending_ttl_seconds`; interval honoured; `slow_down` respected; single-use | `test_manager_pending_expires`, `test_manager_respects_poll_interval`, `test_manager_denied_and_expired_clear_pending` |
| Google tokens broker-only, never in the guest session | tokens written to `runtime/state/auth-tokens.json` (0600); the backend call receives only the guest message; replies and audit contain no token | `test_tokens_are_private_and_not_in_the_public_view`, `test_audit_records_contain_no_tokens_or_user_codes`, `test_signin_then_verified_message_reaches_the_guest` |
| Transport is a **secondary** account only | `pair` requires `--confirm-secondary`, refuses re-pairing; `run` refuses if the paired JID is an allowlisted guest; broker pins the account and refuses a different one | gateway `policy`/`config` tests, `test_handshake_refuses_a_guest_identity_as_the_transport` |
| No pairing/sending/credential access by this workspace | `pair` was never run; gateway `status` reports `paired: no`; no credential file exists in the tree | `runtime/whatsapp/` contains no store |
| Guest-initiated containers | prohibited sockets/privileged DinD denied in policy; feature disabled at config load | `tests/test_container_policy.py` |

## Resource limits, and what is genuinely enforced where

| Limit | Mechanism | Verified on this host |
| --- | --- | --- |
| memory | cgroup `MemoryMax` + `MemorySwapMax=0` | yes - SIGKILL at the ceiling |
| CPU | cgroup `CPUQuota` | yes - 22% observed against 25% requested |
| processes/threads | cgroup `TasksMax` | yes - fork blocked |
| wall clock per turn | `RuntimeMaxSec` (+ broker outer timeout) | yes - killed early |
| single file size | `RLIMIT_FSIZE` via in-namespace launcher | yes - write capped |
| open files / core dumps | `RLIMIT_NOFILE`, `RLIMIT_CORE=0` | yes (plan + launcher) |
| `/tmp` | size-limited private tmpfs | yes |
| workspace disk | watchdog budget (`disk_quota`) | supervisor-enforced; the SD card filesystem has no quota support, so this is a kill-on-breach guard, not a filesystem quota |
| network | net namespace with only the model proxy socket | yes - no host interface in the namespace |

## What the guest cannot reach

`/home`, `/root`, `/run/user`, `/run/dbus`, `/run/systemd`, container runtime
sockets, `/sys`, `/boot`, the host network, the Google token file, the registry,
the allowlist, the WhatsApp device store, and any other user's workspace.

Verified by `test_filesystem_isolation` and
`test_network_namespace_blocks_host_reachability`.

## Residual risks (honest list)

1. **Prompt injection** is expected. The model is not a security boundary; the
   sandbox and broker checks are. A successful injection can only use the tools
   the guest already has inside its own workspace.
2. **Disk quota on exFAT/SD** is supervisor-enforced (kill on breach), not a
   filesystem quota. Move the guest workspace to a filesystem with project
   quotas for a hard guarantee.
3. **Pi history is guest-writable** (it lives in the workspace). That is the
   guest's own conversation, not a trust boundary.
4. **Google ID-token signature** is not verified; identity is taken from the
   OIDC userinfo endpoint over TLS with the access token. Hardening step:
   verify the ID token against Google's JWKS and check `aud`/`iss`/`email_verified`.
5. **WhatsApp transport compromise** cannot escalate: it can only submit frames,
   and those are strictly validated and attributed by allowlist, not by anything
   the frame claims.
