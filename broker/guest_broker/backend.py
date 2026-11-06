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
import os
import re
import shutil
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Protocol, Sequence

from . import sandbox as sandbox_mod
from .config import Limits, RuntimeConfig, SandboxConfig
from .errors import LimitExceededError, SessionBackendError
from .registry import GuestBinding, UserRegistry

#: In-sandbox mount points (must match broker/guest_broker/sandbox.py).
GUEST_IO_DEST = "/run/guest-io"
GUEST_RUNTIME_DEST = "/run/guest-runtime"
MODEL_SHIM_NAME = "model-shim.py"
MODEL_SHIM_LISTEN = "127.0.0.1:11434"

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
    #: "verified" (broker enforced it), "operator-attested" (relayed to a runner the
    #: broker cannot inspect), or "none".
    isolation: str = "unknown"


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
        msg_id: str = "",
    ) -> BackendResult: ...


# --------------------------------------------------------------------------
# guest state layout
# --------------------------------------------------------------------------
def guest_runtime_helpers_dir() -> Path:
    """Directory holding the helper scripts that run inside the sandbox."""
    return Path(__file__).resolve().parents[2] / "guest-runtime"


def guest_state_dir(workspace: Path) -> Path:
    return workspace / ".agent-state"


def contained_state_path(workspace: Path, *parts: str) -> Path:
    """Return workspace/parts, refusing anything that escapes via a symlink.

    ``.agent-state`` lives inside the guest-writable workspace, so a previous
    guest turn can replace it (or ``pi-agent/models.json``) with a symlink to a
    broker-owned file. The broker must never follow that: it would overwrite
    its own registry/allowlist/auth state from outside the sandbox. Fail closed
    and touch nothing.
    """
    base = workspace.resolve(strict=True)
    current = base
    for part in parts:
        current = current / part
        if current.is_symlink():
            raise SessionBackendError(
                f"refusing to use {current}: a guest-writable state path is a symlink "
                "(possible escape attempt); nothing was written"
            )
    target = base.joinpath(*parts)
    if target.exists():
        resolved = target.resolve(strict=True)
        if resolved != base and base not in resolved.parents:
            raise SessionBackendError(
                f"refusing to use {target}: resolves outside the workspace ({resolved})"
            )
    return target


def prepare_guest_state(workspace: Path, runtime: RuntimeConfig) -> Path:
    """Create the guest's own Pi config with a single fixed model.

    The config lives inside the guest workspace (the only writable path) and
    contains no credentials and no other model ids.
    """
    contained_state_path(workspace, ".agent-state")
    agent_dir = contained_state_path(workspace, ".agent-state", "pi-agent")
    for sub in (agent_dir,):
        sub.mkdir(parents=True, exist_ok=True)
    for name in ("sessions", "home", "tmp"):
        contained_state_path(workspace, ".agent-state", name).mkdir(parents=True, exist_ok=True)

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
    models_path = contained_state_path(workspace, ".agent-state", "pi-agent", "models.json")
    desired = json.dumps(models, indent=2, sort_keys=True)
    if models_path.is_symlink():
        raise SessionBackendError(f"refusing to write {models_path}: it is a symlink")
    if not models_path.is_file() or models_path.read_text(encoding="utf-8") != desired:
        models_path.write_text(desired, encoding="utf-8")

    settings_path = contained_state_path(workspace, ".agent-state", "pi-agent", "settings.json")
    if settings_path.is_symlink():
        raise SessionBackendError(f"refusing to write {settings_path}: it is a symlink")
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
        msg_id: str = "",
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
        contained_state_path(workspace, ".agent-state")
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

    def wrap_with_model_shim(self, python_path: str, command: list[str]) -> list[str]:
        """Put the loopback model shim in front of the guest command.

        The guest netns has no listener, so the shim is the only way Pi can reach
        the model: it listens on 127.0.0.1:<port> and forwards over the Unix
        socket the broker's allowlisting proxy owns.
        """
        return [
            python_path,
            f"{GUEST_RUNTIME_DEST}/{MODEL_SHIM_NAME}",
            "--listen", MODEL_SHIM_LISTEN,
            "--upstream-unix", f"{GUEST_IO_DEST}/model-proxy.sock",
            "--",
            *command,
        ]

    def build_env(self, runtime: RuntimeConfig, workspace: Path) -> dict[str, str]:
        state = guest_state_dir(workspace)
        contained_state_path(workspace, ".agent-state")
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
        msg_id: str = "",
    ) -> BackendResult:
        workspace = binding.workspace.resolve()
        prepare_guest_state(workspace, runtime)

        extra_ro: list[tuple[str, str]] = []
        command = self.build_command(runtime, binding, workspace, message)
        if sandbox.model_proxy.enabled:
            if guest_io_dir is None:
                raise SessionBackendError("model proxy enabled but no guest-io directory was provided")
            helpers = guest_runtime_helpers_dir()
            if not (helpers / MODEL_SHIM_NAME).is_file():
                raise SessionBackendError(f"model shim not found under {helpers}")
            extra_ro.append((str(helpers), GUEST_RUNTIME_DEST))
            extra_ro.append((str(guest_io_dir), GUEST_IO_DEST))
            command = self.wrap_with_model_shim(sandbox.python_path, command)
        env = self.build_env(runtime, workspace)
        env.update(dict(sandbox.extra_env))

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
        result = sandbox_mod.run_plan(
            plan,
            timeout_seconds=limits.runtime_max_seconds,
            disk_quota_bytes=limits.disk_quota_bytes,
            workspace=workspace,
        )
        if result.limit_reason == "disk":
            raise LimitExceededError(
                f"guest workspace exceeded its {limits.disk_quota_bytes} byte budget; "
                "the turn was terminated"
            )
        if result.limit_reason == "timeout":
            raise LimitExceededError(
                f"guest turn exceeded {limits.runtime_max_seconds}s; the turn was terminated"
            )
        text = (result.stdout or "").strip()
        if result.exit_code != 0 and not text:
            # Never echo guest stderr to WhatsApp: it can contain host paths,
            # tokens, or model output fragments. The guest gets a generic
            # message; the operator gets a bounded, redacted detail.
            text = "The assistant could not complete that request. Please try again."
        detail = _operator_detail(result.stderr)
        return BackendResult(
            text=text,
            exit_code=result.exit_code,
            duration_seconds=result.duration_seconds,
            sandboxed=True,
            detail=detail if result.exit_code != 0 else "",
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
        msg_id: str = "",
    ) -> BackendResult:
        raise SessionBackendError(
            "omnigent backend is not implemented yet; remaining operator steps: "
            + "; ".join(self.REQUIRED_OPERATOR_STEPS)
        )


_TOKEN_PATTERN = re.compile(r"(ya29\.[A-Za-z0-9._-]+|1//[A-Za-z0-9._-]+|Bearer\s+[A-Za-z0-9._-]+)")


def _operator_detail(stderr: str, limit: int = 200) -> str:
    """Bounded, redacted stderr for the audit log only."""
    text = (stderr or "").strip()
    if not text:
        return ""
    text = _TOKEN_PATTERN.sub("<redacted>", text)
    last = text.splitlines()[-1] if text.splitlines() else text
    return last[:limit]


def build_backend(
    runtime: RuntimeConfig,
    registry: "UserRegistry | None" = None,
    *,
    allow_echo: bool = False,
    environ: Mapping[str, str] | None = None,
) -> SessionBackend:
    """Build the single backend for the pilot.

    ``omnigent`` (default) needs the operator token from the environment and the
    one provisioned session id from the registry. ``pi-cli`` is the development
    shortcut and is refused unless config opted in; ``echo`` needs an explicit
    dev flag.
    """
    environ = os.environ if environ is None else environ
    if runtime.backend == "omnigent":
        from .omnigent_backend import RelayStore, OmnigentBackend
        from .omnigent_api import OmnigentAPI

        if registry is None:
            raise SessionBackendError("the omnigent backend needs the session registry")
        if len(registry) != 1:
            raise SessionBackendError(
                "the pilot binds exactly one guest session; the registry has "
                f"{len(registry)} users, so the omnigent backend refuses to choose"
            )
        binding = registry.get(registry.user_ids[0])
        token = environ.get(runtime.omnigent_token_env, "")
        if not token:
            raise SessionBackendError(
                f"operator API token is not set in ${runtime.omnigent_token_env} (broker-only)"
            )
        api = OmnigentAPI(
            runtime.omnigent_base_url,
            token,
            session_id=binding.session_id,
        )
        return OmnigentBackend(
            api,
            RelayStore(runtime.relay_state_path),
            poll_interval=runtime.omnigent_poll_interval,
            turn_timeout=float(runtime.omnigent_turn_timeout),
        )
    if runtime.backend == "pi-cli":
        if not runtime.allow_local_pi:
            raise SessionBackendError(
                "pi-cli is development-only; the PoC relays through the Omnigent API "
                "(see docs/ARCHITECTURE.md)"
            )
        return PiCliBackend(timeout_seconds=runtime.timeout_seconds)
    if runtime.backend == "echo":
        if not allow_echo:
            raise SessionBackendError("echo backend requires --allow-echo (dev only)")
        return EchoBackend()
    raise SessionBackendError(f"unknown runtime backend {runtime.backend!r}")


def which_pi(path: str) -> tuple[bool, str]:
    found = shutil.which(path) or (path if Path(path).is_file() else None)
    return (True, found) if found else (False, f"{path} not found")
