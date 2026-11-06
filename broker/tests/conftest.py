from __future__ import annotations

import json
import os
import shutil
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from guest_broker import config as config_mod  # noqa: E402
from guest_broker.allowlist import load_allowlist  # noqa: E402
from guest_broker.broker import Broker  # noqa: E402
from guest_broker.backend import EchoBackend  # noqa: E402
from guest_broker.registry import load_registry  # noqa: E402

SESSION_ID = "conv_0123456789abcdef"
GUEST_JID = "15551230001@s.whatsapp.net"


@pytest.fixture
def layout(tmp_path: Path) -> dict[str, Path]:
    guests_root = tmp_path / "guests"
    workspace = guests_root / "pilot-001" / "workspace"
    workspace.mkdir(parents=True)
    operator = tmp_path / "operator"
    operator.mkdir()
    return {"root": tmp_path, "guests_root": guests_root, "workspace": workspace, "operator": operator}


def write_allowlist(path: Path, entries: list[dict], mode: int = 0o600) -> Path:
    path.write_text(json.dumps({"version": 1, "identities": entries}), encoding="utf-8")
    os.chmod(path, mode)
    return path


def write_registry(path: Path, guests_root: Path, users: dict[str, str] | None = None, mode: int = 0o600) -> Path:
    users = users or {"pilot-001": SESSION_ID}
    lines = ["version: 1", "users:"]
    for user_id, session_id in users.items():
        lines += [
            f"  {user_id}:",
            f"    session_id: {session_id}",
            f"    workspace: {guests_root / user_id / 'workspace'}",
            "    enabled: true",
        ]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    os.chmod(path, mode)
    return path


@pytest.fixture
def mappings(layout: dict[str, Path]) -> dict[str, Path]:
    allowlist = write_allowlist(
        layout["operator"] / "allowlist.json",
        [{"wa_jid": GUEST_JID, "user_id": "pilot-001", "enabled": True}],
    )
    registry = write_registry(layout["operator"] / "registry.yaml", layout["guests_root"])
    return {"allowlist": allowlist, "registry": registry}


def make_config(
    layout: dict[str, Path],
    mappings: dict[str, Path],
    *,
    sandbox_mode: str = "systemd-bwrap",
    backend: str = "echo",
    model: str = "deepseek-v4.1-flash:cloud",
    extra: str = "",
) -> Path:
    operator = layout["operator"]
    text = f"""\
version: 1
broker:
  socket_path: {operator / 'broker.sock'}
  state_dir: {operator / 'state'}
  audit_path: {operator / 'audit.jsonl'}
  guests_root: {layout['guests_root']}
  registry_path: {mappings['registry']}
  allowlist_path: {mappings['allowlist']}
  max_message_chars: 200
  max_reply_chars: 80
  per_user_rate: 3
  per_user_rate_window: 60
limits:
  memory_max: 128M
  memory_swap_max: 0
  cpu_quota: 50%
  tasks_max: 32
  runtime_max_sec: 30
  file_size_max: 16M
  tmpfs_size: 16M
  nofile: 1024
  io_weight: 100
  disk_quota: 256M
sandbox:
  mode: {sandbox_mode}
  share_net: false
  bwrap_path: {shutil.which('bwrap') or '/usr/bin/bwrap'}
  systemd_run_path: {shutil.which('systemd-run') or '/usr/bin/systemd-run'}
  read_only_paths: []
  extra_env: {{}}
  rlimit_launcher: {Path(__file__).resolve().parents[2] / 'guest-runtime' / 'guest-exec.py'}
  python_path: /usr/bin/python3
  model_proxy:
    enabled: false
    upstream: http://127.0.0.1:11434
    allowed_paths: ["/v1/chat/completions", "/v1/completions"]
runtime:
  backend: {backend}
  provider: ollama
  model: {model}
  pi_path: /usr/bin/true
  pi_agent_dir: {operator / 'pi-agent'}
  pi_session_dir_name: pi-sessions
  pi_tools: [read, write, edit, bash]
  timeout_seconds: 60
  omnigent:
    base_url: ""
    token_env: OMNIGENT_OPERATOR_TOKEN
{extra}"""
    path = operator / "broker.yaml"
    path.write_text(text, encoding="utf-8")
    return path


@pytest.fixture
def cfg(layout: dict[str, Path], mappings: dict[str, Path]):
    return config_mod.load_config(make_config(layout, mappings))


@pytest.fixture
def loaded(layout: dict[str, Path], mappings: dict[str, Path], cfg):
    registry = load_registry(cfg.resolve(cfg.broker.registry_path), cfg.broker.guests_root)
    allowlist = load_allowlist(cfg.resolve(cfg.broker.allowlist_path))
    return {"registry": registry, "allowlist": allowlist, "cfg": cfg}


@pytest.fixture
def broker(loaded) -> Broker:
    cfg = loaded["cfg"]
    return Broker(
        settings=cfg.broker,
        limits=cfg.limits,
        sandbox=cfg.sandbox,
        runtime=cfg.runtime,
        registry=loaded["registry"],
        allowlist=loaded["allowlist"],
        backend=EchoBackend(),
        base_dir=cfg.base_dir,
    )
