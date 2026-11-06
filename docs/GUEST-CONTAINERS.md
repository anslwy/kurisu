# Guest-initiated container use (deferred)

Some future guest tasks may need to build or run containers themselves. This
document records the evaluation so nobody re-litigates it from scratch, and so
the prohibited shortcuts stay prohibited.

**Status: disabled.** `sandbox.guest_containers` must be `none`;
any other value is rejected at config load with a pointer to this file. DinD is
deferred by instruction and does not block the PoC.

## Prohibited, no exceptions

| Option | Why it is prohibited |
| --- | --- |
| Mount the host Docker socket (`/var/run/docker.sock`) | Equivalent to host root: the guest can start a privileged container that mounts the host filesystem, and it can see every other container, including the broker and the Omnigent control plane. |
| Mount the host rootless-Podman socket | Same class of control for that user's containers, plus read/write access to their container storage and any volume, including the pilot's own state directory. |
| `docker:dind --privileged` | Not an isolation boundary: the inner daemon can remount host paths and load kernel modules, and it needs the very capabilities the outer sandbox removed. |
| A dedicated per-user rootless daemon socket mounted into the guest | The daemon lives *outside* the guest sandbox, so the socket re-grants exactly what the cgroup/namespaces took away; its lifetime and storage also outlive the guest turn, so a runaway build can exhaust the host after the broker stopped the guest. |

These verdicts are executable policy, not prose:
`broker/guest_broker/container_policy.py` (tested in
`broker/tests/test_container_policy.py`, and printable with
`python -m guest_broker.cli container-policy`).

```
[DENY       ] host-docker-socket
[DENY       ] host-podman-socket
[DENY       ] privileged-dind
[NOT_ENABLED] rootless-dind-in-sandbox
[DENY       ] per-user-rootless-daemon
[NOT_ENABLED] microvm
```

## Options, assessed

1. **Rootless DinD inside the outer per-user sandbox** - the least-bad
   in-sandbox option, but it needs nested user namespaces, a userspace storage
   driver (`fuse-overlayfs`/`vfs`), a second resource budget, and it enlarges the
   kernel attack surface for every guest turn. Verdict: documented, not enabled.
2. **Dedicated per-user rootless daemon with broker restrictions** - rejected as
   a socket mount (see above). The only survivable variant would put the daemon
   *inside* the guest boundary, which collapses back into option 1.
3. **Per-guest microVM (recommended if the need becomes real)** - arbitrary
   container use stays inside a real machine boundary: own kernel, own cgroup
   budget, no host sockets, no host filesystem shares beyond the single
   workspace. Requirements: KVM available to the broker user, per-VM memory/CPU
   ceiling, teardown after every turn. Verdict: `ALLOW` only when
   `/dev/kvm` exists **and** the operator opts in; otherwise `NOT_ENABLED`.

## Gate conditions if this is ever revisited

Before enabling anything, all of these must hold and be recorded:

- [ ] outer runtime is Podman or a microVM (never bwrap-only);
- [ ] nested user namespaces proven on the target host;
- [ ] inner runtime has its own memory/PID budget inside the outer cgroup;
- [ ] storage driver is `fuse-overlayfs` or `vfs`, with a disk budget distinct
      from the guest workspace quota;
- [ ] no host container runtime socket, no host network bridge, no host device;
- [ ] broker-side timeout kills the whole process tree, and the VM/container is
      torn down after the turn;
- [ ] the pilot owner has approved the enlarged attack surface in writing.

Until then: guests get a shell, a workspace, a fixed model, and no container
engine.
