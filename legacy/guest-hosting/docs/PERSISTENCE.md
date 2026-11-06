# Kurisu persistence

Restarting the guest container (or rebooting the Steam Deck) must never lose the
WhatsApp pairing, the Google sign-in binding, the guest session id, the workspace
files, or the Pi history. Those live in host directories, never inside the
disposable container.

Scope of what is actually verified today: the broker-side state (registry, auth
mappings/tokens, audit) is exercised across restarts; the workspace and
`.agent-state` paths exist by construction. **No real Pi turn has been run**, so
"Pi history survives" is by design, not by observation, and **no toolchain/cache
mount exists for the guest yet** (see the corrected rows below).

## Directory table

All paths are relative to `guests/pilot-001/implementation/` (the deployment
tree) unless noted. Files holding secrets or identity are mode `0600`; their
directories are `0700`.

| Path | Mode | Survives container replacement | Survives reboot | Contents |
| --- | --- | --- | --- | --- |
| `runtime/whatsapp/wa-store.db` (+ `-wal`/`-shm`/`-journal`) | 0700 dir / **0600 files, enforced by the gateway** | yes (host mount) | yes | whatsmeow device credentials + signal session store. **Losing this = re-pairing the secondary account.** |
| `runtime/state/gateway-account.json` | 0600 | yes | yes | pinned transport account JID (trust-on-first-use) |
| `runtime/state/auth-mappings.json` | 0600 | yes | yes | verified Google `sub` <-> WhatsApp JID <-> `pilot-001` |
| `runtime/state/auth-tokens.json` | 0600 | yes | yes | Google access/refresh tokens, **broker-only**, never mounted into the guest |
| `config/registry.local.yaml` | 0600 | yes | yes | the single guest Omnigent session id + workspace path |
| `runtime/state/relay.json` | 0600 | yes | yes | per-session relay cursor, bounded seen item ids, pending-turn marker, reply cache (dedup/restart) |
| `runtime/state/` (pending sign-ins) | in-memory | n/a | n/a | pending device codes expire by design; a restart mid-sign-in just needs `/signin` again |
| `guests/pilot-001/workspace/` | 0700 | yes | yes | guest work area **when the runner mounts it**; the broker no longer runs the agent, so persistence here is the runner's responsibility |
| `guests/pilot-001/workspace/.agent-state/sessions/` | 0700 | yes (inside the workspace) | yes | Pi conversation history (`--session-dir`), so the guest continues where it left off |
| `guests/pilot-001/workspace/.agent-state/pi-agent/` | 0700 | yes | yes | the guest's own Pi config: exactly one model (fixed DeepSeek) |
| `runtime/audit.jsonl` | 0600 | yes | yes | broker audit: events, hashes and lengths only - no message text, no tokens |
| `runtime/toolchains/`, Go module cache, Node prefix | 0700 | **intended, not verified** | **intended** | planned host-mounted toolchains/caches so a restart does not re-download. No such guest mount exists yet; today only the workspace and `.agent-state` are mounted. |
| `guests/pilot-001/implementation/.toolchain/go` | 0755 | yes | yes | the Go toolchain used to build the gateway |

## Why this layout

* The broker's state lives **outside** any guest workspace. The config loader
  refuses a state/registry/allowlist path inside `guests/<user>/workspace`
  because that is the single writable path the sandbox exposes.
* The whatsmeow store and the Google token file are never bind-mounted into the
  guest. The guest gets the workspace and a read-only model-proxy socket, nothing
  else.
* Pi history lives inside the workspace because it *is* the guest's own data;
  keeping it there means a container replacement keeps the conversation with no
  extra plumbing.

## Restart test (what "passing" means)

1. Stop broker and gateway.
2. Remove the guest container if one is around (`podman rm -f guest-pilot-001`,
   or rely on `--rm` per turn).
3. Reboot the Steam Deck.
4. Start broker + gateway again, friend sends a plain message.

Pass criteria:

- no QR pairing required;
- no `/signin` required (`already signed in`);
- `config/registry.local.yaml` still shows the **same** session id;
- `<workspace>/persist-probe.txt` still exists;
- Pi continues the same session file under `.agent-state/sessions/`;
- no toolchain re-download (build the gateway again and confirm it is instant).

## Verified locally (no credentials needed)

The broker-side half of this table was exercised end to end:

```
$ printf '/signin\nhello\n/signin\nhello kurisu\n' | guest-broker dev --config config/broker.dev.yaml --fake-signin
... [reply] Signed in as pi***@example.test ...
... [reply] [echo:pilot-001] hello kurisu

$ printf '/signin\nhello again\n' | guest-broker dev --config config/broker.dev.yaml --fake-signin
... [reply] You are already signed in as pilot@example.test ...
... [reply] [echo:pilot-001] hello again
```

```
$ ls -l runtime/state/
-rw------- auth-mappings.json
-rw------- auth-tokens.json
```

The WhatsApp device store and the Podman volume mounts are the parts that need
the operator's credentials/host to verify (see `docs/REMAINING-STEPS.md`).

Correction from review: the earlier table implied toolchain/cache mounts already
exist for the guest. They do not.

Verified today: broker state survives restarts (demonstrated with the fake sign-in
flow), and the whatsmeow store path exists (empty; nothing is written until an
operator pairs). By construction, not yet observed: Pi history under
`.agent-state/sessions` (no real turn has run) and workspace file persistence
across a container replacement (no container has been run here).

## Credential-file permissions (enforced, not just documented)

The WhatsApp SQLite store contains the paired device's keys, so the gateway does
not rely on the operator's umask:

* it sets a process umask of `0077` **before** opening the store, so the DB and
  every file SQLite creates later (`-wal`, `-shm`, `-journal`) are private from
  creation;
* on every `run` / `pair` / `status` start it also `chmod`s the state directory to
  `0700` and any existing store file to `0600`, which repairs a store created by
  an older build (the reported `0644` case);
* it never reads or prints credential contents.

Regression coverage: `cmd/whatsmeow-gateway/store_perms_test.go`.

Fail-closed details and known limitations:

* only a genuinely absent file is skipped; any other stat error, a symlink at the
  store/companion path, or a non-regular file is an **error**, so the process
  refuses to run rather than leave a credential path unsecured;
* `pair` re-secures the files right after pairing succeeds (that is when SQLite
  writes them), on top of the process umask;
* POSIX ACLs are not cleared: `chmod` changes mode bits only. A pre-existing ACL
  grant would survive. This is production hardening, not a PoC blocker;
* DSN parsing is intentionally narrow (`file:` prefix + `?query` stripping); the
  config generates plain absolute POSIX paths;
* `status` can create an empty database as a side effect of opening it - it is
  created under the private umask, so it is 0600 from the start.

The helper never reads or prints credential contents.

## Dedicated instance persistence (per guest)

| Path under `<guests_root>/<user>/` | Holds | Survives |
| --- | --- | --- |
| `instance/instance.json` (0600) | port, fixed model, **root session id**, host id | process, container, Deck restart |
| `instance/data/omnigent.db`, `instance/data/artifacts/` | instance session history, items, artifacts | same |
| `instance/config/`, `instance/home/`, `instance/logs/` | instance config home, server HOME, logs | same |
| `persist/home/` (container `/home/guest`) | runner state, Pi history, caches, `~/.omnigent` of the runner | same; the container is `--rm` |
| `persist/downloads/`, `persist/toolchains/`, `persist/state/` | downloads, binaries/toolchains, local state | same |
| `workspace/` | the guest's files | same |

WhatsApp device state stays in the transport's state dir, and Google OAuth state
stays in the broker's `runtime/state` (sections above). Neither is mounted into
the guest container. A restart reuses the root id from `instance.json`
(`ensure_root_session` never silently creates a second root).
