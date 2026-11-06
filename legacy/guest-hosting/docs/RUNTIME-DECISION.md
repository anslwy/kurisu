# Kurisu runtime decision: rootless Podman vs systemd scope + bubblewrap

## Short answer

**Rootless Podman is the intended production runtime, but it is NOT verified for
this PoC.** The example config selects `systemd-bwrap`, the mode that was
exercised end-to-end on this machine. Podman mode is argv-tested only and is
marked experimental. **systemd scope + bubblewrap is the fallback that is verified to work in
the environment this code was developed in**, because that environment has no
Podman available to the broker process. Both live behind the same
`sandbox.build_plan()` interface, so switching is a config change - but a Podman
switch also needs the in-image Pi path (`/usr/local/bin/pi`), an image build,
and a host-side verification run before it can be called supported.

## Evidence from the development environment

| Capability | Result | Consequence |
| --- | --- | --- |
| `podman` inside the Distrobox container | **absent** (`command -v podman` empty; no `podman.sock`) | the broker cannot start containers from here |
| Host Podman | present at `/run/host/usr/bin/podman`, rootless storage at `/run/user/1000/containers` | usable only by reaching into host paths |
| `systemd-run --user --scope` | works | transient cgroup v2 scopes are available |
| `bwrap` | works, unprivileged userns enabled | namespaces + read-only root available |
| Kernel memory enforcement | child OOM-killed at `MemoryMax` | limits are real, not advisory |
| CPU quota | 22% vs 25% requested | real CFS throttling |
| `TasksMax=8`, fork bomb | `BlockingIOError` at 7 forks | real pid ceiling |
| `RuntimeMaxSec=3`, `sleep 30` | killed after ~4s | real wall-clock ceiling |

Using the host Podman binary from inside the container would mean binding host
paths (`/run/host/...`, `/run/user/1000/containers`, the host's crun/conmon).
That is exactly the kind of host-service reach-through this pilot must not have,
and the user instruction for this milestone was to not touch host services.

## Comparison

| Property | rootless Podman | systemd scope + bwrap (this repo, verified) |
| --- | --- | --- |
| Memory / CPU / PID limits | cgroup v2 via container cgroup | cgroup v2 via transient scope |
| Runtime ceiling | broker supervisor timeout, then `podman kill` + `podman rm -f` (tested with a fake podman); no `--stop-timeout` guarantee is claimed | `RuntimeMaxSec` enforced by systemd |
| Filesystem isolation | image root read-only, one rw volume | read-only bind set, root remounted ro, one rw bind |
| Capabilities | `--cap-drop=ALL` | user namespace, no capabilities by construction |
| `no_new_privs` | explicit `--security-opt` | implicit in user namespace |
| Network | `--network=none` (+ socket mount) | `--unshare-all` net namespace (+ socket mount) |
| Predictable toolchain | **planned** - would pin Node/Pi in the image (no image built/verified here) | depends on host paths (`read_only_paths`) |
| Image build / storage needed | yes | no |
| Runs inside this Distrobox container | no | yes |
| Status in this PoC | **experimental / argv-tested only** | **verified end-to-end** |
| Guest-initiated containers | plausible (nested userns) | not realistic |

## What each mode therefore gives the pilot

* **Podman mode** is stronger for reproducibility (the guest cannot see host
  library drift) and slightly stronger on defence-in-depth (`no-new-privileges`
  is explicit, the image root is immutable by construction). It is the intended
  production path.
* **bwrap mode** is the one that can be proven today on this machine, and it
  already satisfies the pilot's hard requirement: the guest cannot exhaust or
  reboot the host, cannot see host files, and cannot reach any host service other
  than the allowlisted model endpoint.

Neither mode changes the broker-side guarantees (identity mapping, single
session, fixed model, token custody): those live in Python, above the sandbox.

## Migration step for the operator

```yaml
sandbox:
  mode: podman
  image: kurisu/guest:1.0.0     # pinned tag, or a digest
  podman_path: /usr/bin/podman
```

Then run `guest_broker.cli preflight`: in Podman mode it probes
`podman info --format '{{.Host.Security.Rootless}}'` and refuses to start when it
is not `true`.

## Deferred: DinD / nested container use

Guest-initiated container use is deferred and disabled
(`sandbox.guest_containers: none`, enforced at config load). The user instruction
for this milestone was explicit: do not let it block the PoC. The evaluation and
the only acceptable future path are in `docs/GUEST-CONTAINERS.md`.
