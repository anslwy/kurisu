"""roster.yaml: the team. One lead, named long-lived staff, explicit gates.

Rules enforced here:
* exactly one ``lead``; the user only ever talks to the lead;
* role names are unique slugs; folders are absolute and distinct;
* ``gates`` reference existing roles (e.g. QA must pass before merge);
* ``tier`` is a router tier name; ``sandbox`` is a known profile.
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping

import yaml

from .config import SANDBOX_PROFILES
from .errors import ConfigError
from .router import TIERS

ROLE_RE = re.compile(r"^[a-z][a-z0-9-]{0,31}$")
KINDS = ("lead", "dev", "qa", "admin", "custom")
GATE_EVENTS = ("merge", "push", "deploy", "release")


@dataclass(frozen=True)
class Role:
    name: str
    kind: str
    folder: Path
    tier: str = "normal"
    sandbox: str = "none"
    mcp: tuple[str, ...] = ()
    description: str = ""


@dataclass(frozen=True)
class Gate:
    event: str
    role: str  # who must pass first


@dataclass(frozen=True)
class Roster:
    roles: tuple[Role, ...]
    gates: tuple[Gate, ...] = ()

    @property
    def lead(self) -> Role:
        return next(r for r in self.roles if r.kind == "lead")

    @property
    def staff(self) -> tuple[Role, ...]:
        return tuple(r for r in self.roles if r.kind != "lead")

    def get(self, name: str) -> Role:
        for r in self.roles:
            if r.name == name:
                return r
        raise KeyError(name)

    def gates_for(self, event: str) -> tuple[str, ...]:
        return tuple(g.role for g in self.gates if g.event == event)


def parse_roster(data: Mapping[str, Any], *, base_dir: Path) -> Roster:
    if not isinstance(data, Mapping) or set(data) - {"version", "roles", "gates"}:
        raise ConfigError("roster.yaml: allowed keys are version, roles, gates")
    if data.get("version") != 1:
        raise ConfigError("roster.yaml: version must be 1")
    raw_roles = data.get("roles") or {}
    if not isinstance(raw_roles, Mapping) or not raw_roles:
        raise ConfigError("roster.yaml: roles must be a non-empty mapping")
    roles: list[Role] = []
    folders: dict[Path, str] = {}
    for name, spec in raw_roles.items():
        where = f"roles.{name}"
        if not ROLE_RE.fullmatch(str(name)):
            raise ConfigError(f"{where}: name must match {ROLE_RE.pattern}")
        spec = spec or {}
        if set(spec) - {"kind", "folder", "tier", "sandbox", "mcp", "description"}:
            raise ConfigError(f"{where}: unknown keys {sorted(set(spec) - {'kind','folder','tier','sandbox','mcp','description'})}")
        kind = spec.get("kind", "custom")
        if kind not in KINDS:
            raise ConfigError(f"{where}.kind: must be one of {KINDS}")
        folder_raw = spec.get("folder")
        if not folder_raw:
            raise ConfigError(f"{where}.folder: required")
        folder = Path(os.path.expanduser(str(folder_raw)))
        folder = (folder if folder.is_absolute() else base_dir / folder).resolve()
        if folder in folders and kind != "qa" and raw_roles[folders[folder]].get("kind") != "qa":
            # QA may share a dev's folder read-mostly; two builders may not.
            raise ConfigError(f"{where}.folder: already used by role {folders[folder]!r}")
        folders.setdefault(folder, str(name))
        tier = spec.get("tier", "normal")
        if tier not in TIERS:
            raise ConfigError(f"{where}.tier: must be one of {TIERS}")
        sandbox = spec.get("sandbox", "none")
        if sandbox not in SANDBOX_PROFILES:
            raise ConfigError(f"{where}.sandbox: must be one of {SANDBOX_PROFILES}")
        mcp = spec.get("mcp", []) or []
        if not isinstance(mcp, list) or not all(isinstance(m, str) and re.fullmatch(r"[A-Za-z0-9_.-]{1,64}", m) for m in mcp):
            raise ConfigError(f"{where}.mcp: must be a list of MCP server names")
        roles.append(Role(name=str(name), kind=kind, folder=folder, tier=tier, sandbox=sandbox,
                          mcp=tuple(mcp), description=str(spec.get("description", ""))))
    leads = [r for r in roles if r.kind == "lead"]
    if len(leads) != 1:
        raise ConfigError(f"roster.yaml: exactly one role must have kind: lead (found {len(leads)})")
    names = {r.name for r in roles}
    gates: list[Gate] = []
    for event, who in (data.get("gates") or {}).items():
        if event not in GATE_EVENTS:
            raise ConfigError(f"gates.{event}: must be one of {GATE_EVENTS}")
        for role in ([who] if isinstance(who, str) else list(who or [])):
            if role not in names:
                raise ConfigError(f"gates.{event}: unknown role {role!r}")
            if role == leads[0].name:
                raise ConfigError(f"gates.{event}: the lead cannot gate its own delegation")
            gates.append(Gate(event=event, role=role))
    return Roster(roles=tuple(roles), gates=tuple(gates))


def load_roster(path: str | Path) -> Roster:
    path = Path(path)
    return parse_roster(yaml.safe_load(path.read_text(encoding="utf-8")) or {}, base_dir=path.resolve().parent)
