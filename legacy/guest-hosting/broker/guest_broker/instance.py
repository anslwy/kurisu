"""Dedicated per-guest Omnigent instance (pilot isolation architecture).

Why this exists: the installed Omnigent 0.20 managed-runner credential can mint
an *owner* bearer (docs/RUNNER-AUTH-BLOCKER.md). An owner bearer is harmless only
if the owner owns nothing but this guest's session tree. So every guest gets its
own Omnigent server process with its own config home, data dir, SQLite DB,
artifacts and logs. The guest's runner connects to that instance and nothing
else. Whatever authority the runner obtains is bounded by the instance.

Layout (all under ``<guests_root>/<user_id>/``, created 0700)::

    workspace/                 container /workspace          (rw, persistent)
    persist/home/              container /home/guest         (rw: runner + Pi history, caches)
    persist/downloads/         container /home/guest/Downloads
    persist/toolchains/        container /opt/toolchains
    persist/state/             container /var/lib/kurisu
    instance/instance.json     port, fixed model, root session id (0600)
    instance/config/           OMNIGENT_CONFIG_HOME of the instance server
    instance/data/             OMNIGENT_DATA_DIR: omnigent.db + artifacts/
    instance/logs/             server + relay logs
    instance/home/             HOME of the host-side instance server process
    instance/io/               bind-mounted read-only at /run/kurisu-io:
                               omnigent.sock (-> 127.0.0.1:<port>) and
                               model-proxy.sock (-> allowlisted model endpoint)

Network path, and the only one: the container runs with ``--network=none``.
Inside it, ``guest-runtime/instance-relay.py`` listens on the container's own
loopback ``127.0.0.1:<port>`` and forwards bytes to ``/run/kurisu-io/omnigent.sock``.
On the host, :class:`HostRelay` accepts on that Unix socket and connects to the
fixed ``127.0.0.1:<port>`` of *this guest's* instance. The relay target is not
taken from the connection, so the container cannot reach the main Omni Lead
server, another guest, or any other host port.

Nothing in this module starts a permanent service. ``serve``/``runner`` are
foreground operator commands; everything else is pure planning and validation.
"""

from __future__ import annotations

import datetime as _dt
import hashlib
import ipaddress
import json
import os
import re
import socket
import stat
import threading
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterable, Mapping, Sequence

from .errors import BrokerError

#: The one model every guest instance uses. No guest model selection exists.
FIXED_PROVIDER = "ollama"
FIXED_MODEL = "deepseek-v4.1-flash:cloud"
FIXED_MODEL_ID = f"{FIXED_PROVIDER}/{FIXED_MODEL}"

#: The only bind address an instance may use. Loopback makes Omnigent default to
#: single-user mode (no login, no admin credential to share) and keeps the API off
#: every network interface.
INSTANCE_HOST = "127.0.0.1"

#: Default instance port window. Distinct from the main server's default (6767).
DEFAULT_PORT_BASE = 27100
DEFAULT_PORT_SPAN = 900
#: Ports an instance may never use (main Omni Lead default and common services).
ALWAYS_RESERVED_PORTS = frozenset({6767, 11434})

USER_ID_RE = re.compile(r"^[a-z0-9][a-z0-9-]{0,31}$")
SESSION_ID_RE = re.compile(r"^conv_[A-Za-z0-9_-]{1,128}$")
IMAGE_RE = re.compile(r"^[a-z0-9./_:@-]+$")

#: Container mount points (fixed; the guest cannot choose them).
CONTAINER_WORKSPACE = "/workspace"
CONTAINER_HOME = "/home/guest"
CONTAINER_DOWNLOADS = "/home/guest/Downloads"
CONTAINER_TOOLCHAINS = "/opt/toolchains"
CONTAINER_STATE = "/var/lib/kurisu"
CONTAINER_IO = "/run/kurisu-io"

#: Environment never forwarded to the instance server or the runner container.
_FORBIDDEN_ENV_RE = re.compile(
    r"(TOKEN|SECRET|PASSWORD|API_KEY|CREDENTIAL|COOKIE)|^OMNIGENT_(SERVER|SERVER_URL|URL|AUTH|HOST_ID|RUNNER)",
    re.IGNORECASE,
)


class InstanceError(BrokerError):
    """Invalid instance spec, unsafe path/port, or identity mismatch."""


# ---------------------------------------------------------------------------
# validation helpers
# ---------------------------------------------------------------------------


def validate_user_id(user_id: str) -> str:
    if not isinstance(user_id, str) or not USER_ID_RE.fullmatch(user_id):
        raise InstanceError(f"user id {user_id!r} must match {USER_ID_RE.pattern}")
    return user_id


def validate_session_id(session_id: str) -> str:
    if not isinstance(session_id, str) or not SESSION_ID_RE.fullmatch(session_id):
        raise InstanceError(f"session id {session_id!r} is not a valid conv_ id")
    return session_id


def validate_host(host: str) -> str:
    """Only the IPv4 loopback address. ``0.0.0.0``, ``localhost`` names, LAN IPs: refused."""
    try:
        addr = ipaddress.ip_address(host)
    except ValueError as exc:
        raise InstanceError(f"instance host must be the literal {INSTANCE_HOST}, got {host!r}") from exc
    if str(addr) != INSTANCE_HOST:
        raise InstanceError(
            f"instance host must be {INSTANCE_HOST} (private, single-user, no public API), got {host!r}"
        )
    return INSTANCE_HOST


def validate_port(port: int, *, reserved: Iterable[int] = (), base: int = DEFAULT_PORT_BASE,
                  span: int = DEFAULT_PORT_SPAN) -> int:
    if not isinstance(port, int) or isinstance(port, bool):
        raise InstanceError(f"port must be an integer, got {port!r}")
    if not (base <= port < base + span):
        raise InstanceError(f"port {port} is outside the instance window [{base}, {base + span})")
    if port in ALWAYS_RESERVED_PORTS or port in set(reserved):
        raise InstanceError(f"port {port} is reserved (main server or another service)")
    return port


def _is_within(child: Path, parent: Path) -> bool:
    try:
        child.relative_to(parent)
        return True
    except ValueError:
        return False


def default_protected_paths(env: Mapping[str, str] | None = None) -> tuple[Path, ...]:
    """Main Omni Lead state an instance must never live in (or contain)."""
    env = os.environ if env is None else env
    paths = [Path(env.get("HOME", str(Path.home()))) / ".omnigent"]
    for name in ("OMNIGENT_CONFIG_HOME", "OMNIGENT_DATA_DIR"):
        if env.get(name):
            paths.append(Path(env[name]))
    return tuple(p.resolve() for p in paths)


def check_path_isolation(paths: Iterable[Path], *, guests_root: Path, user_id: str,
                         protected: Iterable[Path]) -> None:
    """Every instance path is inside ``guests_root/user_id`` and disjoint from protected trees."""
    user_root = (guests_root / user_id).resolve()
    protected = tuple(Path(p).resolve() for p in protected)
    for raw in paths:
        path = Path(raw).resolve()
        if not _is_within(path, user_root):
            raise InstanceError(f"{path} escapes the guest root {user_root}")
        for prot in protected:
            if _is_within(path, prot) or _is_within(prot, path):
                raise InstanceError(f"{path} overlaps protected main Omnigent state {prot}")


# ---------------------------------------------------------------------------
# the spec
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class InstanceSpec:
    user_id: str
    guests_root: Path
    port: int
    host: str = INSTANCE_HOST

    def __post_init__(self) -> None:
        validate_user_id(self.user_id)
        validate_host(self.host)
        if not Path(self.guests_root).is_absolute():
            raise InstanceError("guests_root must be an absolute path")
        object.__setattr__(self, "guests_root", Path(self.guests_root))

    # -- host paths ---------------------------------------------------------
    @property
    def user_root(self) -> Path:
        return self.guests_root / self.user_id

    @property
    def instance_dir(self) -> Path:
        return self.user_root / "instance"

    @property
    def state_file(self) -> Path:
        return self.instance_dir / "instance.json"

    @property
    def config_home(self) -> Path:
        return self.instance_dir / "config"

    @property
    def data_dir(self) -> Path:
        return self.instance_dir / "data"

    @property
    def db_path(self) -> Path:
        return self.data_dir / "omnigent.db"

    @property
    def artifacts_dir(self) -> Path:
        return self.data_dir / "artifacts"

    @property
    def logs_dir(self) -> Path:
        return self.instance_dir / "logs"

    @property
    def server_home(self) -> Path:
        return self.instance_dir / "home"

    @property
    def io_dir(self) -> Path:
        return self.instance_dir / "io"

    @property
    def relay_socket(self) -> Path:
        return self.io_dir / "omnigent.sock"

    @property
    def model_socket(self) -> Path:
        return self.io_dir / "model-proxy.sock"

    @property
    def workspace(self) -> Path:
        return self.user_root / "workspace"

    @property
    def persist_dir(self) -> Path:
        return self.user_root / "persist"

    def container_mounts(self) -> tuple[tuple[Path, str], ...]:
        """(host path, container path) pairs that are writable and persistent."""
        p = self.persist_dir
        return (
            (self.workspace, CONTAINER_WORKSPACE),
            (p / "home", CONTAINER_HOME),
            (p / "downloads", CONTAINER_DOWNLOADS),
            (p / "toolchains", CONTAINER_TOOLCHAINS),
            (p / "state", CONTAINER_STATE),
        )

    def all_dirs(self) -> tuple[Path, ...]:
        return (
            self.user_root, self.instance_dir, self.config_home, self.data_dir,
            self.artifacts_dir, self.logs_dir, self.server_home, self.io_dir,
            self.persist_dir, *(host for host, _ in self.container_mounts()),
        )

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"

    @property
    def container_name(self) -> str:
        return f"kurisu-{self.user_id}-runner"

    def check_isolation(self, protected: Iterable[Path] | None = None) -> None:
        check_path_isolation(
            self.all_dirs() + (self.db_path, self.state_file, self.relay_socket, self.model_socket),
            guests_root=self.guests_root,
            user_id=self.user_id,
            protected=default_protected_paths() if protected is None else protected,
        )


# ---------------------------------------------------------------------------
# port allocation (unique per guest, deterministic, persisted in instance.json)
# ---------------------------------------------------------------------------


def known_instance_ports(guests_root: Path, *, exclude_user: str | None = None) -> dict[str, int]:
    """Ports already claimed by other guests' instance.json files."""
    claimed: dict[str, int] = {}
    root = Path(guests_root)
    if not root.is_dir():
        return claimed
    for state_file in sorted(root.glob("*/instance/instance.json")):
        user = state_file.parent.parent.name
        if user == exclude_user:
            continue
        try:
            data = json.loads(state_file.read_text())
            claimed[user] = int(data["port"])
        except (OSError, ValueError, KeyError, TypeError):
            raise InstanceError(f"unreadable instance state {state_file}; fix it before allocating") from None
    return claimed


def allocate_port(user_id: str, claimed: Mapping[str, int], *, reserved: Iterable[int] = (),
                  base: int = DEFAULT_PORT_BASE, span: int = DEFAULT_PORT_SPAN) -> int:
    """Deterministic start (hash of user id) + linear probe over the window."""
    validate_user_id(user_id)
    taken = set(claimed.values()) | set(reserved) | ALWAYS_RESERVED_PORTS
    start = int(hashlib.sha256(user_id.encode()).hexdigest(), 16) % span
    for offset in range(span):
        port = base + (start + offset) % span
        if port not in taken:
            return port
    raise InstanceError("no free instance port left in the window")


def spec_for(user_id: str, guests_root: Path) -> InstanceSpec:
    """Spec of an already provisioned guest (port read from its instance.json)."""
    validate_user_id(user_id)
    state_file = Path(guests_root) / user_id / "instance" / "instance.json"
    try:
        port = int(json.loads(state_file.read_text())["port"])
    except (OSError, ValueError, KeyError, TypeError):
        raise InstanceError(f"{user_id}: {state_file} missing or unreadable; run `instance provision`") from None
    if not (1024 <= port <= 65535) or port in ALWAYS_RESERVED_PORTS:
        raise InstanceError(f"{state_file} records an invalid or reserved port {port}")
    return InstanceSpec(user_id=user_id, guests_root=Path(guests_root).resolve(), port=port)


# ---------------------------------------------------------------------------
# persistent identity (instance.json)
# ---------------------------------------------------------------------------


@dataclass
class InstanceState:
    user_id: str
    port: int
    model: str = FIXED_MODEL_ID
    root_session_id: str = ""
    runner_id: str = ""
    host_id: str = ""
    created_at: str = ""
    sub_sessions: str = "instance-contained"
    extra: dict[str, Any] = field(default_factory=dict)

    def to_json(self) -> dict[str, Any]:
        data = {
            "version": 1,
            "user_id": self.user_id,
            "port": self.port,
            "model": self.model,
            "root_session_id": self.root_session_id,
            "runner_id": self.runner_id,
            "host_id": self.host_id,
            "created_at": self.created_at,
            "sub_sessions": self.sub_sessions,
        }
        return data


def load_state(spec: InstanceSpec) -> InstanceState | None:
    path = spec.state_file
    if not path.exists():
        return None
    mode = stat.S_IMODE(path.stat().st_mode)
    if mode & 0o077:
        raise InstanceError(f"{path} must be 0600 (is {oct(mode)})")
    data = json.loads(path.read_text())
    state = InstanceState(
        user_id=str(data.get("user_id", "")),
        port=int(data.get("port", 0)),
        model=str(data.get("model", "")),
        root_session_id=str(data.get("root_session_id", "") or ""),
        runner_id=str(data.get("runner_id", "") or ""),
        host_id=str(data.get("host_id", "") or ""),
        created_at=str(data.get("created_at", "") or ""),
        sub_sessions=str(data.get("sub_sessions", "instance-contained")),
    )
    if state.user_id != spec.user_id:
        raise InstanceError(f"{path} belongs to {state.user_id!r}, not {spec.user_id!r}")
    if state.port != spec.port:
        raise InstanceError(f"{path} records port {state.port}, spec says {spec.port}")
    if state.model != FIXED_MODEL_ID:
        raise InstanceError(f"{path} records model {state.model!r}; only {FIXED_MODEL_ID} is allowed")
    if state.root_session_id:
        validate_session_id(state.root_session_id)
    return state


def save_state(spec: InstanceSpec, state: InstanceState) -> None:
    spec.instance_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    tmp = spec.state_file.with_suffix(".json.tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as fh:
        json.dump(state.to_json(), fh, indent=2, sort_keys=True)
        fh.write("\n")
        fh.flush()
        os.fsync(fh.fileno())
    os.chmod(tmp, 0o600)
    os.replace(tmp, spec.state_file)


# ---------------------------------------------------------------------------
# provisioning (filesystem only: no process, no network)
# ---------------------------------------------------------------------------


def provision(user_id: str, guests_root: Path, *, port: int | None = None,
              reserved: Iterable[int] = (), protected: Iterable[Path] | None = None,
              base: int = DEFAULT_PORT_BASE, span: int = DEFAULT_PORT_SPAN) -> tuple[InstanceSpec, InstanceState]:
    """Create (or re-validate) one guest's instance tree. Idempotent.

    An existing instance keeps its port and root session id; asking for a
    different port is an error rather than a silent re-allocation.
    """
    validate_user_id(user_id)
    guests_root = Path(guests_root).resolve()
    reserved = tuple(reserved)
    state_file = guests_root / user_id / "instance" / "instance.json"
    claimed = known_instance_ports(guests_root, exclude_user=user_id)
    if state_file.exists():
        existing_port = int(json.loads(state_file.read_text())["port"])
        if port is not None and port != existing_port:
            raise InstanceError(
                f"{user_id} already owns port {existing_port}; refusing to move it to {port}"
            )
        port = existing_port
    elif port is None:
        port = allocate_port(user_id, claimed, reserved=reserved, base=base, span=span)
    validate_port(port, reserved=reserved, base=base, span=span)
    for other, other_port in claimed.items():
        if other_port == port:
            raise InstanceError(f"port {port} is already used by guest {other!r}")

    spec = InstanceSpec(user_id=user_id, guests_root=guests_root, port=port)
    spec.check_isolation(protected)
    for path in spec.all_dirs():
        path.mkdir(parents=True, exist_ok=True, mode=0o700)
        if path.is_symlink():
            raise InstanceError(f"{path} is a symlink; refusing")
        os.chmod(path, 0o700)

    state = load_state(spec)
    if state is None:
        state = InstanceState(
            user_id=user_id,
            port=port,
            created_at=_dt.datetime.now(_dt.timezone.utc).isoformat(timespec="seconds"),
        )
        save_state(spec, state)
    write_instance_manifest(spec)
    return spec, state


def write_instance_manifest(spec: InstanceSpec) -> Path:
    """Kurisu-owned description of the instance (no secrets)."""
    manifest = spec.config_home / "kurisu-instance.json"
    data = {
        "user_id": spec.user_id,
        "bind": f"{spec.host}:{spec.port}",
        "auth": "single-user loopback (no credential; reachable only from host loopback "
                "and this guest's relay socket)",
        "database_uri": f"sqlite:///{spec.db_path}",
        "artifact_location": str(spec.artifacts_dir),
        "model": FIXED_MODEL_ID,
        "model_selection": "disabled",
        "managed_sandboxes": "none (no server config file is passed)",
    }
    fd = os.open(manifest, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as fh:
        json.dump(data, fh, indent=2, sort_keys=True)
        fh.write("\n")
    return manifest


# ---------------------------------------------------------------------------
# command plans
# ---------------------------------------------------------------------------


def _clean_passthrough(names: Iterable[str], source: Mapping[str, str]) -> dict[str, str]:
    out: dict[str, str] = {}
    for name in names:
        if not re.fullmatch(r"[A-Z][A-Z0-9_]{0,63}", name):
            raise InstanceError(f"invalid environment variable name {name!r}")
        if name.startswith("OMNIGENT_"):
            raise InstanceError(f"{name}: OMNIGENT_* variables are set by Kurisu, not passed through")
        if name in source:
            out[name] = source[name]
    return out


def server_plan(spec: InstanceSpec, *, omnigent_bin: str = "omnigent",
                provider_env: Iterable[str] = (), source_env: Mapping[str, str] | None = None
                ) -> tuple[list[str], dict[str, str]]:
    """argv + a from-scratch environment for the dedicated instance server.

    No ``--config`` file is passed, so no managed-sandbox provider, no shared
    preferences authority and no accounts admin are configured. The explicit
    ``--port``/``--database-uri``/``--artifact-location`` make Omnigent treat it
    as a *dedicated* server: it never consults or registers in the main
    ``local_server.pid``.
    """
    source_env = os.environ if source_env is None else source_env
    argv = [
        omnigent_bin, "server",
        "--host", spec.host,
        "--port", str(spec.port),
        "--database-uri", f"sqlite:///{spec.db_path}",
        "--artifact-location", str(spec.artifacts_dir),
        "--no-open",
    ]
    env = {
        "PATH": source_env.get("PATH", "/usr/bin:/bin"),
        "LANG": source_env.get("LANG", "C.UTF-8"),
        "HOME": str(spec.server_home),
        "OMNIGENT_CONFIG_HOME": str(spec.config_home),
        "OMNIGENT_DATA_DIR": str(spec.data_dir),
        "OMNIGENT_LOCAL_SINGLE_USER": "1",
        "OMNIGENT_ACCOUNTS_AUTO_OPEN": "0",
    }
    passthrough = _clean_passthrough(provider_env, source_env)
    env.update(passthrough)
    return argv, env


def server_sandbox_plan(spec: InstanceSpec, argv: Sequence[str], env: Mapping[str, str], *,
                        ro_paths: Iterable[str | Path], bwrap: str = "bwrap",
                        protected: Iterable[Path] | None = None) -> list[str]:
    """Wrap the instance server in bubblewrap.

    The guest's runner can drive this server's API as its owner, so the server
    must not be able to read operator files even if some server feature were
    misused. It gets: read-only system dirs, read-only Omnigent/Python install
    trees (``ro_paths``), its own instance dir read-write, host loopback network
    (``--share-net`` is required so it can bind 127.0.0.1:<port>), and nothing
    else: no operator home, no main ``.omnigent``, no other guest, no runtime
    sockets. ``--clearenv`` + explicit ``--setenv`` keeps the operator env out.
    """
    protected = tuple(Path(p).resolve() for p in (default_protected_paths() if protected is None else protected))
    guests_root = spec.guests_root.resolve()
    ro: list[str] = []
    for raw in ro_paths:
        path = Path(raw).resolve()
        if not path.is_absolute() or str(path) == "/":
            raise InstanceError(f"read-only path {raw!r} must be a specific absolute directory")
        if _is_within(path, guests_root) or _is_within(guests_root, path):
            raise InstanceError(f"read-only path {path} overlaps the guests root")
        for prot in protected:
            if _is_within(path, prot) or _is_within(prot, path):
                raise InstanceError(f"read-only path {path} overlaps protected main Omnigent state {prot}")
        if path.name in ("run", "home", "root") or str(path).startswith(("/run/user", "/var/run")):
            raise InstanceError(f"read-only path {path} is too broad or exposes runtime sockets")
        ro.append(str(path))
    wrapped = [
        bwrap, "--die-with-parent", "--new-session",
        "--unshare-user", "--unshare-pid", "--unshare-ipc", "--unshare-uts", "--unshare-cgroup-try",
        "--share-net",
        "--ro-bind", "/usr", "/usr",
        "--symlink", "usr/bin", "/bin", "--symlink", "usr/lib", "/lib", "--symlink", "usr/lib64", "/lib64",
        "--ro-bind-try", "/etc/ssl", "/etc/ssl", "--ro-bind-try", "/etc/ca-certificates", "/etc/ca-certificates",
        "--ro-bind-try", "/etc/resolv.conf", "/etc/resolv.conf", "--ro-bind-try", "/etc/hosts", "/etc/hosts",
        "--ro-bind-try", "/etc/localtime", "/etc/localtime",
        "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
    ]
    for path in ro:
        wrapped += ["--ro-bind", path, path]
    wrapped += ["--bind", str(spec.instance_dir), str(spec.instance_dir),
                "--chdir", str(spec.server_home), "--clearenv"]
    for key, value in env.items():
        wrapped += ["--setenv", key, value]
    wrapped += ["--", *argv]
    return wrapped


def runner_plan(spec: InstanceSpec, *, image: str, podman: str = "podman",
                memory: str = "2g", cpus: str = "1.50", pids: int = 256,
                extra_env: Mapping[str, str] | None = None) -> list[str]:
    """Foreground rootless-Podman argv for this guest's Omnigent runner.

    The container sees only its persistent mounts and the read-only io dir.
    ``--network=none`` removes every network route; the two Unix sockets in
    ``/run/kurisu-io`` are the only egress (instance API and allowlisted model).
    """
    if not image or not IMAGE_RE.fullmatch(image) or image.startswith("-"):
        raise InstanceError(f"invalid container image reference {image!r}")
    extra_env = dict(extra_env or {})
    for key in extra_env:
        if _FORBIDDEN_ENV_RE.search(key):
            raise InstanceError(f"{key}: credentials/Omnigent routing env may not enter the guest container")
    argv = [
        podman, "run", "--rm", "--replace",
        "--name", spec.container_name,
        "--hostname", f"kurisu-{spec.user_id}",
        "--userns=keep-id",
        "--read-only",
        "--tmpfs", "/tmp:rw,size=256m,mode=1777",
        "--network=none",
        "--ipc=private",
        "--pid=private",
        "--cap-drop=ALL",
        "--security-opt", "no-new-privileges",
        "--pids-limit", str(pids),
        "--memory", memory,
        "--memory-swap", memory,
        "--cpus", cpus,
        "--ulimit", "core=0:0",
        "--log-driver=none",
        "--workdir", CONTAINER_WORKSPACE,
    ]
    for host_path, container_path in spec.container_mounts():
        argv += ["--volume", f"{host_path}:{container_path}:rw,Z"]
    argv += ["--volume", f"{spec.io_dir}:{CONTAINER_IO}:ro"]
    env = {
        "HOME": CONTAINER_HOME,
        "XDG_CACHE_HOME": f"{CONTAINER_HOME}/.cache",
        "KURISU_USER_ID": spec.user_id,
        "KURISU_FIXED_MODEL": FIXED_MODEL_ID,
        **extra_env,
    }
    for key, value in env.items():
        argv += ["--env", f"{key}={value}"]
    loopback = f"{INSTANCE_HOST}:{spec.port}"
    argv += [
        image,
        "python3", "/opt/kurisu/instance-relay.py",
        "--forward", f"{loopback}={CONTAINER_IO}/omnigent.sock",
        "--forward", f"{INSTANCE_HOST}:11434={CONTAINER_IO}/model-proxy.sock",
        "--",
        "omnigent", "host", "--server", f"http://{loopback}", "--no-open", "--non-interactive",
    ]
    return argv


def assert_runner_plan_safe(argv: Sequence[str], spec: InstanceSpec) -> None:
    """Defense in depth: reject any argv that would widen the container's reach."""
    joined = " ".join(argv)
    banned = ("--privileged", "--network=host", "--net=host", "--pid=host", "--ipc=host",
              "--userns=host", "docker.sock", "podman.sock", "/run/user", "--device", "--cap-add")
    for item in banned:
        if item in joined:
            raise InstanceError(f"runner plan contains forbidden {item!r}")
    if "--network=none" not in argv:
        raise InstanceError("runner plan must use --network=none")
    allowed = {str(h) for h, _ in spec.container_mounts()} | {str(spec.io_dir)}
    for i, arg in enumerate(argv):
        if arg == "--volume":
            src, _, rest = argv[i + 1].partition(":")
            if src not in allowed:
                raise InstanceError(f"runner plan mounts {src}, which is not a guest path")
            if src == str(spec.io_dir) and not rest.endswith(":ro"):
                raise InstanceError("the io dir must be mounted read-only")


# ---------------------------------------------------------------------------
# host-side relay: guest unix socket -> fixed 127.0.0.1:<instance port>
# ---------------------------------------------------------------------------


class HostRelay:
    """Accept on ``spec.relay_socket``; connect only to ``spec.host:spec.port``."""

    def __init__(self, spec: InstanceSpec, *, connect_timeout: float = 10.0) -> None:
        self.spec = spec
        self.target = (spec.host, spec.port)
        self.connect_timeout = connect_timeout
        self._sock: socket.socket | None = None
        self._stop = threading.Event()
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        path = self.spec.relay_socket
        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        if path.exists() or path.is_symlink():
            if not stat.S_ISSOCK(path.lstat().st_mode):
                raise InstanceError(f"{path} exists and is not a socket")
            path.unlink()
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        old = os.umask(0o177)
        try:
            sock.bind(str(path))
        finally:
            os.umask(old)
        os.chmod(path, 0o600)
        sock.listen(32)
        sock.settimeout(0.5)
        self._sock = sock
        self._thread = threading.Thread(target=self._accept_loop, name="kurisu-host-relay", daemon=True)
        self._thread.start()

    def _accept_loop(self) -> None:
        assert self._sock is not None
        while not self._stop.is_set():
            try:
                client, _ = self._sock.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            threading.Thread(target=self._bridge, args=(client,), daemon=True).start()

    def _bridge(self, client: socket.socket) -> None:
        try:
            upstream = socket.create_connection(self.target, timeout=self.connect_timeout)
        except OSError:
            client.close()
            return
        upstream.settimeout(None)
        client.settimeout(None)

        def pump(src: socket.socket, dst: socket.socket) -> None:
            try:
                while True:
                    data = src.recv(65536)
                    if not data:
                        break
                    dst.sendall(data)
            except OSError:
                pass
            finally:
                try:
                    dst.shutdown(socket.SHUT_WR)
                except OSError:
                    pass

        t = threading.Thread(target=pump, args=(upstream, client), daemon=True)
        t.start()
        pump(client, upstream)
        t.join()
        client.close()
        upstream.close()

    def stop(self) -> None:
        self._stop.set()
        if self._sock is not None:
            self._sock.close()
            self._sock = None
        try:
            if self.spec.relay_socket.exists():
                self.spec.relay_socket.unlink()
        except OSError:
            pass

    def __enter__(self) -> "HostRelay":
        self.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self.stop()


# ---------------------------------------------------------------------------
# root session identity (create once, reuse forever)
# ---------------------------------------------------------------------------


class InstanceClient:
    """Minimal JSON client for a single-user loopback instance (no credential)."""

    def __init__(self, spec: InstanceSpec, transport=None, timeout: float = 15.0) -> None:
        from .omnigent_api import default_transport

        self.spec = spec
        self._transport = transport or (lambda m, u, h, b: default_transport(m, u, h, b, timeout))

    def _call(self, method: str, path: str, body: Mapping[str, Any] | None = None) -> tuple[int, Any]:
        url = f"{self.spec.base_url}{path}"
        headers = {"Accept": "application/json"}
        payload = None
        if body is not None:
            payload = json.dumps(body, separators=(",", ":")).encode()
            headers["Content-Type"] = "application/json"
        return self._transport(method, url, headers, payload)

    def get(self, path: str) -> tuple[int, Any]:
        return self._call("GET", path)

    def post(self, path: str, body: Mapping[str, Any]) -> tuple[int, Any]:
        return self._call("POST", path, body)


def _items(payload: Any, *keys: str) -> list[dict[str, Any]]:
    if isinstance(payload, list):
        return [x for x in payload if isinstance(x, dict)]
    if isinstance(payload, dict):
        for key in keys + ("data", "items"):
            value = payload.get(key)
            if isinstance(value, list):
                return [x for x in value if isinstance(x, dict)]
    return []


def ensure_root_session(spec: InstanceSpec, client: InstanceClient, *, agent_id: str,
                        title: str = "Kurisu guest root", recreate: bool = False) -> tuple[InstanceState, bool]:
    """Return (state, created). Reuses ``root_session_id`` across every restart.

    * recorded id exists on the instance -> reuse, never create;
    * recorded id is missing on the instance -> refuse (history would silently
      fork) unless the operator passes ``recreate``;
    * no recorded id -> create exactly one, bound to the single connected host
      (the guest container) with the fixed model, and persist it before returning.
    """
    state = load_state(spec)
    if state is None:
        raise InstanceError(f"{spec.user_id}: instance not provisioned (run `instance provision`)")
    if state.root_session_id and not recreate:
        status, _ = client.get(f"/v1/sessions/{state.root_session_id}")
        if status == 200:
            return state, False
        if status == 404:
            raise InstanceError(
                f"recorded root session {state.root_session_id} is missing from the instance DB; "
                "refusing to create a new one (pass --recreate only after operator review)"
            )
        raise InstanceError(f"instance returned HTTP {status} for the root session")

    status, hosts = client.get("/v1/hosts")
    if status != 200:
        raise InstanceError(f"cannot list instance hosts (HTTP {status}); is the runner connected?")
    host_list = _items(hosts, "hosts")
    if len(host_list) != 1:
        raise InstanceError(
            f"expected exactly one connected host (the guest container), found {len(host_list)}"
        )
    host_id = str(host_list[0].get("host_id") or host_list[0].get("id") or "")
    if not host_id:
        raise InstanceError("connected host has no id")
    status, created = client.post(
        "/v1/sessions",
        {
            "agent_id": agent_id,
            "title": title,
            "host_id": host_id,
            "workspace": CONTAINER_WORKSPACE,
            "model_override": FIXED_MODEL_ID,
            "labels": {"kurisu.user": spec.user_id, "kurisu.role": "root"},
        },
    )
    if status not in (200, 201) or not isinstance(created, dict):
        raise InstanceError(f"root session create failed (HTTP {status})")
    session_id = validate_session_id(str(created.get("id") or created.get("session_id") or ""))
    state.root_session_id = session_id
    state.host_id = host_id
    state.runner_id = str(created.get("runner_id") or "")
    save_state(spec, state)
    return state, True


# ---------------------------------------------------------------------------
# acceptance checks (pure functions over observed data)
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Check:
    name: str
    ok: bool
    detail: str


def check_listeners(ss_output: str, port: int) -> Check:
    """Parse ``ss -ltnH`` output: the instance port is bound on 127.0.0.1 only."""
    binds = []
    for line in ss_output.splitlines():
        cols = line.split()
        if len(cols) < 4:
            continue
        local = cols[3]
        host, _, p = local.rpartition(":")
        if p == str(port):
            binds.append(host.strip("[]"))
    if not binds:
        return Check("listener", False, f"nothing listens on port {port}")
    bad = [b for b in binds if b != INSTANCE_HOST]
    if bad:
        return Check("listener", False, f"port {port} is also bound on {bad} (must be {INSTANCE_HOST} only)")
    return Check("listener", True, f"port {port} bound on {INSTANCE_HOST} only")


def check_session_tree(sessions: Sequence[Mapping[str, Any]], root_id: str, root_runner: str) -> list[Check]:
    """Every session in the instance descends from the root, on the root's runner, with the fixed model.

    Fail closed: a missing parent/runner field is a failure, not a pass.
    """
    checks: list[Check] = []
    by_id = {str(s.get("id") or s.get("session_id") or ""): s for s in sessions}
    if root_id not in by_id:
        return [Check("root-present", False, f"root {root_id} not in the instance session list")]
    checks.append(Check("root-present", True, root_id))
    for sid, sess in by_id.items():
        # ancestry
        seen = set()
        cur = sid
        while cur and cur != root_id:
            if cur in seen or cur not in by_id:
                break
            seen.add(cur)
            cur = str(by_id[cur].get("parent_session_id") or "")
        checks.append(Check(f"tree:{sid}", cur == root_id,
                            "descends from root" if cur == root_id else "NOT a descendant of the guest root"))
        runner = sess.get("runner_id")
        if not root_runner:
            checks.append(Check(f"runner:{sid}", False, "root runner id unknown"))
        elif runner in (None, ""):
            checks.append(Check(f"runner:{sid}", False, "runner_id not reported"))
        else:
            checks.append(Check(f"runner:{sid}", runner == root_runner,
                                "container runner" if runner == root_runner else f"foreign runner {runner}"))
        model = sess.get("model") or sess.get("model_override")
        checks.append(Check(f"model:{sid}", model == FIXED_MODEL_ID,
                            f"model={model!r}"))
    return checks


def check_hosts(hosts: Sequence[Mapping[str, Any]], expected_host: str) -> Check:
    ids = [str(h.get("host_id") or h.get("id") or "") for h in hosts]
    if ids == [expected_host]:
        return Check("hosts", True, f"only the guest container host {expected_host}")
    return Check("hosts", False, f"hosts={ids}, expected exactly [{expected_host}]")


def static_preflight(spec: InstanceSpec, *, protected: Iterable[Path] | None = None,
                     source_env: Mapping[str, str] | None = None) -> list[Check]:
    """No process, no network: paths, permissions, identity file, server env."""
    checks: list[Check] = []
    try:
        spec.check_isolation(protected)
        checks.append(Check("paths", True, "all instance paths inside the guest root; main state untouched"))
    except InstanceError as exc:
        checks.append(Check("paths", False, str(exc)))
    for path in spec.all_dirs():
        if not path.is_dir():
            checks.append(Check(f"dir:{path.name}", False, f"{path} missing"))
            continue
        mode = stat.S_IMODE(path.stat().st_mode)
        checks.append(Check(f"dir:{path.relative_to(spec.guests_root)}", not (mode & 0o077), oct(mode)))
    try:
        state = load_state(spec)
        if state is None:
            checks.append(Check("state", False, "instance.json missing"))
        else:
            checks.append(Check("state", True, f"port={state.port} model={state.model}"))
            checks.append(Check(
                "root-session", bool(state.root_session_id),
                state.root_session_id or "not created yet (run `instance root-session` once)",
            ))
    except (InstanceError, ValueError) as exc:
        checks.append(Check("state", False, str(exc)))
    _, env = server_plan(spec, source_env=source_env or {})
    leaked = [k for k in env if _FORBIDDEN_ENV_RE.search(k)]
    checks.append(Check("server-env", not leaked, "clean" if not leaked else f"forbidden: {leaked}"))
    server_cfg = spec.config_home / "config.yaml"
    if server_cfg.exists() and re.search(r"(?m)^\s*(managed_sandboxes|sandbox)\s*:", server_cfg.read_text()):
        checks.append(Check("no-managed-sandbox", False, f"{server_cfg} configures a managed sandbox"))
    else:
        checks.append(Check("no-managed-sandbox", True, "no managed sandbox provider configured"))
    return checks
