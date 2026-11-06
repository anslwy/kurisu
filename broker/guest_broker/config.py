"""Strict configuration loading for the trusted broker.

Design rules enforced here:

* Placeholders (``__REQUIRED__``) must be replaced before the broker will run.
  There is no "development default" for anything security relevant.
* ``${ENV_VAR}`` indirection is the only way secrets enter the process, so no
  credential ever has to live in a file that might be committed.
* Unknown keys are rejected: a typo in a limit name must not silently disable
  the limit.
* :func:`load_config` never starts processes and never touches the network.
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping

from .errors import ConfigError

PLACEHOLDER = "__REQUIRED__"
_ENV_PATTERN = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}")

#: Sandbox modes that actually isolate. ``none`` exists only so that unit tests
#: can exercise pure logic; the CLI refuses it for a real run.
SANDBOX_MODES = ("systemd-bwrap", "podman", "bwrap", "none")
#: Modes that provide the full guarantee set (cgroup memory/CPU/tasks/runtime).
#: ``bwrap`` alone has no cgroup accounting, so it is dev-only: it can be used
#: with `preflight --allow-no-sandbox` but never for a real `run`.
REAL_SANDBOX_MODES = ("systemd-bwrap", "podman")
DEV_SANDBOX_MODES = ("bwrap",)
#: Preferred order for a production deployment on the host itself.
PREFERRED_SANDBOX_MODES = ("podman", "systemd-bwrap")


def _expand(value: Any) -> Any:
    if isinstance(value, str):
        def repl(match: re.Match[str]) -> str:
            name = match.group(1)
            got = os.environ.get(name)
            if got is None:
                raise ConfigError(f"environment variable {name!r} referenced by config is unset")
            return got

        return _ENV_PATTERN.sub(repl, value)
    if isinstance(value, list):
        return [_expand(v) for v in value]
    if isinstance(value, dict):
        return {k: _expand(v) for k, v in value.items()}
    return value


def find_placeholders(value: Any, path: str = "") -> list[str]:
    """Return dotted paths of every unresolved placeholder."""
    found: list[str] = []
    if isinstance(value, str):
        if value == PLACEHOLDER or PLACEHOLDER in value:
            found.append(path or "<root>")
    elif isinstance(value, list):
        for i, item in enumerate(value):
            found.extend(find_placeholders(item, f"{path}[{i}]"))
    elif isinstance(value, dict):
        for key, item in value.items():
            found.extend(find_placeholders(item, f"{path}.{key}" if path else str(key)))
    return found


def _require_keys(data: Mapping[str, Any], allowed: set[str], where: str) -> None:
    unknown = set(data) - allowed
    if unknown:
        raise ConfigError(f"{where}: unknown key(s) {sorted(unknown)}; allowed: {sorted(allowed)}")


def _as_int(value: Any, where: str, *, minimum: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise ConfigError(f"{where}: expected integer, got {type(value).__name__}")
    if value < minimum:
        raise ConfigError(f"{where}: must be >= {minimum}, got {value}")
    return value


def _as_str(value: Any, where: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ConfigError(f"{where}: expected non-empty string")
    return value


def _as_bool(value: Any, where: str) -> bool:
    if not isinstance(value, bool):
        raise ConfigError(f"{where}: expected boolean")
    return value


def parse_size(value: Any, where: str, *, allow_zero: bool = False) -> int:
    """Parse ``512M`` / ``2G`` / plain bytes into bytes."""
    if isinstance(value, int) and not isinstance(value, bool):
        if value < 0 or (value == 0 and not allow_zero):
            raise ConfigError(f"{where}: size must be positive")
        return value
    if not isinstance(value, str):
        raise ConfigError(f"{where}: expected size string like '2G'")
    text = value.strip().upper()
    match = re.fullmatch(r"(\d+)([KMGT]?)", text)
    if not match:
        raise ConfigError(f"{where}: invalid size {value!r}")
    number = int(match.group(1))
    unit = match.group(2)
    factor = {"": 1, "K": 1024, "M": 1024**2, "G": 1024**3, "T": 1024**4}[unit]
    total = number * factor
    if total < 0 or (total == 0 and not allow_zero):
        raise ConfigError(f"{where}: size must be positive")
    return total


@dataclass(frozen=True)
class Limits:
    """Resource ceilings applied by cgroup v2 + rlimits + watchdog.

    ``memory``/``cpu_quota``/``tasks``/``runtime``/``nofile``/``io_weight`` are
    enforced by the kernel through the transient systemd scope. ``file_size``
    is an rlimit. ``disk_quota_bytes`` is enforced cooperatively by the broker
    watchdog (see docs/SECURITY.md for why exFAT/SD storage has no fs quota).
    """

    memory_bytes: int
    memory_swap_bytes: int
    cpu_quota: str
    tasks_max: int
    runtime_max_seconds: int
    file_size_bytes: int
    tmpfs_bytes: int
    nofile: int
    io_weight: int
    disk_quota_bytes: int

    @staticmethod
    def from_dict(data: Mapping[str, Any]) -> "Limits":
        allowed = {
            "memory_max", "memory_swap_max", "cpu_quota", "tasks_max",
            "runtime_max_sec", "file_size_max", "tmpfs_size", "nofile",
            "io_weight", "disk_quota",
        }
        _require_keys(data, allowed, "limits")
        missing = allowed - set(data)
        if missing:
            raise ConfigError(f"limits: missing required key(s) {sorted(missing)}")
        cpu_quota = _as_str(data["cpu_quota"], "limits.cpu_quota")
        if not re.fullmatch(r"\d{1,4}%", cpu_quota):
            raise ConfigError("limits.cpu_quota: expected percentage such as '150%'")
        io_weight = _as_int(data["io_weight"], "limits.io_weight")
        if not 1 <= io_weight <= 10000:
            raise ConfigError("limits.io_weight: must be within 1..10000")
        return Limits(
            memory_bytes=parse_size(data["memory_max"], "limits.memory_max"),
            memory_swap_bytes=parse_size(data["memory_swap_max"], "limits.memory_swap_max", allow_zero=True),
            cpu_quota=cpu_quota,
            tasks_max=_as_int(data["tasks_max"], "limits.tasks_max", minimum=8),
            runtime_max_seconds=_as_int(data["runtime_max_sec"], "limits.runtime_max_sec", minimum=5),
            file_size_bytes=parse_size(data["file_size_max"], "limits.file_size_max"),
            tmpfs_bytes=parse_size(data["tmpfs_size"], "limits.tmpfs_size"),
            nofile=_as_int(data["nofile"], "limits.nofile", minimum=64),
            io_weight=io_weight,
            disk_quota_bytes=parse_size(data["disk_quota"], "limits.disk_quota"),
        )


@dataclass(frozen=True)
class ModelProxyConfig:
    enabled: bool
    upstream: str
    allowed_paths: tuple[str, ...] = ("/v1/chat/completions", "/v1/completions")

    @staticmethod
    def from_dict(data: Mapping[str, Any]) -> "ModelProxyConfig":
        _require_keys(data, {"enabled", "upstream", "allowed_paths", "allowed_path_prefixes"}, "sandbox.model_proxy")
        enabled = _as_bool(data.get("enabled", True), "sandbox.model_proxy.enabled")
        upstream = _as_str(data.get("upstream", ""), "sandbox.model_proxy.upstream")
        if enabled and not upstream.startswith(("http://127.0.0.1", "http://localhost")):
            raise ConfigError("sandbox.model_proxy.upstream: must be a loopback HTTP URL")
        if "allowed_path_prefixes" in data:
            raise ConfigError(
                "sandbox.model_proxy.allowed_path_prefixes is no longer supported: list exact "
                "model endpoints in sandbox.model_proxy.allowed_paths instead"
            )
        paths = tuple(data.get("allowed_paths", ("/v1/chat/completions", "/v1/completions")))
        if not paths or not all(isinstance(p, str) and p.startswith("/") and p != "/" for p in paths):
            raise ConfigError("sandbox.model_proxy.allowed_paths: must be explicit absolute paths")
        for path in paths:
            if path.endswith("/"):
                raise ConfigError(
                    f"sandbox.model_proxy.allowed_paths: {path!r} is a prefix; list exact endpoints"
                )
            if path.rstrip("/").endswith("/models"):
                raise ConfigError(
                    "sandbox.model_proxy.allowed_paths must not expose model listing endpoints"
                )
        return ModelProxyConfig(
            enabled=enabled,
            upstream=upstream,
            allowed_paths=paths,
        )


@dataclass(frozen=True)
class SandboxConfig:
    mode: str
    share_net: bool
    bwrap_path: str
    systemd_run_path: str
    model_proxy: ModelProxyConfig
    read_only_paths: tuple[str, ...] = ()
    extra_env: Mapping[str, str] = field(default_factory=dict)
    #: In-sandbox rlimit helper. Scopes cannot set rlimits on a process they do
    #: not spawn, so file-size/descriptor ceilings are applied here instead.
    rlimit_launcher: str = ""
    python_path: str = "/usr/bin/python3"
    #: Rootless container image for sandbox.mode: podman.
    image: str = ""
    podman_path: str = "podman"
    guest_containers: str = "none"

    @staticmethod
    def from_dict(data: Mapping[str, Any]) -> "SandboxConfig":
        _require_keys(
            data,
            {"mode", "share_net", "bwrap_path", "systemd_run_path", "model_proxy",
             "read_only_paths", "extra_env", "rlimit_launcher", "python_path",
             "image", "podman_path", "guest_containers"},
            "sandbox",
        )
        mode = _as_str(data.get("mode", ""), "sandbox.mode")
        if mode not in SANDBOX_MODES:
            raise ConfigError(f"sandbox.mode: must be one of {SANDBOX_MODES}")
        share_net = _as_bool(data.get("share_net", False), "sandbox.share_net")
        if share_net:
            raise ConfigError(
                "sandbox.share_net: must stay false; the guest reaches only the "
                "allowlisted loopback model proxy (see docs/SECURITY.md)"
            )
        paths = data.get("read_only_paths", [])
        if not isinstance(paths, list) or not all(isinstance(p, str) and p.startswith("/") for p in paths):
            raise ConfigError("sandbox.read_only_paths: must be a list of absolute paths")
        extra_env = data.get("extra_env", {})
        if not isinstance(extra_env, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in extra_env.items()):
            raise ConfigError("sandbox.extra_env: must be a mapping of string to string")
        return SandboxConfig(
            mode=mode,
            share_net=share_net,
            bwrap_path=_as_str(data.get("bwrap_path", "bwrap"), "sandbox.bwrap_path"),
            systemd_run_path=_as_str(data.get("systemd_run_path", "systemd-run"), "sandbox.systemd_run_path"),
            model_proxy=ModelProxyConfig.from_dict(data.get("model_proxy", {})),
            read_only_paths=tuple(paths),
            extra_env=dict(extra_env),
            rlimit_launcher=str(data.get("rlimit_launcher", "") or ""),
            python_path=_as_str(data.get("python_path", "/usr/bin/python3"), "sandbox.python_path"),
            image=str(data.get("image", "") or ""),
            podman_path=_as_str(data.get("podman_path", "podman"), "sandbox.podman_path"),
            guest_containers=str(data.get("guest_containers", "none") or "none"),
        )


@dataclass(frozen=True)
class RuntimeConfig:
    """Fixed guest runtime. There is deliberately no per-request override API."""

    backend: str
    provider: str
    model: str
    pi_path: str
    pi_agent_dir: Path
    pi_session_dir_name: str = "pi-sessions"
    pi_tools: tuple[str, ...] = ("read", "write", "edit", "bash")
    timeout_seconds: int = 300
    omnigent_base_url: str = ""
    omnigent_token_env: str = ""
    omnigent_poll_interval: float = 1.0
    omnigent_turn_timeout: int = 300
    relay_state_path: Path = Path("")
    allow_local_pi: bool = False

    @staticmethod
    def from_dict(data: Mapping[str, Any]) -> "RuntimeConfig":
        allowed = {"backend", "provider", "model", "pi_path", "pi_agent_dir",
                   "pi_session_dir_name", "pi_tools", "timeout_seconds",
                   "omnigent", "allow_local_pi"}
        _require_keys(data, allowed, "runtime")
        # A guest model must be a single, literal value. Reject anything that
        # looks like it could select/cycle models at runtime.
        model = _as_str(data.get("model", ""), "runtime.model")
        if "*" in model or "," in model:
            raise ConfigError("runtime.model: must be exactly one model id (no globs or lists)")
        backend = _as_str(data.get("backend", ""), "runtime.backend")
        if backend not in ("pi-cli", "omnigent", "echo"):
            raise ConfigError("runtime.backend: must be one of omnigent, pi-cli (dev), echo")
        tools = tuple(data.get("pi_tools", ("read", "write", "edit", "bash")))
        if not tools or not all(isinstance(t, str) and re.fullmatch(r"[a-z_]+", t) for t in tools):
            raise ConfigError("runtime.pi_tools: must be a list of plain tool names")
        omni = data.get("omnigent", {}) or {}
        _require_keys(
            omni,
            {"base_url", "token_env", "poll_interval_seconds", "turn_timeout_seconds",
             "relay_state_path"},
            "runtime.omnigent",
        )
        return RuntimeConfig(
            backend=backend,
            provider=_as_str(data.get("provider", ""), "runtime.provider"),
            model=model,
            pi_path=_as_str(data.get("pi_path", ""), "runtime.pi_path"),
            pi_agent_dir=Path(_as_str(data.get("pi_agent_dir", ""), "runtime.pi_agent_dir")),
            pi_session_dir_name=_as_str(data.get("pi_session_dir_name", "pi-sessions"), "runtime.pi_session_dir_name"),
            pi_tools=tools,
            timeout_seconds=_as_int(data.get("timeout_seconds", 300), "runtime.timeout_seconds", minimum=10),
            omnigent_base_url=str(omni.get("base_url", "")),
            omnigent_token_env=str(omni.get("token_env", "")),
            omnigent_poll_interval=float(omni.get("poll_interval_seconds", 1.0) or 1.0),
            omnigent_turn_timeout=_as_int(
                omni.get("turn_timeout_seconds", 300), "runtime.omnigent.turn_timeout_seconds", minimum=5
            ),
            relay_state_path=Path(str(omni.get("relay_state_path", "") or "")),
            allow_local_pi=_as_bool(data.get("allow_local_pi", False), "runtime.allow_local_pi"),
        )


@dataclass(frozen=True)
class AuthConfig:
    """Kurisu sign-in (Google OAuth device flow)."""

    enabled: bool
    google_client_id: str
    google_client_secret_env: str
    mappings_path: Path
    tokens_path: Path
    pending_ttl_seconds: int

    @staticmethod
    def from_dict(data: Mapping[str, Any]) -> "AuthConfig":
        allowed = {"enabled", "google_client_id", "google_client_secret_env",
                   "mappings_path", "tokens_path", "pending_ttl_seconds"}
        _require_keys(data, allowed, "auth")
        enabled = _as_bool(data.get("enabled", False), "auth.enabled")
        client_id = str(data.get("google_client_id", "") or "")
        if enabled and not client_id:
            raise ConfigError("auth.enabled requires auth.google_client_id")
        mappings = str(data.get("mappings_path", "") or "")
        tokens = str(data.get("tokens_path", "") or "")
        if enabled and (not mappings.strip() or not tokens.strip()):
            raise ConfigError("auth.enabled requires auth.mappings_path and auth.tokens_path")
        return AuthConfig(
            enabled=enabled,
            google_client_id=client_id,
            google_client_secret_env=str(data.get("google_client_secret_env", "KURISU_GOOGLE_CLIENT_SECRET") or ""),
            mappings_path=Path(mappings) if mappings else Path(""),
            tokens_path=Path(tokens) if tokens else Path(""),
            pending_ttl_seconds=_as_int(data.get("pending_ttl_seconds", 1800), "auth.pending_ttl_seconds", minimum=60),
        )


@dataclass(frozen=True)
class BrokerSettings:
    socket_path: Path
    state_dir: Path
    audit_path: Path
    guests_root: Path
    registry_path: Path
    allowlist_path: Path
    max_message_chars: int
    max_reply_chars: int
    per_user_rate: int
    per_user_rate_window: int

    @staticmethod
    def from_dict(data: Mapping[str, Any]) -> "BrokerSettings":
        allowed = {"socket_path", "state_dir", "audit_path", "guests_root",
                   "registry_path", "allowlist_path", "max_message_chars",
                   "max_reply_chars", "per_user_rate", "per_user_rate_window"}
        _require_keys(data, allowed, "broker")
        missing = allowed - set(data)
        if missing:
            raise ConfigError(f"broker: missing required key(s) {sorted(missing)}")
        return BrokerSettings(
            socket_path=Path(_as_str(data["socket_path"], "broker.socket_path")),
            state_dir=Path(_as_str(data["state_dir"], "broker.state_dir")),
            audit_path=Path(_as_str(data["audit_path"], "broker.audit_path")),
            guests_root=Path(_as_str(data["guests_root"], "broker.guests_root")),
            registry_path=Path(_as_str(data["registry_path"], "broker.registry_path")),
            allowlist_path=Path(_as_str(data["allowlist_path"], "broker.allowlist_path")),
            max_message_chars=_as_int(data["max_message_chars"], "broker.max_message_chars", minimum=1),
            max_reply_chars=_as_int(data["max_reply_chars"], "broker.max_reply_chars", minimum=1),
            per_user_rate=_as_int(data["per_user_rate"], "broker.per_user_rate", minimum=1),
            per_user_rate_window=_as_int(data["per_user_rate_window"], "broker.per_user_rate_window", minimum=1),
        )


@dataclass(frozen=True)
class Config:
    broker: BrokerSettings
    limits: Limits
    sandbox: SandboxConfig
    runtime: RuntimeConfig
    raw: Mapping[str, Any]
    auth: AuthConfig = field(default_factory=lambda: AuthConfig(
        enabled=False, google_client_id="", google_client_secret_env="KURISU_GOOGLE_CLIENT_SECRET",
        mappings_path=Path(""), tokens_path=Path(""), pending_ttl_seconds=1800,
    ))

    def resolve(self, path: Path) -> Path:
        """Resolve a config-relative path against the config file's directory."""
        return path if path.is_absolute() else (self.base_dir / path).resolve()

    base_dir: Path = Path(".")


def load_config(path: str | Path) -> Config:
    import yaml  # imported lazily so pure-logic tests need no dependency

    path = Path(path)
    if not path.is_file():
        raise ConfigError(f"config not found: {path}")
    try:
        raw = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    except yaml.YAMLError as exc:  # pragma: no cover - malformed input path
        raise ConfigError(f"config {path} is not valid YAML: {exc}") from exc
    if not isinstance(raw, dict):
        raise ConfigError("config root must be a mapping")

    _require_keys(raw, {"version", "broker", "limits", "sandbox", "runtime", "auth"}, "config")
    if raw.get("version") != 1:
        raise ConfigError("config.version: must be 1")

    raw = _expand(raw)
    placeholders = find_placeholders(raw)
    if placeholders:
        raise ConfigError(
            "config still contains placeholders at: " + ", ".join(sorted(placeholders))
            + " (copy the *.example.yaml file and fill in real values)"
        )

    base_dir = path.parent.resolve()
    cfg = Config(
        broker=BrokerSettings.from_dict(raw["broker"]),
        limits=Limits.from_dict(raw["limits"]),
        sandbox=SandboxConfig.from_dict(raw["sandbox"]),
        runtime=RuntimeConfig.from_dict(raw["runtime"]),
        raw=raw,
        base_dir=base_dir,
        auth=AuthConfig.from_dict(raw.get("auth", {}) or {}),
    )

    # Cross-field checks that must never be skippable.
    if cfg.sandbox.model_proxy.enabled and cfg.sandbox.mode == "none":
        raise ConfigError("sandbox.mode 'none' cannot be combined with the model proxy")
    # extra_env must not be able to rewrite the runtime's own wiring.
    protected = {"PATH", "HOME", "TMPDIR", "OPENAI_BASE_URL", "PI_CODING_AGENT_DIR",
                 "PI_CODING_AGENT_SESSION_DIR"}
    for key in cfg.sandbox.extra_env:
        if key in protected or key.startswith("PI_"):
            raise ConfigError(f"sandbox.extra_env: {key!r} may not override the guest runtime wiring")

    # The intended PoC path calls the Omnigent API for the bound session. A local
    # Pi process is a development shortcut only, so it must be opted into
    # explicitly and can never be the accidental production path.
    if cfg.runtime.backend == "pi-cli" and not cfg.runtime.allow_local_pi:
        raise ConfigError(
            "runtime.backend 'pi-cli' is development-only: set runtime.allow_local_pi: true "
            "and see docs/ARCHITECTURE.md (the PoC uses backend: omnigent)"
        )
    if cfg.runtime.backend == "omnigent":
        if not cfg.runtime.omnigent_base_url:
            raise ConfigError("runtime.backend 'omnigent' requires runtime.omnigent.base_url")
        if not cfg.runtime.omnigent_token_env:
            raise ConfigError("runtime.backend 'omnigent' requires runtime.omnigent.token_env")
        if not str(cfg.runtime.relay_state_path):
            raise ConfigError(
                "runtime.backend 'omnigent' requires runtime.omnigent.relay_state_path "
                "(persistent relay cursor/dedup state)"
            )

    # pi-cli runs in an isolated network namespace, so without the model proxy it
    # has no reachable model endpoint at all. Fail closed instead of hanging.
    if cfg.runtime.backend == "pi-cli" and not cfg.sandbox.model_proxy.enabled:
        raise ConfigError(
            "runtime.backend 'pi-cli' requires sandbox.model_proxy.enabled: the guest has no "
            "network namespace route to any model endpoint without the allowlisting proxy"
        )

    # Guest-initiated container use is disabled in pilot v1 and cannot be enabled
    # by editing this file: see docs/GUEST-CONTAINERS.md for the evaluation and
    # the only acceptable path (a per-guest microVM).
    if cfg.sandbox.guest_containers != "none":
        raise ConfigError(
            f"sandbox.guest_containers={cfg.sandbox.guest_containers!r} is not supported. "
            "Host Docker/Podman sockets and privileged DinD are prohibited; a per-guest "
            "microVM is the only acceptable path and is not implemented. See docs/GUEST-CONTAINERS.md."
        )
    if cfg.sandbox.mode == "podman":
        if not cfg.sandbox.image:
            raise ConfigError("sandbox.mode 'podman' requires sandbox.image (a pinned container image)")
        if ":" not in cfg.sandbox.image and "@" not in cfg.sandbox.image:
            raise ConfigError(
                "sandbox.image must be pinned to an explicit tag or digest (no implicit :latest)"
            )
    guests_root = cfg.broker.guests_root
    if not str(guests_root).startswith("/"):
        raise ConfigError("broker.guests_root: must be an absolute path")
    # The broker's own state (registry, allowlist, audit log, socket) must never
    # live inside a guest *workspace*: that is the single writable path the
    # sandbox exposes, so anything there is guest-readable and guest-writable.
    # Other operator directories under guests_root (e.g. the deployment tree
    # guests/<user>/implementation) are fine.
    for label, candidate in (
        ("broker.state_dir", cfg.broker.state_dir),
        ("broker.audit_path", cfg.broker.audit_path),
        ("broker.registry_path", cfg.broker.registry_path),
        ("broker.allowlist_path", cfg.broker.allowlist_path),
        ("broker.socket_path", cfg.broker.socket_path),
    ):
        resolved = cfg.resolve(candidate)
        if guests_root in resolved.parents:
            relative = resolved.relative_to(guests_root)
            if len(relative.parts) >= 2 and relative.parts[1] == "workspace":
                raise ConfigError(
                    f"{label} ({resolved}) is inside a guest workspace; guest-writable trees "
                    "must never contain broker state, the registry, or the allowlist"
                )
    # read_only_paths are mounted into the sandbox by the operator's own config.
    # A typo here (or a copied-in "/") would re-expose what the sandbox removed.
    forbidden_ro = ("/", "/home", "/root", "/proc", "/sys", "/boot", "/run/user",
                    "/run/systemd", "/run/dbus", "/dev")
    sensitive = [
        cfg.resolve(cfg.broker.state_dir),
        cfg.resolve(cfg.broker.registry_path),
        cfg.resolve(cfg.broker.allowlist_path),
    ]
    if cfg.auth.enabled and str(cfg.auth.mappings_path):
        sensitive.append(cfg.resolve(cfg.auth.mappings_path))
    if cfg.auth.enabled and str(cfg.auth.tokens_path):
        sensitive.append(cfg.resolve(cfg.auth.tokens_path))
    for raw in cfg.sandbox.read_only_paths:
        path = Path(raw)
        normalised = str(path).rstrip("/") or "/"
        if normalised in forbidden_ro:
            raise ConfigError(f"sandbox.read_only_paths: {raw!r} must never be exposed to the guest")
        for f in forbidden_ro:
            if f != "/" and (normalised == f or normalised.startswith(f + "/")):
                raise ConfigError(f"sandbox.read_only_paths: {raw!r} is under forbidden host root {f}")
        for blocked in sensitive:
            if normalised == str(blocked) or normalised.startswith(str(blocked) + "/"):
                raise ConfigError(
                    f"sandbox.read_only_paths: {raw!r} would expose broker state/credentials to the guest"
                )
        allowed_ws = cfg.broker.guests_root
        if normalised == str(allowed_ws):
            raise ConfigError(
                "sandbox.read_only_paths: the guests root would expose other guests' workspaces"
            )

    if cfg.auth.enabled:
        for label, p in (("auth.mappings_path", cfg.auth.mappings_path),
                         ("auth.tokens_path", cfg.auth.tokens_path)):
            resolved = cfg.resolve(p)
            if guests_root in resolved.parents:
                relative = resolved.relative_to(guests_root)
                if len(relative.parts) >= 2 and relative.parts[1] == "workspace":
                    raise ConfigError(f"{label} ({resolved}) is inside a guest workspace")
    return cfg
