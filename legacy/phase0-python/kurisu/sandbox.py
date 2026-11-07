"""Per-role sandbox profiles.

``none``    trusted local mode (default): the role runs as the user. Say so
            plainly in docs and `kurisu init`.
``podman``  rootless container with only the role's folder mounted. Phase 2
            ports the plan and widening guard from
            ``legacy/guest-hosting/broker/guest_broker/instance.py``
            (``runner_plan`` / ``assert_runner_plan_safe``).
"""

from __future__ import annotations

PROFILES = {
    "none": "trusted local mode: the role acts with your user's permissions",
    "podman": "rootless Podman: only the role's folder is writable, no host sockets (Phase 2)",
}


def describe(profile: str) -> str:
    try:
        return PROFILES[profile]
    except KeyError:
        raise ValueError(f"unknown sandbox profile {profile!r}") from None
