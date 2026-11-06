"""Sandbox plan construction plus real kernel-enforcement tests.

The unit tests are cheap. The integration tests actually start transient systemd
scopes and bubblewrap sandboxes, so they prove the limits instead of asserting
them. They are skipped automatically where the host cannot provide them.
"""

from __future__ import annotations

import dataclasses
import shutil
from pathlib import Path

import pytest

from guest_broker import config as config_mod
from guest_broker import sandbox as sandbox_mod
from guest_broker.errors import SandboxError

from .conftest import make_config


def bwrap_available() -> bool:
    return shutil.which("bwrap") is not None


def systemd_scope_available() -> bool:
    try:
        import subprocess

        out = subprocess.run(
            [shutil.which("systemd-run") or "systemd-run", "--user", "--scope", "--quiet", "--collect", "--", "/bin/true"],
            capture_output=True,
            timeout=20,
        )
        return out.returncode == 0
    except Exception:
        return False


requires_sandbox = pytest.mark.skipif(
    not (bwrap_available() and systemd_scope_available()),
    reason="bwrap + systemd user scope required",
)


# ------------------------------------------------------------------ unit
def test_plan_contains_every_limit(loaded):
    cfg = loaded["cfg"]
    plan = sandbox_mod.build_plan(
        workspace=cfg.broker.guests_root / "pilot-001" / "workspace",
        limits=cfg.limits,
        runtime=cfg.runtime,
        sandbox=cfg.sandbox,
        command=["/bin/true"],
        env={"PATH": "/usr/bin"},
    )
    argv = list(plan.argv)
    joined = " ".join(argv)
    for expected in (
        f"MemoryMax={cfg.limits.memory_bytes}",
        f"MemorySwapMax={cfg.limits.memory_swap_bytes}",
        f"CPUQuota={cfg.limits.cpu_quota}",
        f"TasksMax={cfg.limits.tasks_max}",
        f"RuntimeMaxSec={cfg.limits.runtime_max_seconds}",
        f"IOWeight={cfg.limits.io_weight}",
    ):
        assert expected in joined, expected
    # rlimits are applied inside the namespace by the launcher, not by systemd
    assert "--fsize" in joined and str(cfg.limits.file_size_bytes) in joined
    assert "--unshare-all" in argv
    assert "--clearenv" in argv
    assert "--die-with-parent" in argv


def test_plan_has_exactly_one_writable_host_bind(loaded):
    cfg = loaded["cfg"]
    workspace = cfg.broker.guests_root / "pilot-001" / "workspace"
    plan = sandbox_mod.build_plan(
        workspace=workspace,
        limits=cfg.limits,
        runtime=cfg.runtime,
        sandbox=cfg.sandbox,
        command=["/bin/true"],
        env={"PATH": "/usr/bin"},
    )
    argv = list(plan.bwrap_argv)
    binds = [argv[i + 1] for i, token in enumerate(argv) if token == "--bind"]
    assert binds == [str(workspace.resolve())]
    # No host home, no docker socket, no systemd runtime paths anywhere.
    forbidden = ["/home", "/root", "/run/user", "/run/systemd", "docker.sock", "/sys"]
    for path in forbidden:
        for token in argv:
            if token.startswith("/"):
                assert token != path, f"forbidden path {path} bound into sandbox"


def test_plan_refuses_forbidden_workspace(loaded, tmp_path):
    cfg = loaded["cfg"]
    bad = Path("/root/workspace")
    with pytest.raises(SandboxError):
        sandbox_mod.build_plan(
            workspace=bad,
            limits=cfg.limits,
            runtime=cfg.runtime,
            sandbox=cfg.sandbox,
            command=["/bin/true"],
            env={},
        )


def test_plan_refuses_mode_none(loaded):
    cfg = loaded["cfg"]
    with pytest.raises(SandboxError):
        sandbox_mod.build_plan(
            workspace=cfg.broker.guests_root / "pilot-001" / "workspace",
            limits=cfg.limits,
            runtime=cfg.runtime,
            sandbox=dataclasses.replace(cfg.sandbox, mode="none"),
            command=["/bin/true"],
            env={},
        )


# ------------------------------------------------------- real enforcement
def _plan(loaded, command, **overrides):
    cfg = loaded["cfg"]
    limits = dataclasses.replace(
        cfg.limits,
        memory_bytes=128 * 1024 * 1024,
        memory_swap_bytes=0,
        cpu_quota="25%",
        tasks_max=16,
        runtime_max_seconds=6,
        file_size_bytes=8 * 1024 * 1024,
        tmpfs_bytes=8 * 1024 * 1024,
    )
    return sandbox_mod.build_plan(
        workspace=cfg.broker.guests_root / "pilot-001" / "workspace",
        limits=limits,
        runtime=cfg.runtime,
        sandbox=cfg.sandbox,
        command=command,
        env={"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": "/tmp"},
    )


@requires_sandbox
def test_memory_limit_is_enforced_by_the_kernel(loaded):
    # Baseline: the same plan and the same launcher succeed when nothing allocates.
    baseline = sandbox_mod.run_plan(
        _plan(loaded, ["/usr/bin/python3", "-c", "print('OK')"]), timeout_seconds=90
    )
    assert baseline.exit_code == 0 and baseline.stdout.strip() == "OK"

    plan = _plan(
        loaded,
        [
            "/usr/bin/python3",
            "-c",
            "b=bytearray()\n"
            "try:\n"
            "    for _ in range(2048):\n"
            "        b.extend(bytearray(1024*1024))\n"
            "    print('NOT_LIMITED', len(b)//1048576)\n"
            "except MemoryError:\n"
            "    print('LIMITED')\n",
        ],
    )
    result = sandbox_mod.run_plan(plan, timeout_seconds=90)
    # SIGKILL (137/-9) is the kernel OOM killer; -15 is systemd stopping the scope
    # on the OOM event. Either way the 2 GiB allocation never completed.
    assert result.exit_code in (137, -9, -15) or "LIMITED" in (result.stdout + result.stderr)
    assert "NOT_LIMITED" not in result.stdout
    assert result.duration_seconds < 60


@requires_sandbox
def test_task_limit_is_enforced(loaded):
    plan = _plan(
        loaded,
        [
            "/usr/bin/python3",
            "-c",
            "import os,time\n"
            "kids=[]\n"
            "try:\n"
            "    for _ in range(64):\n"
            "        pid=os.fork()\n"
            "        if pid==0:\n"
            "            time.sleep(4); os._exit(0)\n"
            "        kids.append(pid)\n"
            "    print('FORKED_ALL', len(kids))\n"
            "except OSError as e:\n"
            "    print('BLOCKED_AT', len(kids), e.errno)\n",
        ],
    )
    result = sandbox_mod.run_plan(plan, timeout_seconds=90)
    assert result.exit_code in (137, -9) or "BLOCKED_AT" in result.stdout


@requires_sandbox
def test_runtime_limit_kills_a_long_turn(loaded):
    plan = _plan(loaded, ["/usr/bin/bash", "-c", "sleep 60; echo NOT_KILLED"])
    result = sandbox_mod.run_plan(plan, timeout_seconds=90)
    assert "NOT_KILLED" not in result.stdout
    assert result.exit_code != 0
    assert result.duration_seconds < 40


@requires_sandbox
def test_filesystem_isolation(loaded):
    plan = _plan(
        loaded,
        [
            "/usr/bin/bash",
            "-c",
            'touch /etc/escape 2>&1 | head -1; '
            'touch /escape 2>&1 | head -1; '
            'ls /var/run/docker.sock 2>&1 | head -1; '
            'ls -d /home 2>&1 | head -1; '
            'ls -d /sys/kernel 2>&1 | head -1; '
            'echo HOME=$(pwd)',
        ],
    )
    result = sandbox_mod.run_plan(plan, timeout_seconds=60)
    out = result.stdout
    assert out.count("Read-only file system") >= 2, out
    assert out.count("No such file or directory") >= 3, out
    assert "pilot-001/workspace" in out


@requires_sandbox
def test_only_the_workspace_is_writable(loaded):
    cfg = loaded["cfg"]
    workspace = cfg.broker.guests_root / "pilot-001" / "workspace"
    plan = _plan(
        loaded,
        [
            "/usr/bin/bash",
            "-c",
            "echo ok > ./inside.txt && echo WORKSPACE_OK; "
            "echo no > /usr/escape.txt 2>/dev/null || echo USR_READONLY; "
            "echo no > /tmp/scratch.txt 2>/dev/null && echo TMPFS_WRITABLE; "
            "echo no > /escape.txt 2>/dev/null || echo ROOT_READONLY",
        ],
    )
    result = sandbox_mod.run_plan(plan, timeout_seconds=60)
    assert "WORKSPACE_OK" in result.stdout
    assert "USR_READONLY" in result.stdout
    assert "ROOT_READONLY" in result.stdout
    assert "TMPFS_WRITABLE" in result.stdout
    assert (workspace / "inside.txt").read_text().strip() == "ok"


@requires_sandbox
def test_file_size_rlimit_blocks_large_writes(loaded):
    workspace = loaded["cfg"].broker.guests_root / "pilot-001" / "workspace"
    plan = _plan(
        loaded,
        [
            "/usr/bin/bash",
            "-c",
            "dd if=/dev/zero of=./big.bin bs=1M count=32 >/dev/null 2>&1; "
            "stat -c %s big.bin 2>/dev/null || echo 0; rm -f big.bin",
        ],
    )
    result = sandbox_mod.run_plan(plan, timeout_seconds=90)
    sizes = [token for token in result.stdout.split() if token.isdigit()]
    biggest = max((int(s) for s in sizes), default=0)
    assert biggest <= 9 * 1024 * 1024, result.stdout


@requires_sandbox
def test_network_namespace_blocks_host_reachability(loaded):
    plan = _plan(
        loaded,
        [
            "/usr/bin/bash",
            "-c",
            "cat /proc/net/dev | tail -n +3 | awk '{print $1}' | tr -d ':'",
        ],
    )
    result = sandbox_mod.run_plan(plan, timeout_seconds=60)
    interfaces = [line for line in result.stdout.split() if line]
    # A fresh netns has only loopback: no host route, no eth0.
    assert interfaces in ([], ["lo"]) or all(iface.startswith("lo") for iface in interfaces)


def test_probe_reports_capabilities(loaded):
    caps = sandbox_mod.probe(loaded["cfg"].sandbox, None)
    names = {cap.name for cap in caps}
    assert {"bubblewrap", "unprivileged userns", "cgroup v2", "systemd --user scope"} <= names


# ------------------------------------------------------------------ podman
def test_podman_plan_is_hardened(loaded, tmp_path):
    """Argv construction only: no container is started by the test suite."""
    cfg = loaded["cfg"]
    fake = tmp_path / "podman"
    fake.write_text("#!/bin/sh\nexit 0\n")
    fake.chmod(0o755)
    sandbox = dataclasses.replace(cfg.sandbox, mode="podman", image="pilot/guest:1.0.0", podman_path=str(fake))
    workspace = cfg.broker.guests_root / "pilot-001" / "workspace"
    guest_io = tmp_path / "guest-io"
    guest_io.mkdir()
    plan = sandbox_mod.build_plan(
        workspace=workspace,
        limits=cfg.limits,
        runtime=cfg.runtime,
        sandbox=sandbox,
        command=["/opt/pi", "-p", "hello"],
        env={"PATH": "/usr/bin", "PI_CODING_AGENT_DIR": "/opt/pi-agent"},
        guest_io_dir=guest_io,
    )
    argv = list(plan.argv)
    joined = " ".join(argv)

    for required in (
        "--userns=keep-id",
        "--read-only",
        "--cap-drop=ALL",
        "no-new-privileges",
        "--network=none",
        f"--pids-limit {cfg.limits.tasks_max}",
        f"--memory {cfg.limits.memory_bytes // (1024 * 1024)}m",
        "--ipc=none",
        "--log-driver=none",
    ):
        assert required in joined, required
    # exactly one writable host volume: the guest workspace
    volumes = [argv[i + 1] for i, token in enumerate(argv) if token == "--volume"]
    writable = [v for v in volumes if v.endswith(":rw")]
    assert writable == [f"{workspace.resolve()}:{workspace.resolve()}:rw"]
    assert any(v.startswith(f"{guest_io}:") and v.endswith(":ro") for v in volumes)
    # never privileged, never a host namespace or runtime socket
    for forbidden_flag in ("--privileged", "--pid=host", "--ipc=host", "--network=host", "--device"):
        assert forbidden_flag not in argv, forbidden_flag
    assert "docker.sock" not in joined and "podman.sock" not in joined and "/run/user" not in joined
    # rlimit ceilings travel with the container
    assert f"fsize={cfg.limits.file_size_bytes}:{cfg.limits.file_size_bytes}" in joined


def test_podman_mode_requires_pinned_image(layout, mappings):
    from guest_broker.errors import ConfigError
    import pytest as _pytest

    path = make_config(layout, mappings)
    path.write_text(
        path.read_text(encoding="utf-8")
        .replace("mode: systemd-bwrap", "mode: podman")
        .replace("  read_only_paths: []", "  image: pilot/guest:1.0.0\n  read_only_paths: []"),
        encoding="utf-8",
    )
    cfg = config_mod.load_config(path)
    assert cfg.sandbox.mode == "podman"
    assert config_mod.PREFERRED_SANDBOX_MODES[0] == "podman"

    path.write_text(path.read_text(encoding="utf-8").replace("  image: pilot/guest:1.0.0\n", ""), encoding="utf-8")
    with _pytest.raises(ConfigError, match="requires sandbox.image"):
        config_mod.load_config(path)

    path.write_text(
        path.read_text(encoding="utf-8").replace("  read_only_paths: []", "  image: pilot/guest\n  read_only_paths: []"),
        encoding="utf-8",
    )
    with _pytest.raises(ConfigError, match="explicit tag or digest"):
        config_mod.load_config(path)
