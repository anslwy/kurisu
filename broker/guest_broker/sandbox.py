"""Sandbox construction and execution.

Two layers, both required in production:

1. **cgroup v2 via a transient systemd user scope** -- the kernel enforces
   memory, swap, CPU bandwidth, task count and wall-clock runtime, and accounts
   block-IO weight. A transient scope starts no unit file and no service; it
   lives under the operator's existing user manager and is collected on exit.
2. **bubblewrap** -- a fresh user/PID/net/IPC/UTS/cgroup namespace with a
   read-only system tree, a size-limited private ``/tmp``, and exactly one
   writable path: the guest workspace.

The broker refuses to degrade: if either layer is unavailable, :func:`probe`
reports it and the CLI refuses to start a real run.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Mapping, Sequence

from .config import Limits, RuntimeConfig, SandboxConfig
from .errors import SandboxError

#: Paths the guest runtime needs read-only. Anything not listed is absent.
_READ_ONLY_SYSTEM_PATHS = (
    "/usr",
    "/lib",
    "/lib32",
    "/lib64",
    "/bin",
    "/sbin",
    "/etc/ssl",
    "/etc/alternatives",
    "/etc/ld.so.cache",
    "/etc/localtime",
    "/etc/nsswitch.conf",
    "/etc/passwd",
    "/etc/group",
)

#: Host paths that must never appear inside the guest mount namespace. Checked
#: structurally: no bind source or workspace may live under these.
FORBIDDEN_PATHS = (
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/run/systemd",
    "/run/dbus",
    "/run/user",
    "/proc/sys",
    "/sys",
    "/boot",
    "/root",
    "/home",
)

#: Prefixes that are explicitly *not* forbidden, because the pilot lives on the
#: SD card under /run/media (which contains /run/media, not /run/user).
_FORBIDDEN_PREFIX_EXCEPTIONS = ("/run/media",)


@dataclass
class SandboxPlan:
    argv: Sequence[str]
    cwd: str
    env: Mapping[str, str]
    description: str = ""
    bwrap_argv: Sequence[str] = field(default_factory=tuple)


@dataclass
class RunResult:
    exit_code: int
    stdout: str
    stderr: str
    duration_seconds: float
    timed_out: bool = False
    killed_by_signal: int | None = None

    @property
    def ok(self) -> bool:
        return self.exit_code == 0 and not self.timed_out


def _check_workspace(workspace: Path) -> Path:
    for forbidden in FORBIDDEN_PATHS:
        if forbidden in _FORBIDDEN_PREFIX_EXCEPTIONS:
            continue
        if str(workspace) == forbidden or str(workspace).startswith(forbidden.rstrip("/") + "/"):
            raise SandboxError(f"guest workspace {workspace} is under forbidden path {forbidden}")
    try:
        resolved = workspace.resolve(strict=True)
    except OSError as exc:
        raise SandboxError(f"guest workspace is not accessible: {workspace} ({exc})") from exc
    if not resolved.is_dir():
        raise SandboxError(f"guest workspace is not a directory: {resolved}")
    for forbidden in FORBIDDEN_PATHS:
        if forbidden in _FORBIDDEN_PREFIX_EXCEPTIONS:
            continue
        if str(resolved) == forbidden or str(resolved).startswith(forbidden.rstrip("/") + "/"):
            raise SandboxError(f"guest workspace {resolved} is under forbidden path {forbidden}")
    return resolved


def _resolve_binary(path: str, label: str) -> str:
    """Resolve to an absolute path so a hostile PATH cannot substitute a binary."""
    found = shutil.which(path) if not path.startswith("/") else (path if Path(path).is_file() else None)
    if not found:
        raise SandboxError(f"{label} not found: {path!r}")
    return str(Path(found).resolve())


def scope_properties(limits: Limits) -> list[str]:
    """systemd properties that make the kernel enforce the guest budget.

    Only cgroup properties belong here: a transient *scope* is not spawned by
    systemd, so ``LimitFSIZE``-style rlimit properties are rejected by
    systemd-run ("Unknown assignment"). POSIX rlimits are applied in-namespace by
    ``guest-runtime/guest-exec.py`` instead.
    """
    return [
        f"MemoryMax={int(limits.memory_bytes)}",
        f"MemorySwapMax={int(limits.memory_swap_bytes)}",
        f"CPUQuota={limits.cpu_quota}",
        f"TasksMax={limits.tasks_max}",
        f"RuntimeMaxSec={limits.runtime_max_seconds}",
        f"IOWeight={limits.io_weight}",
        "MemoryAccounting=yes",
        "CPUAccounting=yes",
        "TasksAccounting=yes",
        "IOAccounting=yes",
    ]


def plan_bwrap(
    *,
    workspace: Path,
    runtime: RuntimeConfig,
    sandbox: SandboxConfig,
    command: Sequence[str],
    env: Mapping[str, str],
    tmpfs_bytes: int,
    limits: Limits | None = None,
    extra_ro_binds: Sequence[tuple[str, str]] = (),
) -> list[str]:
    """Build the bubblewrap argv for one guest command."""
    workspace = _check_workspace(workspace)
    if sandbox.mode == "none":
        raise SandboxError("bubblewrap plan requested for sandbox mode 'none'")

    effective_command = list(command)
    if limits is not None and sandbox.rlimit_launcher:
        launcher = Path(sandbox.rlimit_launcher)
        if not launcher.is_file():
            raise SandboxError(f"rlimit launcher not found: {launcher}")
        extra_ro_binds = tuple(extra_ro_binds) + ((str(launcher.parent), "/run/guest-runtime"),)
        effective_command = [
            sandbox.python_path,
            f"/run/guest-runtime/{launcher.name}",
            "--fsize", str(int(limits.file_size_bytes)),
            "--nofile", str(int(limits.nofile)),
            "--nproc", str(int(limits.tasks_max)),
            "--core", "0",
            "--",
            *command,
        ]

    bwrap: list[str] = [_resolve_binary(sandbox.bwrap_path, "bubblewrap"), "--unshare-all"]
    bwrap += [
        "--die-with-parent",
        "--new-session",
        "--clearenv",
        "--hostname", "guest",
        "--proc", "/proc",
        "--dev", "/dev",
        # Private scratch: size-limited tmpfs, so the guest cannot fill the SD card.
        # bubblewrap's --size applies to the *next* mount, so it comes first.
        "--size", str(int(tmpfs_bytes)),
        "--tmpfs", "/tmp",
    ]

    bound: set[str] = set()
    for path in _READ_ONLY_SYSTEM_PATHS:
        if path in bound or not Path(path).exists():
            continue
        bound.add(path)
        bwrap += ["--ro-bind-try", path, path]
    for path in sandbox.read_only_paths:
        if path in bound or not Path(path).exists():
            continue
        bound.add(path)
        bwrap += ["--ro-bind-try", path, path]
    for src, dest in extra_ro_binds:
        if not Path(src).exists():
            continue
        bwrap += ["--ro-bind-try", src, dest]

    # The single writable path. The guest's Pi configuration is created *inside*
    # it, so the operator's auth.json / models catalog never enters the sandbox.
    bwrap += ["--bind", str(workspace), str(workspace)]

    # Everything else that is not a bind mount lives on a private tmpfs; make it
    # read-only so a stray write cannot persist or grow unbounded.
    bwrap += ["--remount-ro", "/", "--chdir", str(workspace)]

    for key, value in env.items():
        bwrap += ["--setenv", key, value]

    bwrap += ["--", *effective_command]
    return bwrap


def plan_podman(
    *,
    workspace: Path,
    limits: Limits,
    runtime: RuntimeConfig,
    sandbox: SandboxConfig,
    command: Sequence[str],
    env: Mapping[str, str],
    guest_io_dir: Path | None = None,
) -> list[str]:
    """Rootless Podman argv (preferred runtime when the broker runs on the host).

    Every flag here maps to a documented guarantee:

    ``--userns=keep-id``      the guest runs as the *same* uid inside and outside,
                              so workspace files keep sane ownership and the
                              container cannot become another user.
    ``--read-only``           image root is immutable; only the workspace and an
                              explicitly sized tmpfs are writable.
    ``--cap-drop=ALL``        no capabilities at all.
    ``--security-opt no-new-privileges``  setuid binaries cannot escalate.
    ``--pids-limit``          fork-bomb ceiling (container cgroup).
    ``--memory``/``--memory-swap``  hard memory ceiling with swap disabled.
    ``--cpus``                CPU bandwidth ceiling.
    ``--ulimit fsize``        single-file size ceiling.
    ``--network=none``        no host network at all: the model path is a Unix
                              socket bind mount, not TCP.
    ``--log-driver=none``     no container logs written to the host journal.

    There is deliberately no ``--privileged``, no ``--device``, no host IPC,
    no host PID namespace, no container runtime socket, and no systemd/DBus
    mount anywhere in this argv.
    """
    workspace = _check_workspace(workspace)
    if not sandbox.image:
        raise SandboxError("sandbox.mode 'podman' requires sandbox.image")
    podman = _resolve_binary(sandbox.podman_path, "podman")

    cpu_quota_percent = int(limits.cpu_quota.rstrip("%"))
    memory_mib = max(16, limits.memory_bytes // (1024 * 1024))
    swap_mib = max(0, limits.memory_swap_bytes // (1024 * 1024))
    # Podman: memory-swap includes memory; equal values mean "no swap".
    memory_swap_total = memory_mib + swap_mib

    argv: list[str] = [
        podman, "run", "--rm",
        "--name", f"guest-{workspace.parent.name}",
        "--userns=keep-id",
        "--read-only",
        "--tmpfs", f"/tmp:rw,size={limits.tmpfs_bytes},mode=1777",
        "--cpus", f"{cpu_quota_percent / 100:.2f}",
        "--memory", f"{memory_mib}m",
        "--memory-swap", f"{memory_swap_total}m",
        "--pids-limit", str(limits.tasks_max),
        "--cap-drop=ALL",
        "--security-opt", "no-new-privileges",
        "--network=none",
        "--ipc=none",
        "--log-driver=none",
        "--ulimit", f"fsize={limits.file_size_bytes}:{limits.file_size_bytes}",
        "--ulimit", f"nofile={limits.nofile}:{limits.nofile}",
        "--ulimit", "core=0:0",
        "--workdir", str(workspace),
        # The only writable host path.
        "--volume", f"{workspace}:{workspace}:rw",
    ]
    if guest_io_dir is not None:
        # Model proxy socket (and nothing else) is visible read-only.
        argv += ["--volume", f"{guest_io_dir}:/run/guest-io:ro"]
    for key, value in env.items():
        argv += ["--env", f"{key}={value}"]
    argv += [sandbox.image, *command]
    return argv


def build_plan(
    *,
    workspace: Path,
    limits: Limits,
    runtime: RuntimeConfig,
    sandbox: SandboxConfig,
    command: Sequence[str],
    env: Mapping[str, str],
    extra_ro_binds: Sequence[tuple[str, str]] = (),
    guest_io_dir: Path | None = None,
) -> SandboxPlan:
    workspace = _check_workspace(workspace)
    if sandbox.mode == "podman":
        argv = plan_podman(
            workspace=workspace,
            limits=limits,
            runtime=runtime,
            sandbox=sandbox,
            command=command,
            env=env,
            guest_io_dir=guest_io_dir,
        )
        return SandboxPlan(
            argv=argv,
            cwd=str(workspace),
            env=dict(env),
            description=f"podman image={sandbox.image} chdir={workspace}",
            bwrap_argv=(),
        )

    bwrap_argv = plan_bwrap(
        workspace=workspace,
        runtime=runtime,
        sandbox=sandbox,
        command=command,
        env=env,
        tmpfs_bytes=limits.tmpfs_bytes,
        limits=limits,
        extra_ro_binds=extra_ro_binds,
    )

    if sandbox.mode == "systemd-bwrap":
        argv: list[str] = [_resolve_binary(sandbox.systemd_run_path, "systemd-run"), "--user", "--scope", "--quiet", "--collect"]
        for prop in scope_properties(limits):
            argv += ["-p", prop]
        argv += ["--", *bwrap_argv]
    elif sandbox.mode == "bwrap":
        argv = list(bwrap_argv)
    else:
        raise SandboxError(f"refusing to build a plan for sandbox mode {sandbox.mode!r}")

    return SandboxPlan(
        argv=argv,
        cwd=str(workspace),
        env=dict(env),
        description=f"{sandbox.mode} chdir={workspace}",
        bwrap_argv=tuple(bwrap_argv),
    )


def run_plan(
    plan: SandboxPlan,
    *,
    stdin_text: str | None = None,
    timeout_seconds: int = 300,
    env: Mapping[str, str] | None = None,
) -> RunResult:
    """Execute a plan. Guest failure is data, not an exception."""
    started = time.monotonic()
    try:
        proc = subprocess.run(
            list(plan.argv),
            cwd=plan.cwd,
            input=stdin_text,
            capture_output=True,
            text=True,
            timeout=timeout_seconds + 30,  # outer guard; the scope enforces the real one
            # The *launcher* (systemd-run/bwrap) needs the operator environment to
            # reach the user manager bus. The guest itself gets a clean env via
            # bwrap --clearenv/--setenv, so nothing leaks inward.
            env={**os.environ, **(dict(env) if env else {})},
            stdin=None if stdin_text is not None else subprocess.DEVNULL,
        )
    except subprocess.TimeoutExpired as exc:  # pragma: no cover - scope kills first
        return RunResult(
            exit_code=-9,
            stdout=exc.stdout if isinstance(exc.stdout, str) else "",
            stderr=exc.stderr if isinstance(exc.stderr, str) else "",
            duration_seconds=time.monotonic() - started,
            timed_out=True,
        )
    duration = time.monotonic() - started
    return RunResult(
        exit_code=proc.returncode,
        stdout=proc.stdout,
        stderr=proc.stderr,
        duration_seconds=duration,
        timed_out=proc.returncode in (-9, 137),
        killed_by_signal=-proc.returncode if proc.returncode < 0 else None,
    )


# --------------------------------------------------------------------------
# capability probe
# --------------------------------------------------------------------------
@dataclass(frozen=True)
class Capability:
    name: str
    ok: bool
    detail: str

    @property
    def marker(self) -> str:
        return "OK  " if self.ok else "FAIL"

    def format(self) -> str:
        return f"[{self.marker}] {self.name}: {self.detail}"


def _which(path: str) -> str | None:
    return shutil.which(path) or (path if Path(path).is_file() else None)


def probe(sandbox: SandboxConfig, limits: Limits | None = None) -> list[Capability]:
    caps: list[Capability] = []

    if sandbox.mode == "podman":
        podman = _which(sandbox.podman_path)
        if not podman:
            caps.append(Capability("rootless podman", False, f"{sandbox.podman_path} not found"))
        else:
            try:
                out = subprocess.run([podman, "info", "--format", "{{.Host.Security.Rootless}}"],
                                     capture_output=True, text=True, timeout=30)
                rootless = (out.stdout or "").strip().lower()
                caps.append(Capability(
                    "rootless podman",
                    out.returncode == 0 and rootless == "true",
                    f"rootless={rootless or 'unknown'} version={_podman_version(podman)}",
                ))
            except (OSError, subprocess.SubprocessError) as exc:
                caps.append(Capability("rootless podman", False, f"{type(exc).__name__}: {exc}"))
        return caps

    bwrap_path = _which(sandbox.bwrap_path)
    if not bwrap_path:
        caps.append(Capability("bubblewrap", False, "bwrap not found"))
    else:
        try:
            out = subprocess.run([bwrap_path, "--version"], capture_output=True, text=True, timeout=10)
            caps.append(Capability("bubblewrap", out.returncode == 0, (out.stdout or "").strip() or "unknown version"))
        except (OSError, subprocess.SubprocessError) as exc:
            caps.append(Capability("bubblewrap", False, f"{type(exc).__name__}: {exc}"))

    try:
        max_user_ns = Path("/proc/sys/user/max_user_namespaces").read_text().strip()
        caps.append(Capability("unprivileged userns", int(max_user_ns) > 0, f"max_user_namespaces={max_user_ns}"))
    except (OSError, ValueError) as exc:
        caps.append(Capability("unprivileged userns", False, f"cannot read sysctl: {exc}"))

    try:
        controllers = Path("/sys/fs/cgroup/cgroup.controllers").read_text().strip()
        caps.append(Capability("cgroup v2", bool(controllers), f"controllers: {controllers or 'none'}"))
    except OSError as exc:
        caps.append(Capability("cgroup v2", False, f"cannot read cgroup.controllers: {exc}"))

    systemd_run = _which(sandbox.systemd_run_path)
    if not systemd_run:
        caps.append(Capability("systemd --user scope", False, "systemd-run not found"))
    else:
        argv = [
            systemd_run, "--user", "--scope", "--quiet", "--collect",
            "-p", "MemoryMax=32M", "-p", "MemorySwapMax=0",
            "--", "/bin/sh", "-c", "exit 0",
        ]
        try:
            out = subprocess.run(argv, capture_output=True, text=True, timeout=20)
            if out.returncode == 0:
                caps.append(Capability("systemd --user scope", True, "transient scope with MemoryMax accepted"))
            else:
                lines = (out.stderr or out.stdout or "").strip().splitlines()
                caps.append(Capability("systemd --user scope", False, lines[-1] if lines else f"exit {out.returncode}"))
        except (OSError, subprocess.SubprocessError) as exc:
            caps.append(Capability("systemd --user scope", False, f"{type(exc).__name__}: {exc}"))

    if limits is not None:
        caps.append(_probe_oom(systemd_run, limits))
    return caps


def _podman_version(podman: str) -> str:
    try:
        out = subprocess.run([podman, "--version"], capture_output=True, text=True, timeout=15)
        return (out.stdout or "").strip()
    except (OSError, subprocess.SubprocessError):
        return "unknown"


def _probe_oom(systemd_run: str | None, limits: Limits) -> Capability:
    """Allocate past MemoryMax in a scope and require the kernel to stop it."""
    if not systemd_run:
        return Capability("memory enforcement", False, "systemd-run unavailable")
    probe_bytes = min(limits.memory_bytes, 64 * 1024 * 1024)
    script = (
        "import sys\n"
        "b = bytearray()\n"
        "try:\n"
        "    for _ in range(4096):\n"
        "        b.extend(bytearray(1024 * 1024))\n"
        "except MemoryError:\n"
        "    sys.exit(3)\n"
        "sys.exit(0)\n"
    )
    argv = [
        systemd_run, "--user", "--scope", "--quiet", "--collect",
        "-p", f"MemoryMax={probe_bytes}", "-p", "MemorySwapMax=0",
        "--", "python3", "-c", script,
    ]
    try:
        out = subprocess.run(argv, capture_output=True, text=True, timeout=90)
    except (OSError, subprocess.SubprocessError) as exc:
        return Capability("memory enforcement", False, f"{type(exc).__name__}: {exc}")
    ok = out.returncode in (137, -9, 3)
    detail = {
        137: "kernel OOM-killed the guest at MemoryMax",
        -9: "kernel OOM-killed the guest at MemoryMax",
        3: "allocation refused at MemoryMax",
    }.get(out.returncode, f"unexpected exit {out.returncode}: {(out.stderr or '').strip()[:200]}")
    return Capability(
        "memory enforcement",
        ok,
        f"MemoryMax={probe_bytes // (1024 * 1024)}MiB -> {detail}",
    )
