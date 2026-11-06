"""Guest session backends.

The broker owns exactly one backend instance for the whole pilot; the guest and
the transport can never choose or swap it.

``pi-cli``  -- the real runtime for the pilot: Pi, fixed DeepSeek model, inside
               the sandbox, with a per-user Pi config that lists *only* that
               model so no other model can be selected even by hand.
``omnigent``-- the Omnigent-session runtime. Deliberately fail-closed: creating
               a guest session needs operator credentials that the broker (not
               the guest, not the transport) holds. See docs/REMAINING-STEPS.md.
``echo``    -- test/dev backend. Refuses to run unless the operator passes
               ``--allow-echo`` so it can never be the accidental production path.
"""

from __future__ import annotations

import json
import shutil
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol, Sequence

from . import sandbox as sandbox_mod
from .config import Limits, RuntimeConfig, SandboxConfig
from .errors import SessionBackendError
from .registry import GuestBinding

GUEST_SYSTEM_PROMPT = """\
You are the isolated guest assistant for a single pilot user.

Constraints you must respect at all times:
- Your work area is the current directory and nothing else. Do not read, write,
  or probe any path outside it; do not look for host configuration, credentials,
  other users' files, or service control planes.
- You have no model selection. Your model is fixed by the operator; do not ask
  for, attempt to change, or claim to change it.
- You have no messaging, session-management, or administrative tools. Do not ask
  for operator credentials or try to discover them.
- Network access is limited to the language model endpoint. Do not attempt other
  hosts, ports, or protocols.
- Keep answers short enough to be read in a chat message.

If a request needs anything outside these limits, say plainly what is blocked and
what the operator would have to do. Never pretend a blocked action succeeded.
"""

_MODEL_TEMPLATE = {
    "$schema": "https://pi.dev/schemas/models.schema.json",
    "providers": {
        "ollama": {
            "name": "Ollama (pilot fixed model)",
            "baseUrl": "http://127.0.0.1:11434/v1",
            "api": "openai-completions",
            "apiKey": "ollama",
            "models": [],
        }
    },
}


@dataclass(frozen=True)
class BackendResult:
    text: str
    exit_code: int
    duration_seconds: float
    sandboxed: bool
    detail: str = ""


class SessionBackend(Protocol):
    name: str

    def run(
        self,
        binding: GuestBinding,
        message: str,
        *,
        limits: Limits,
        sandbox: SandboxConfig,
        runtime: RuntimeConfig,
        proxy_socket: Path | None,
        guest_io_dir: Path | None,
    ) -> BackendResult: ...


# --------------------------------------------------------------------------
# guest state layout
# --------------------------------------------------------------------------
def guest_state_dir(workspace: Path) -> Path:
    return workspace / ".agent-state"


def prepare_guest_state(workspace: Path, runtime: RuntimeConfig) -> Path:
    """Create the guest's own Pi config with a single fixed model.

    The config lives inside the guest workspace (the only writable path) and
    contains no credentials and no other model ids.
    """
    state = guest_state_dir(workspace)
    agent_dir = state / "pi-agent"
    for sub in (agent_dir, state / "sessions", state / "home", state / "tmp"):
        sub.mkdir(parents=True, exist_ok=True)

    models = json.loads(json.dumps(_MODEL_TEMPLATE))
    models["providers"]["ollama"]["models"] = [
        {
            "id": runtime.model,
            "name": runtime.model,
            "reasoning": True,
            "input": ["text"],
            "contextWindow": 131072,
            "maxTokens": 8192,
        }
    ]
    models_path = agent_dir / "models.json"
    desired = json.dumps(models, indent=2, sort_keys=True)
    if not models_path.is_file() or models_path.read_text(encoding="utf-8") != desired:
        models_path.write_text(desired, encoding="utf-8")

    settings_path = agent_dir / "settings.json"
    settings = {
        # No telemetry, no catalog refresh, no self-update from the guest.
        "telemetry": False,
        "autoUpdate": False,
    }
    if not settings_path.is_file():
        settings_path.write_text(json.dumps(settings, indent=2, sort_keys=True), encoding="utf-8")
    return agent_dir


# --------------------------------------------------------------------------
# echo backend
# --------------------------------------------------------------------------
class EchoBackend:
    name = "echo"

    def __init__(self) -> None:
        self.calls: list[tuple[str, str]] = []

    def run(
        self,
        binding: GuestBinding,
        message: str,
        *,
        limits: Limits,
        sandbox: SandboxConfig,
        runtime: RuntimeConfig,
        proxy_socket: Path | None,
        guest_io_dir: Path | None,
    ) -> BackendResult:
        self.calls.append((binding.user_id, message))
        preview = message.strip().replace("\n", " ")[:120]
        return BackendResult(
            text=f"[echo:{binding.user_id}] {preview}",
            exit_code=0,
            duration_seconds=0.0,
            sandboxed=False,
            detail="echo backend: no model call, no sandbox",
        )


# --------------------------------------------------------------------------
# pi-cli backend
# --------------------------------------------------------------------------
class PiCliBackend:
    name = "pi-cli"

    def __init__(self, *, timeout_seconds: int = 300) -> None:
        self.timeout_seconds = timeout_seconds

    def build_command(
        self, runtime: RuntimeConfig, binding: GuestBinding, workspace: Path, message: str
    ) -> list[str]:
        """Fixed argv. Every element is operator-controlled except the final one.

        The guest text occupies exactly one argv element, is never shell-parsed,
        and cannot introduce new options because it follows ``--``.
        """
        pi_session_id = f"broker-{binding.session_id}"
        state = guest_state_dir(workspace)
        return [
            runtime.pi_path,
            "--print",
            "--provider", runtime.provider,
            "--model", f"{runtime.provider}/{runtime.model}",
            "--session-id", pi_session_id,
            "--session-dir", str(state / "sessions"),
            "--tools", ",".join(runtime.pi_tools),
            "--append-system-prompt", GUEST_SYSTEM_PROMPT,
            "--",
            message,
        ]

    def build_env(self, runtime: RuntimeConfig, workspace: Path) -> dict[str, str]:
        state = guest_state_dir(workspace)
        return {
            "PATH": "/usr/local/bin:/usr/bin:/bin",
            "HOME": str(state / "home"),
            "TMPDIR": str(state / "tmp"),
            "PI_CODING_AGENT_DIR": str(state / "pi-agent"),
            "PI_CODING_AGENT_SESSION_DIR": str(state / "sessions"),
            "PI_OFFLINE": "1",
            "PI_SKIP_VERSION_CHECK": "1",
            "PI_TELEMETRY": "0",
            "LANG": "C.UTF-8",
            "TZ": "UTC",
            # The guest's only network path: the in-namespace shim.
            "OPENAI_BASE_URL": "http://127.0.0.1:11434/v1",
        }

    def run(
        self,
        binding: GuestBinding,
        message: str,
        *,
        limits: Limits,
        sandbox: SandboxConfig,
        runtime: RuntimeConfig,
        proxy_socket: Path | None,
        guest_io_dir: Path | None,
    ) -> BackendResult:
        workspace = binding.workspace.resolve()
        prepare_guest_state(workspace, runtime)

        extra_ro: list[tuple[str, str]] = []
        if sandbox.model_proxy.enabled:
            if guest_io_dir is None:
                raise SessionBackendError("model proxy enabled but no guest-io directory was provided")
            extra_ro.append((str(guest_io_dir), "/run/guest-io"))

        command = self.build_command(runtime, binding, workspace, message)
        env = self.build_env(runtime, workspace)

        if sandbox.mode == "none":
            raise SessionBackendError(
                "refusing to run the guest without a sandbox; set sandbox.mode to "
                "systemd-bwrap (see docs/SECURITY.md)"
            )

        plan = sandbox_mod.build_plan(
            workspace=workspace,
            limits=limits,
            runtime=runtime,
            sandbox=sandbox,
            command=command,
            env=env,
            extra_ro_binds=extra_ro,
            guest_io_dir=guest_io_dir,
        )
        result = sandbox_mod.run_plan(plan, timeout_seconds=limits.runtime_max_seconds)
        text = (result.stdout or "").strip()
        if result.exit_code != 0 and not text:
            text = (
                "The guest runtime exited before answering "
                f"(exit {result.exit_code}). Operator detail: "
                f"{(result.stderr or '').strip().splitlines()[-1][:200] if result.stderr else 'no stderr'}"
            )
        return BackendResult(
            text=text,
            exit_code=result.exit_code,
            duration_seconds=result.duration_seconds,
            sandboxed=True,
            detail=result.stderr.strip()[:500],
        )


# --------------------------------------------------------------------------
# omnigent backend (fail closed until an operator provisions it)
# --------------------------------------------------------------------------
class OmnigentBackend:
    """Placeholder for the Omnigent-session runtime.

    It fails closed with an actionable message instead of silently falling back
    to an unsandboxed path. Provisioning requires operator credentials that are
    intentionally unavailable to the guest and to the WhatsApp transport.
    """

    name = "omnigent"
    REQUIRED_OPERATOR_STEPS = (
        "set runtime.omnigent.base_url to the local Omnigent host service URL",
        "export the operator API token in the variable named by runtime.omnigent.token_env",
        "create one guest Omnigent session and record its id with "
        "`guest-broker provision --user pilot-001 --session-id <conv_id>`",
        "replace this backend with the HTTP client in docs/REMAINING-STEPS.md step 4",
    )

    def __init__(self, runtime: RuntimeConfig) -> None:
        self.runtime = runtime

    def run(
        self,
        binding: GuestBinding,
        message: str,
        *,
        limits: Limits,
        sandbox: SandboxConfig,
        runtime: RuntimeConfig,
        proxy_socket: Path | None,
        guest_io_dir: Path | None,
    ) -> BackendResult:
        raise SessionBackendError(
            "omnigent backend is not implemented yet; remaining operator steps: "
            + "; ".join(self.REQUIRED_OPERATOR_STEPS)
        )


def build_backend(runtime: RuntimeConfig, *, allow_echo: bool = False) -> SessionBackend:
    if runtime.backend == "pi-cli":
        return PiCliBackend(timeout_seconds=runtime.timeout_seconds)
    if runtime.backend == "omnigent":
        return OmnigentBackend(runtime)
    if runtime.backend == "echo":
        if not allow_echo:
            raise SessionBackendError("echo backend requires --allow-echo (dev only)")
        return EchoBackend()
    raise SessionBackendError(f"unknown runtime backend {runtime.backend!r}")


def which_pi(path: str) -> tuple[bool, str]:
    found = shutil.which(path) or (path if Path(path).is_file() else None)
    return (True, found) if found else (False, f"{path} not found")
