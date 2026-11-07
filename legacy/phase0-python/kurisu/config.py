"""kurisu.yaml: the one per-install config file.

Validation is strict and fails closed: unknown keys are errors, listeners
must be loopback, secrets are only ever *names* of environment variables, and
the WhatsApp channel needs an explicit risk acknowledgement
(docs/terms-check.md section 3).
"""

from __future__ import annotations

import ipaddress
import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Mapping

import yaml

from .errors import ConfigError

ENV_NAME_RE = re.compile(r"^[A-Z][A-Z0-9_]{1,63}$")
CHANNEL_KINDS = ("telegram", "web", "whatsapp")
SANDBOX_PROFILES = ("none", "podman")


def _keys(data: Mapping[str, Any], allowed: set[str], where: str) -> None:
    if not isinstance(data, Mapping):
        raise ConfigError(f"{where}: must be a mapping")
    unknown = set(data) - allowed
    if unknown:
        raise ConfigError(f"{where}: unknown keys {sorted(unknown)}")


def _env_name(value: Any, where: str) -> str:
    if not isinstance(value, str) or not ENV_NAME_RE.fullmatch(value):
        raise ConfigError(f"{where}: must be the NAME of an environment variable, not a secret value")
    return value


def _loopback(host: Any, where: str) -> str:
    try:
        addr = ipaddress.ip_address(str(host))
    except ValueError:
        raise ConfigError(f"{where}: must be a loopback IP literal (127.0.0.1 or ::1)") from None
    if not addr.is_loopback:
        raise ConfigError(f"{where}: {host} is not loopback; Kurisu exposes no public port by default")
    return str(addr)


def default_data_dir(env: Mapping[str, str] | None = None) -> Path:
    env = os.environ if env is None else env
    base = env.get("XDG_DATA_HOME") or str(Path(env.get("HOME", str(Path.home()))) / ".local" / "share")
    return Path(base) / "kurisu"


@dataclass(frozen=True)
class ChannelConfig:
    kind: str
    enabled: bool
    token_env: str = ""
    owner_id: str = ""
    bind: str = "127.0.0.1"
    port: int = 0
    accept_unofficial_client_risk: bool = False


@dataclass(frozen=True)
class BudgetConfig:
    daily_usd: float = 0.0  # 0 = no API spend allowed (subscription only)
    max_concurrent_staff: int = 2


@dataclass(frozen=True)
class RuntimeConfig:
    kind: str = "omnigent"
    server_url: str = "http://127.0.0.1:6767"


@dataclass(frozen=True)
class KurisuConfig:
    data_dir: Path
    roster_path: Path
    runtime: RuntimeConfig
    channels: tuple[ChannelConfig, ...]
    budget: BudgetConfig
    default_sandbox: str = "none"
    backup_recipient: str = ""
    backup_dir: Path | None = None
    extra: dict[str, Any] = field(default_factory=dict)

    def channel(self, kind: str) -> ChannelConfig | None:
        return next((c for c in self.channels if c.kind == kind), None)


def _channel(kind: str, data: Mapping[str, Any]) -> ChannelConfig:
    where = f"channels.{kind}"
    if kind not in CHANNEL_KINDS:
        raise ConfigError(f"channels: unknown channel {kind!r}; known: {CHANNEL_KINDS}")
    common = {"enabled"}
    allowed = {
        "telegram": common | {"token_env", "owner_id"},
        "web": common | {"bind", "port"},
        "whatsapp": common | {"owner_id", "accept_unofficial_client_risk"},
    }[kind]
    _keys(data, allowed, where)
    enabled = bool(data.get("enabled", False))
    cfg = ChannelConfig(kind=kind, enabled=enabled)
    if kind == "telegram" and enabled:
        owner = str(data.get("owner_id", "") or "")
        if not re.fullmatch(r"-?\d{1,20}", owner):
            raise ConfigError(f"{where}.owner_id: required numeric Telegram user id (the bot answers only you)")
        cfg = ChannelConfig(kind=kind, enabled=True, owner_id=owner,
                            token_env=_env_name(data.get("token_env", ""), f"{where}.token_env"))
    elif kind == "web" and enabled:
        port = data.get("port", 8686)
        if not isinstance(port, int) or not (1024 <= port <= 65535):
            raise ConfigError(f"{where}.port: must be 1024-65535")
        cfg = ChannelConfig(kind=kind, enabled=True, bind=_loopback(data.get("bind", "127.0.0.1"), f"{where}.bind"),
                            port=port)
    elif kind == "whatsapp" and enabled:
        if data.get("accept_unofficial_client_risk") is not True:
            raise ConfigError(
                f"{where}: WhatsApp uses an unofficial client and can get the account banned; "
                "set accept_unofficial_client_risk: true to enable it (docs/terms-check.md section 3)"
            )
        owner = str(data.get("owner_id", "") or "")
        if not owner:
            raise ConfigError(f"{where}.owner_id: required (your own WhatsApp JID)")
        cfg = ChannelConfig(kind=kind, enabled=True, owner_id=owner, accept_unofficial_client_risk=True)
    return cfg


def parse_config(data: Mapping[str, Any], *, base_dir: Path, env: Mapping[str, str] | None = None) -> KurisuConfig:
    _keys(data, {"version", "data_dir", "roster", "runtime", "channels", "budget", "sandbox", "backup"}, "kurisu.yaml")
    if data.get("version") != 1:
        raise ConfigError("kurisu.yaml: version must be 1")

    def path(value: Any, default: Path) -> Path:
        if value in (None, ""):
            return default
        p = Path(os.path.expanduser(str(value)))
        return p if p.is_absolute() else (base_dir / p)

    data_dir = path(data.get("data_dir"), default_data_dir(env))
    roster_path = path(data.get("roster"), base_dir / "roster.yaml")

    rt = data.get("runtime", {}) or {}
    _keys(rt, {"kind", "server_url"}, "runtime")
    if rt.get("kind", "omnigent") != "omnigent":
        raise ConfigError("runtime.kind: only 'omnigent' is supported in this version")
    url = str(rt.get("server_url", "http://127.0.0.1:6767"))
    m = re.fullmatch(r"http://(\[[^\]]+\]|[^/:]+)(:\d+)?/?", url)
    if not m:
        raise ConfigError("runtime.server_url: must be http://<loopback>:<port>")
    _loopback(m.group(1).strip("[]"), "runtime.server_url")
    runtime = RuntimeConfig(server_url=url.rstrip("/"))

    chans = data.get("channels", {}) or {}
    if not isinstance(chans, Mapping):
        raise ConfigError("channels: must be a mapping")
    channels = tuple(_channel(str(k), v or {}) for k, v in chans.items())
    if not any(c.enabled for c in channels):
        raise ConfigError("channels: enable at least one channel (telegram or web)")

    b = data.get("budget", {}) or {}
    _keys(b, {"daily_usd", "max_concurrent_staff"}, "budget")
    daily = b.get("daily_usd", 0)
    conc = b.get("max_concurrent_staff", 2)
    if not isinstance(daily, (int, float)) or daily < 0:
        raise ConfigError("budget.daily_usd: must be >= 0")
    if not isinstance(conc, int) or not (1 <= conc <= 16):
        raise ConfigError("budget.max_concurrent_staff: must be 1-16")

    sb = data.get("sandbox", {}) or {}
    _keys(sb, {"default"}, "sandbox")
    default_sandbox = str(sb.get("default", "none"))
    if default_sandbox not in SANDBOX_PROFILES:
        raise ConfigError(f"sandbox.default: must be one of {SANDBOX_PROFILES}")

    bk = data.get("backup", {}) or {}
    _keys(bk, {"age_recipient", "dir"}, "backup")
    recipient = str(bk.get("age_recipient", "") or "")
    if recipient and not re.fullmatch(r"age1[0-9a-z]{20,100}", recipient):
        raise ConfigError("backup.age_recipient: must be an age public key (age1...)")

    return KurisuConfig(
        data_dir=data_dir,
        roster_path=roster_path,
        runtime=runtime,
        channels=channels,
        budget=BudgetConfig(daily_usd=float(daily), max_concurrent_staff=conc),
        default_sandbox=default_sandbox,
        backup_recipient=recipient,
        backup_dir=path(bk.get("dir"), data_dir / "backups") if bk else None,
    )


def load_config(path: str | Path, env: Mapping[str, str] | None = None) -> KurisuConfig:
    path = Path(path)
    data = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    return parse_config(data, base_dir=path.resolve().parent, env=env)
