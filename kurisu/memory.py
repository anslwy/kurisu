"""Durable memory: AGENTS.md (shared rules), per-role notes, atomic state.

Restart identity (pattern from legacy/guest-hosting instance.py): each role's
session id is recorded once and reused across restarts; a recorded id that
disappears is an error to investigate, never a silent new session.
"""

from __future__ import annotations

import json
import os
import stat
from pathlib import Path
from typing import Any

from .errors import KurisuError


class StateStore:
    """Small JSON state file, written atomically with 0600 permissions."""

    def __init__(self, path: Path) -> None:
        self.path = Path(path)

    def load(self) -> dict[str, Any]:
        if not self.path.exists():
            return {}
        mode = stat.S_IMODE(self.path.stat().st_mode)
        if mode & 0o077:
            raise KurisuError(f"{self.path} must be 0600 (is {oct(mode)})")
        return json.loads(self.path.read_text(encoding="utf-8"))

    def save(self, data: dict[str, Any]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        tmp = self.path.with_suffix(self.path.suffix + ".tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as fh:
            json.dump(data, fh, indent=2, sort_keys=True)
            fh.write("\n")
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, self.path)


class SessionRegistry:
    """role -> session id, recorded once."""

    def __init__(self, store: StateStore) -> None:
        self.store = store

    def get(self, role: str) -> str | None:
        return self.store.load().get("sessions", {}).get(role)

    def record(self, role: str, session_id: str) -> None:
        data = self.store.load()
        sessions = data.setdefault("sessions", {})
        if sessions.get(role) not in (None, session_id):
            raise KurisuError(f"role {role!r} already has session {sessions[role]!r}; refusing to replace it")
        sessions[role] = session_id
        self.store.save(data)


def agents_md_path(data_dir: Path) -> Path:
    return Path(data_dir) / "AGENTS.md"
