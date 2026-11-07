"""Runtime adapter: the only module that talks to the orchestrator.

Phase 0 defines the interface; Phase 1 ports the bridge's Omnigent calls
behind ``OmnigentRuntime`` (docs/bridge-reuse.md).

Terms conditions (docs/terms-check.md section 1) this module must keep:
* drive only the user's installed, unmodified ``claude`` CLI via Omnigent;
* never read, store, copy or transmit Claude OAuth/session tokens, and never
  open ``~/.claude.json`` wholesale (MCP changes go through ``claude mcp``);
* never run one user's credentials on behalf of another person.
"""

from __future__ import annotations

import shutil
import subprocess
from dataclasses import dataclass
from typing import Protocol


@dataclass(frozen=True)
class SessionRef:
    session_id: str
    role: str


class Runtime(Protocol):
    def ensure_session(self, role: str, folder: str, model: str, effort: str) -> SessionRef: ...
    def send(self, ref: SessionRef, text: str) -> None: ...
    def items_after(self, ref: SessionRef, cursor: str | None) -> list[dict]: ...
    def interrupt(self, ref: SessionRef) -> None: ...


class OmnigentRuntime:
    """Placeholder; implemented in Phase 1 from the bridge's omni_request code."""

    def __init__(self, server_url: str) -> None:
        self.server_url = server_url

    def ensure_session(self, role, folder, model, effort):  # pragma: no cover - Phase 1
        raise NotImplementedError("Phase 1: port from the Telegram bridge")

    send = items_after = interrupt = ensure_session


def claude_cli_status(claude: str = "claude", timeout: float = 20.0) -> tuple[bool, str]:
    """Is the user's own Claude Code CLI installed? (No credential is read.)

    Only ``claude --version`` is run. Whether the user is logged in is left
    to the CLI itself on first use; Kurisu never inspects its auth files.
    """
    exe = shutil.which(claude)
    if not exe:
        return False, "claude CLI not found on PATH; install Claude Code and run `claude login` yourself"
    try:
        out = subprocess.run([exe, "--version"], capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.TimeoutExpired) as exc:
        return False, f"claude --version failed: {exc}"
    return out.returncode == 0, (out.stdout or out.stderr).strip()
