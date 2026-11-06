"""Server-enforced ``user_id -> one guest session`` registry.

This module is the single authority that answers "which session speaks for this
person". Properties the tests pin down:

1. **Exactly one session per user.** ``session_id`` is a scalar, not a list.
2. **No arbitrary session ids.** The transport and the guest never supply a
   session id; only this file, owned by the operator, does.
3. **No cross-user control.** The same ``session_id`` cannot be bound to two
   users, and a user's workspace must be ``<guests_root>/<user_id>/workspace``.
4. **Immutable at runtime.** Inbound traffic cannot rebind a session; binding
   happens through an explicit operator command with an audit record.
5. **Fail closed.** A malformed registry raises instead of degrading.
"""

from __future__ import annotations

import json
import os
import re
import stat
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .errors import RegistryError

_SESSION_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{7,63}$")
_USER_ID = re.compile(r"^[a-z0-9][a-z0-9_-]{1,31}$")


@dataclass(frozen=True)
class GuestBinding:
    user_id: str
    session_id: str
    workspace: Path
    enabled: bool = True
    label: str = ""


class UserRegistry:
    """Read-mostly registry loaded from an operator-owned file."""

    def __init__(self, bindings: dict[str, GuestBinding], guests_root: Path, source: str) -> None:
        self._bindings = dict(bindings)
        self.guests_root = guests_root
        self.source = source
        self._by_session: dict[str, str] = {}
        for user_id, binding in self._bindings.items():
            if binding.session_id in self._by_session:
                raise RegistryError(
                    f"session {binding.session_id!r} is bound to both "
                    f"{self._by_session[binding.session_id]!r} and {user_id!r}"
                )
            self._by_session[binding.session_id] = user_id

    # -- queries ---------------------------------------------------------
    def __len__(self) -> int:
        return len(self._bindings)

    @property
    def user_ids(self) -> tuple[str, ...]:
        return tuple(sorted(self._bindings))

    def get(self, user_id: str) -> GuestBinding:
        binding = self._bindings.get(user_id)
        if binding is None:
            raise RegistryError(f"user {user_id!r} has no guest session binding")
        if not binding.enabled:
            raise RegistryError(f"user {user_id!r} is disabled in the registry")
        return binding

    def session_owner(self, session_id: str) -> str | None:
        return self._by_session.get(session_id)

    def public_view(self) -> list[dict[str, object]]:
        return [
            {
                "user_id": b.user_id,
                "session_id": b.session_id,
                "workspace": str(b.workspace),
                "enabled": b.enabled,
            }
            for b in sorted(self._bindings.values(), key=lambda b: b.user_id)
        ]

    def with_binding(self, binding: GuestBinding) -> "UserRegistry":
        """Provisioning helper: return a new registry with one binding set."""
        validate_binding(binding, self.guests_root)
        updated = dict(self._bindings)
        updated[binding.user_id] = binding
        return UserRegistry(updated, self.guests_root, self.source)


def validate_binding(binding: GuestBinding, guests_root: Path) -> None:
    if not _USER_ID.fullmatch(binding.user_id):
        raise RegistryError(f"invalid user_id {binding.user_id!r}")
    if not _SESSION_ID.fullmatch(binding.session_id):
        raise RegistryError(
            f"invalid session_id {binding.session_id!r}: expected an opaque id matching "
            f"{_SESSION_ID.pattern}"
        )
    guests_root = guests_root.resolve()
    expected = guests_root / binding.user_id / "workspace"
    workspace = binding.workspace
    if workspace.is_symlink():
        raise RegistryError(f"workspace {workspace} must not be a symlink")
    resolved = workspace.resolve() if workspace.exists() else workspace
    if resolved != expected:
        raise RegistryError(
            f"workspace for {binding.user_id!r} must be exactly {expected}, got {resolved}"
        )
    if guests_root not in resolved.parents and resolved != guests_root:
        raise RegistryError(f"workspace {resolved} escapes guests_root {guests_root}")


def load_registry(path: str | Path, guests_root: Path) -> UserRegistry:
    path = Path(path)
    if not path.is_file():
        raise RegistryError(f"registry file not found: {path}")
    mode = stat.S_IMODE(path.stat().st_mode)
    if mode & 0o077:
        raise RegistryError(f"registry {path} is group/world accessible (mode {mode:04o}); chmod 600 it")
    import yaml

    try:
        raw: Any = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    except yaml.YAMLError as exc:
        raise RegistryError(f"registry {path} is not valid YAML: {exc}") from exc
    if not isinstance(raw, dict) or raw.get("version") != 1:
        raise RegistryError("registry: expected a mapping with version: 1")
    users = raw.get("users")
    if not isinstance(users, dict) or not users:
        raise RegistryError("registry: 'users' must be a non-empty mapping")

    bindings: dict[str, GuestBinding] = {}
    for user_id, entry in users.items():
        if not isinstance(user_id, str) or not isinstance(entry, dict):
            raise RegistryError(f"registry.users[{user_id!r}]: expected an object")
        unknown = set(entry) - {"session_id", "workspace", "enabled", "label"}
        if unknown:
            raise RegistryError(f"registry.users[{user_id!r}]: unknown key(s) {sorted(unknown)}")
        if not isinstance(entry.get("session_id"), str) or not isinstance(entry.get("workspace"), str):
            raise RegistryError(f"registry.users[{user_id!r}]: session_id and workspace are required strings")
        binding = GuestBinding(
            user_id=user_id,
            session_id=entry["session_id"],
            workspace=Path(entry["workspace"]),
            enabled=bool(entry.get("enabled", True)),
            label=str(entry.get("label", "")),
        )
        validate_binding(binding, guests_root)
        if not binding.workspace.is_dir():
            raise RegistryError(f"registry.users[{user_id!r}]: workspace {binding.workspace} does not exist")
        bindings[user_id] = binding
    return UserRegistry(bindings, guests_root.resolve(), source=str(path))


def provision(
    path: str | Path,
    guests_root: Path,
    user_id: str,
    session_id: str,
    *,
    force: bool = False,
) -> GuestBinding:
    """Operator-only provisioning of the one binding for ``user_id``.

    This is the *only* supported way a session id enters the system. It refuses
    to run when the registry file is missing, requires an already-created
    workspace, and (unless ``force``) refuses to overwrite an existing binding.
    """
    path = Path(path)
    workspace = guests_root.resolve() / user_id / "workspace"
    if not workspace.is_dir():
        raise RegistryError(f"create the guest workspace first: {workspace}")
    binding = GuestBinding(user_id=user_id, session_id=session_id, workspace=workspace, enabled=True)

    existing: dict[str, Any] = {"version": 1, "users": {}}
    if path.is_file():
        import yaml

        existing = yaml.safe_load(path.read_text(encoding="utf-8")) or existing
    users = existing.setdefault("users", {})
    if user_id in users and not force:
        raise RegistryError(
            f"user {user_id!r} is already bound to {users[user_id].get('session_id')!r}; "
            "refusing to rebind without --force"
        )
    validate_binding(binding, guests_root)
    users[user_id] = {
        "session_id": session_id,
        "workspace": str(workspace),
        "enabled": True,
    }
    import yaml

    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        yaml.safe_dump(existing, handle, sort_keys=True)
    os.replace(tmp, path)
    os.chmod(path, 0o600)
    return binding


def append_audit(audit_path: Path, event: dict[str, Any]) -> None:
    """Append one JSONL audit record. Contains no message text and no secrets."""
    audit_path.parent.mkdir(parents=True, exist_ok=True)
    record = {"ts": time.time(), **event}
    fd = os.open(audit_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    with os.fdopen(fd, "a", encoding="utf-8") as handle:
        handle.write(json.dumps(record, sort_keys=True) + "\n")
