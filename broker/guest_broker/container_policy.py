"""Policy for "the guest wants to build/run containers itself".

Some guest tasks legitimately need a container runtime. The dangerous shortcuts
are prohibited outright, because each one hands the guest control of the host:

* mounting the host Docker socket (``/var/run/docker.sock``) - full host root
  equivalent: create privileged containers, mount ``/``, read every secret;
* mounting the host rootless-Podman socket - same class of control for that
  user's containers, plus read access to that user's container storage;
* ``docker:dind --privileged`` - the daemon owns the host kernel's namespaces and
  can be used to escape.

This module is pure decision logic: it never starts a daemon, never touches a
socket, and never changes host configuration. It exists so the operator gets a
consistent, testable answer instead of an ad-hoc exception.

Verdicts:

``DENY``          prohibited, no configuration makes it acceptable.
``NOT_ENABLED``   technically sound design, deliberately off in pilot v1.
``ALLOW``         only for "put the guest inside its own machine" (microVM).

See docs/GUEST-CONTAINERS.md for the full trade-off discussion.
"""

from __future__ import annotations

from dataclasses import dataclass, field

#: Ways a guest could be given container access.
REQUESTS = (
    "host-docker-socket",
    "host-podman-socket",
    "privileged-dind",
    "rootless-dind-in-sandbox",
    "per-user-rootless-daemon",
    "microvm",
)

DENY = "DENY"
NOT_ENABLED = "NOT_ENABLED"
ALLOW = "ALLOW"


@dataclass(frozen=True)
class Decision:
    request: str
    verdict: str
    reasons: tuple[str, ...] = ()
    requirements: tuple[str, ...] = ()

    @property
    def allowed(self) -> bool:
        return self.verdict == ALLOW


@dataclass(frozen=True)
class RequestContext:
    """Facts the decision depends on, so tests can vary them explicitly."""

    outer_sandbox_mode: str = "systemd-bwrap"
    host_has_kvm: bool = False
    operator_opt_in: bool = False
    userns_nesting_available: bool = False
    dedicated_budget: bool = False
    extra: dict[str, str] = field(default_factory=dict)


_DECISIONS = {
    "host-docker-socket": Decision(
        request="host-docker-socket",
        verdict=DENY,
        reasons=(
            "a mounted Docker socket is equivalent to host root: the guest can start a "
            "privileged container that mounts the host filesystem",
            "it also exposes every other container on the host, including the broker and "
            "the Omnigent control plane",
        ),
    ),
    "host-podman-socket": Decision(
        request="host-podman-socket",
        verdict=DENY,
        reasons=(
            "a mounted rootless-Podman socket lets the guest create containers with any "
            "volume, including the operator's home and this pilot's state directory",
            "it exposes all other containers and images owned by that user",
        ),
    ),
    "privileged-dind": Decision(
        request="privileged-dind",
        verdict=DENY,
        reasons=(
            "--privileged Docker-in-Docker is not an isolation boundary: the inner daemon "
            "can remount host paths and load kernel modules",
            "it requires capabilities that also defeat the outer sandbox",
        ),
    ),
    "rootless-dind-in-sandbox": Decision(
        request="rootless-dind-in-sandbox",
        verdict=NOT_ENABLED,
        reasons=(
            "the only technically defensible in-sandbox option, but it needs nested user "
            "namespaces, a userspace storage driver (fuse-overlayfs/vfs) and a second "
            "resource budget, and it materially enlarges the kernel attack surface",
            "not enabled in pilot v1: the pilot has no guest requirement that justifies it yet",
        ),
        requirements=(
            "outer sandbox is podman or a microVM (never plain bwrap-only)",
            "nested user namespaces proven available on the host",
            "storage driver is fuse-overlayfs or vfs, with a disk budget distinct from the "
            "guest workspace quota",
            "the inner runtime gets its own memory/PID budget inside the outer cgroup",
            "explicit operator opt-in, recorded, with the pilot owner's approval",
        ),
    ),
    "per-user-rootless-daemon": Decision(
        request="per-user-rootless-daemon",
        verdict=DENY,
        reasons=(
            "a daemon outside the guest sandbox is a privilege boundary crossing: the socket "
            "would grant capabilities the guest's cgroup and namespaces deliberately removed",
            "the daemon's lifetime and storage would outlive the guest turn, so a runaway "
            "build could exhaust the host after the broker stopped the guest",
        ),
        requirements=(
            "only reconsider together with a VM boundary, never as a socket mount",
        ),
    ),
    "microvm": Decision(
        request="microvm",
        verdict=ALLOW,
        reasons=(
            "a per-guest microVM keeps arbitrary container use inside a real machine boundary: "
            "its own kernel, its own cgroup budget, no host sockets",
        ),
        requirements=(
            "KVM available to the broker user and a per-VM memory/CPU ceiling",
            "no host filesystem shares beyond the single guest workspace",
            "no host container runtime socket and no host network bridge",
            "broker-side timeout plus VM teardown after every turn",
        ),
    ),
}


def evaluate(request: str, context: RequestContext | None = None) -> Decision:
    """Return the policy decision for one way of granting container access."""
    context = context or RequestContext()
    base = _DECISIONS.get(request)
    if base is None:
        raise KeyError(f"unknown container capability request {request!r}; known: {REQUESTS}")

    if base.verdict != ALLOW:
        return base

    # ALLOW is conditional: re-check the facts instead of trusting the label.
    reasons = list(base.reasons)
    if request == "microvm":
        if not context.host_has_kvm:
            return Decision(
                request=request,
                verdict=NOT_ENABLED,
                reasons=tuple(reasons) + ("the host does not expose /dev/kvm to the broker user",),
                requirements=base.requirements,
            )
        if not context.operator_opt_in:
            return Decision(
                request=request,
                verdict=NOT_ENABLED,
                reasons=tuple(reasons) + ("operator opt-in is required before a VM runtime is offered",),
                requirements=base.requirements,
            )
    return Decision(request=request, verdict=base.verdict, reasons=tuple(reasons), requirements=base.requirements)


def evaluate_all(context: RequestContext | None = None) -> tuple[Decision, ...]:
    return tuple(evaluate(request, context) for request in REQUESTS)


def allowed_requests(context: RequestContext | None = None) -> tuple[str, ...]:
    return tuple(decision.request for decision in evaluate_all(context) if decision.allowed)


def summary(context: RequestContext | None = None) -> str:
    lines = []
    for decision in evaluate_all(context):
        lines.append(f"[{decision.verdict:11}] {decision.request}")
        for reason in decision.reasons:
            lines.append(f"              - {reason}")
    return "\n".join(lines)
