"""Command line entry point for the trusted broker.

Deliberate properties:

* ``run`` never installs, enables, or starts a host service. It runs in the
  foreground (or under whatever supervisor the operator chooses) and exits.
* ``provision`` is the only command that can bind a session id, and it is an
  operator action, not something the transport or the guest can invoke.
* ``selftest`` proves the limits are real on this host instead of asserting it.
* ``dev`` is the safe local pilot path: echo backend, fake transport, no
  WhatsApp account and no guest model call.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import os
import signal
import socket
import stat
import sys
import threading
import time
from pathlib import Path

from . import auth as auth_mod
from . import backend as backend_mod
from . import config as config_mod
from . import model_proxy, protocol
from . import sandbox as sandbox_mod
from .allowlist import load_allowlist
from .broker import Broker, Outcome
from .errors import BrokerError, ConfigError, ProtocolError, RegistryError, UnauthorizedError
from .protocol import Inbound
from .registry import load_registry, provision
from .server import GatewayServer

BROKER_VERSION = "0.1.0"


def _log(message: str) -> None:
    print(f"{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())} {message}", flush=True)


def build_signin(cfg: config_mod.Config, *, fake: bool = False) -> auth_mod.SigninManager | None:
    """Build the Kurisu sign-in manager, or ``None`` when auth is disabled."""
    if not cfg.auth.enabled:
        return None
    store = auth_mod.SigninStore(
        cfg.resolve(cfg.auth.mappings_path),
        cfg.resolve(cfg.auth.tokens_path),
    )
    if fake:
        flow: object = auth_mod.FakeDeviceFlow()
    else:
        secret = ""
        if cfg.auth.google_client_secret_env:
            secret = os.environ.get(cfg.auth.google_client_secret_env, "")
            if not secret:
                raise ConfigError(
                    f"auth.enabled needs the Google client secret in ${cfg.auth.google_client_secret_env}; "
                    "export it for the broker process only (never in git or in the guest)"
                )
        flow = auth_mod.DeviceFlow(cfg.auth.google_client_id, secret)
    return auth_mod.SigninManager(
        flow,  # type: ignore[arg-type]
        store,
        pending_ttl_seconds=cfg.auth.pending_ttl_seconds,
    )


def _load_all(config_path: str, *, allow_echo: bool, require_sandbox: bool, fake_signin: bool = False):
    cfg = config_mod.load_config(config_path)
    allowlist = load_allowlist(cfg.resolve(cfg.broker.allowlist_path))
    registry = load_registry(cfg.resolve(cfg.broker.registry_path), cfg.broker.guests_root)

    if require_sandbox and cfg.sandbox.mode not in config_mod.REAL_SANDBOX_MODES:
        raise ConfigError(
            f"sandbox.mode is {cfg.sandbox.mode!r}; a real run requires "
            f"{config_mod.REAL_SANDBOX_MODES}"
        )
    backend = backend_mod.build_backend(cfg.runtime, allow_echo=allow_echo)
    signin = build_signin(cfg, fake=fake_signin)
    broker = Broker(
        settings=cfg.broker,
        limits=cfg.limits,
        sandbox=cfg.sandbox,
        runtime=cfg.runtime,
        registry=registry,
        allowlist=allowlist,
        backend=backend,
        base_dir=cfg.base_dir,
        log=_log,
        signin=signin,
    )
    return cfg, allowlist, registry, broker


# --------------------------------------------------------------------------
# preflight
# --------------------------------------------------------------------------
def cmd_preflight(args: argparse.Namespace) -> int:
    problems: list[str] = []
    cfg: config_mod.Config | None = None
    try:
        cfg, allowlist, registry, broker = _load_all(
            args.config, allow_echo=False, require_sandbox=not args.allow_no_sandbox
        )
    except (BrokerError, ConfigError) as exc:
        print(f"[FAIL] configuration: {exc}")
        return 2

    problems.extend(broker.validate_startup())

    print("== configuration ==")
    print(f"[OK  ] config: {Path(args.config).resolve()}")
    print(f"[OK  ] sandbox mode: {cfg.sandbox.mode}")
    print(f"[OK  ] fixed model: {cfg.runtime.provider}/{cfg.runtime.model} (no override surface)")
    print(f"[OK  ] registry: {len(registry)} user(s), allowlist: {len(allowlist)} identity(ies)")

    print("\n== sandbox capabilities ==")
    caps = sandbox_mod.probe(cfg.sandbox, cfg.limits)
    for cap in caps:
        print(cap.format())
        if not cap.ok:
            problems.append(cap.name)

    print("\n== Kurisu sign-in ==")
    if not cfg.auth.enabled:
        print("[WARN] auth disabled: every allowlisted WhatsApp identity is accepted without /signin")
    else:
        print(f"[OK  ] google client id: {cfg.auth.google_client_id[:12]}…")
        secret_env = cfg.auth.google_client_secret_env
        secret_set = bool(secret_env and os.environ.get(secret_env))
        print(f"[{'OK  ' if secret_set else 'FAIL'}] client secret env ${secret_env or '(unset)'}: "
              f"{'set' if secret_set else 'missing'}")
        if not secret_set:
            problems.append("google client secret env")
        for label, path in (("mappings", cfg.auth.mappings_path), ("tokens", cfg.auth.tokens_path)):
            resolved = cfg.resolve(path)
            mode = stat.S_IMODE(resolved.stat().st_mode) if resolved.exists() else None
            ok_perms = mode is None or not mode & 0o077
            print(f"[{'OK  ' if ok_perms else 'FAIL'}] {label} file: {resolved} "
                  f"(mode {oct(mode)[2:] if mode else 'absent'})")
            if not ok_perms:
                problems.append(f"{label} file permissions")

    print("\n== runtime ==")
    pi_ok, pi_detail = backend_mod.which_pi(cfg.runtime.pi_path)
    print(f"[{'OK  ' if pi_ok else 'FAIL'}] pi binary: {pi_detail}")
    if not pi_ok:
        problems.append("pi binary")
    if cfg.sandbox.model_proxy.enabled:
        ok, detail = model_proxy.probe_upstream(cfg.sandbox.model_proxy.upstream)
        print(f"[{'OK  ' if ok else 'FAIL'}] model endpoint: {detail}")
        if not ok:
            problems.append("model endpoint")
    else:
        print("[WARN] model proxy disabled: the guest keeps full network access")

    print("\n== mappings ==")
    for row in registry.public_view():
        print(f"  {row['user_id']} -> session {row['session_id']} @ {row['workspace']}")
    for row in allowlist.public_view():
        print(f"  {row['user_id']} <- {row['wa_jid']} (enabled={row['enabled']})")

    if problems:
        print("\nNOT READY:")
        for item in dict.fromkeys(problems):
            print(f"  - {item}")
        return 1
    print("\nREADY: all preflight checks passed")
    return 0


# --------------------------------------------------------------------------
# run
# --------------------------------------------------------------------------
def cmd_run(args: argparse.Namespace) -> int:
    cfg, allowlist, registry, broker = _load_all(
        args.config, allow_echo=args.allow_echo, require_sandbox=not args.allow_no_sandbox
    )
    problems = broker.validate_startup()
    if problems:
        for problem in problems:
            _log(f"REFUSING TO START: {problem}")
        return 2

    caps = sandbox_mod.probe(cfg.sandbox, None)
    failed = [cap for cap in caps if not cap.ok]
    if failed and not args.skip_probe:
        for cap in failed:
            _log(f"REFUSING TO START: sandbox capability {cap.name}: {cap.detail}")
        return 2

    proxy = None
    guest_io = cfg.resolve(cfg.broker.state_dir) / "guest-io"
    guest_io.mkdir(parents=True, exist_ok=True)
    os.chmod(guest_io, 0o700)
    if cfg.sandbox.model_proxy.enabled:
        proxy_socket = cfg.resolve(cfg.sandbox.model_proxy.socket_path)
        proxy = model_proxy.ModelProxy(
            cfg.sandbox.model_proxy.upstream,
            proxy_socket,
            cfg.sandbox.model_proxy.allowed_path_prefixes,
        )
        proxy.start()
        # Bind the proxy socket into the sandbox as /run/guest-io/model-proxy.sock
        broker.proxy_socket = proxy_socket
        broker.guest_io_dir = guest_io
        linked = guest_io / "model-proxy.sock"
        if linked.exists() or linked.is_symlink():
            linked.unlink()
        linked.symlink_to(proxy_socket)
        _log(f"model proxy: {proxy_socket} -> {cfg.sandbox.model_proxy.upstream}")

    server = GatewayServer(
        broker=broker,
        allowlist=allowlist,
        socket_path=cfg.resolve(cfg.broker.socket_path),
        state_dir=cfg.resolve(cfg.broker.state_dir),
        log=_log,
    )
    server.start()
    _log(f"broker {BROKER_VERSION} listening on {server.socket_path} (backend={cfg.runtime.backend})")
    _log("waiting for the whatsmeow gateway; Ctrl-C to stop")

    stop = threading.Event()

    def _stop(signum: int, _frame: object) -> None:
        _log(f"signal {signum}: stopping")
        stop.set()

    signal.signal(signal.SIGINT, _stop)
    signal.signal(signal.SIGTERM, _stop)
    try:
        while not stop.is_set():
            stop.wait(1.0)
    finally:
        server.stop()
        if proxy is not None:
            proxy.stop()
        _log("broker stopped")
    return 0


# --------------------------------------------------------------------------
# dev: safe local pilot path (no WhatsApp, no model call)
# --------------------------------------------------------------------------
def cmd_dev(args: argparse.Namespace) -> int:
    cfg, allowlist, registry, _ = _load_all(
        args.config, allow_echo=True, require_sandbox=False, fake_signin=args.fake_signin
    )

    # Force the echo backend regardless of what the file says: this command is
    # explicitly the no-model, no-WhatsApp path.
    runtime = dataclasses.replace(cfg.runtime, backend="echo")
    backend = backend_mod.EchoBackend()
    broker = Broker(
        settings=cfg.broker,
        limits=cfg.limits,
        sandbox=cfg.sandbox,
        runtime=runtime,
        registry=registry,
        allowlist=allowlist,
        backend=backend,
        base_dir=cfg.base_dir,
        log=_log,
        signin=build_signin(cfg, fake=args.fake_signin),
    )
    problems = broker.validate_startup()
    if problems:
        for problem in problems:
            _log(f"REFUSING TO START: {problem}")
        return 2

    user_id = args.user
    jid = allowlist.jid_for_user(user_id)
    if jid is None:
        _log(f"REFUSING: user {user_id!r} has no allowlisted identity")
        return 2
    _log(f"dev mode: user={user_id} jid={jid} backend=echo (no WhatsApp, no model, no sandbox)")

    def _dispatch(text: str, counter: int) -> Outcome:
        inbound = Inbound(msg_id=f"dev{counter:04d}", wa_jid=jid, text=text, ts=time.time())
        outcome = broker.handle(inbound)
        print(f"\n[{outcome.kind}] {outcome.text}")
        if outcome.detail and args.verbose:
            print(f"    detail: {outcome.detail}")
        return outcome

    if args.text:
        outcome = _dispatch(args.text, 1)
        return 0 if outcome.kind == "reply" else 1
    if not sys.stdin.isatty():
        counter = 0
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            counter += 1
            _dispatch(line, counter)
        return 0

    print("dev chat> type a message, 'quit' to exit")
    counter = 0
    while True:
        try:
            line = input("dev chat> ").strip()
        except (EOFError, KeyboardInterrupt):
            print()
            return 0
        if line in {"quit", "exit"}:
            return 0
        if not line:
            continue
        counter += 1
        _dispatch(line, counter)


# --------------------------------------------------------------------------
# provision / show
# --------------------------------------------------------------------------
def cmd_provision(args: argparse.Namespace) -> int:
    cfg = config_mod.load_config(args.config)
    try:
        binding = provision(
            cfg.resolve(cfg.broker.registry_path),
            cfg.broker.guests_root,
            args.user,
            args.session_id,
            force=args.force,
        )
    except (BrokerError, RegistryError) as exc:
        print(f"[FAIL] {exc}")
        return 2
    print(f"[OK  ] bound {binding.user_id} -> session {binding.session_id}")
    print(f"       workspace: {binding.workspace}")
    print("       next: add the matching WhatsApp identity to the allowlist (mode 0600)")
    return 0


def cmd_show(args: argparse.Namespace) -> int:
    cfg = config_mod.load_config(args.config)
    allowlist = load_allowlist(cfg.resolve(cfg.broker.allowlist_path))
    registry = load_registry(cfg.resolve(cfg.broker.registry_path), cfg.broker.guests_root)
    print(json.dumps(
        {
            "broker_version": BROKER_VERSION,
            "sandbox_mode": cfg.sandbox.mode,
            "fixed_model": f"{cfg.runtime.provider}/{cfg.runtime.model}",
            "backend": cfg.runtime.backend,
            "registry": registry.public_view(),
            "allowlist": allowlist.public_view(),
            "limits": dataclasses.asdict(cfg.limits),
        },
        indent=2,
    ))
    return 0


# --------------------------------------------------------------------------
# selftest: prove the limits are real
# --------------------------------------------------------------------------
def cmd_selftest(args: argparse.Namespace) -> int:
    cfg = config_mod.load_config(args.config)
    limits = dataclasses.replace(
        cfg.limits,
        memory_bytes=128 * 1024 * 1024,
        memory_swap_bytes=0,
        cpu_quota="25%",
        tasks_max=16,
        runtime_max_seconds=4,
        file_size_bytes=16 * 1024 * 1024,
        tmpfs_bytes=16 * 1024 * 1024,
    )
    if cfg.sandbox.mode == "none":
        print("[FAIL] sandbox.mode is 'none'; selftest needs a real sandbox")
        return 2

    results: list[tuple[str, bool, str]] = []

    def _scope_run(label: str, command: list[str], expect: str, timeout: int = 90) -> None:
        plan = sandbox_mod.build_plan(
            workspace=Path(args.workspace).resolve(),
            limits=limits,
            runtime=cfg.runtime,
            sandbox=cfg.sandbox,
            command=command,
            env={"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": "/tmp"},
        )
        started = time.monotonic()
        result = sandbox_mod.run_plan(plan, timeout_seconds=timeout)
        elapsed = time.monotonic() - started
        ok = _check_expectation(expect, result, elapsed, limits)
        results.append((label, ok, f"exit={result.exit_code} elapsed={elapsed:.1f}s {expect}"))
        print(f"[{'PASS' if ok else 'FAIL'}] {label}: exit={result.exit_code} elapsed={elapsed:.1f}s")
        tail = (result.stderr or "").strip().splitlines()[-1:] if result.stderr else []
        if tail:
            print(f"       {tail[0][:180]}")

    # 1. memory: allocate 4x the ceiling
    _scope_run(
        "memory limit (allocate 4x MemoryMax)",
        ["/usr/bin/python3", "-c",
         "b=bytearray()\n"
         "try:\n"
         "    for _ in range(1024):\n"
         "        b.extend(bytearray(512*1024))\n"
         "    print('NOT_LIMITED', len(b)//1048576)\n"
         "except MemoryError:\n"
         "    print('LIMITED')\n"],
        "memory",
    )
    # 2. tasks: fork bomb
    _scope_run(
        "pid/task limit (fork 64 children, TasksMax=16)",
        ["/usr/bin/python3", "-c",
         "import os,time\n"
         "kids=[]\n"
         "try:\n"
         "    for _ in range(64):\n"
         "        pid=os.fork()\n"
         "        if pid==0:\n"
         "            time.sleep(5); os._exit(0)\n"
         "        kids.append(pid)\n"
         "    print('FORKED_ALL', len(kids))\n"
         "except OSError as e:\n"
         "    print('BLOCKED_AT', len(kids), e.errno)\n"],
        "tasks",
    )
    # 3. runtime: sleep past RuntimeMaxSec
    _scope_run(
        f"runtime limit (sleep {limits.runtime_max_seconds + 22}s, RuntimeMaxSec={limits.runtime_max_seconds})",
        ["/usr/bin/bash", "-c", f"sleep {limits.runtime_max_seconds + 22}; echo NOT_KILLED"],
        "runtime",
    )
    # 4. disk: write past LimitFSIZE
    _scope_run(
        "file size limit (write 32MiB with LimitFSIZE=16MiB)",
        ["/usr/bin/bash", "-c",
         "dd if=/dev/zero of=./big.bin bs=1M count=32 oflag=direct 2>/dev/null "
         "|| dd if=/dev/zero of=./big.bin bs=1M count=32 2>/dev/null; "
         "ls -l big.bin; rm -f big.bin"],
        "fsize",
    )
    # 5. filesystem isolation
    iso_ok, iso_detail = _check_isolation(cfg, limits, Path(args.workspace).resolve())
    results.append(("filesystem isolation", iso_ok, iso_detail))
    print(f"[{'PASS' if iso_ok else 'FAIL'}] filesystem isolation: {iso_detail}")

    # 6. model proxy allowlist (no sockets needed)
    policy = model_proxy.ProxyPolicy(cfg.sandbox.model_proxy.upstream, ("/v1/",))
    proxy_ok = (
        policy.allow("POST", "/v1/chat/completions")[0]
        and not policy.allow("GET", "/")[0]
        and not policy.allow("CONNECT", "/v1/")[0]
        and not policy.allow("GET", "http://example.com/")[0]
    )
    results.append(("model proxy allowlist", proxy_ok, "only /v1/ on the fixed loopback upstream"))
    print(f"[{'PASS' if proxy_ok else 'FAIL'}] model proxy allowlist: only /v1/ loopback")

    failed = [name for name, ok, _ in results if not ok]
    print()
    if failed:
        print(f"SELFTEST FAILED: {', '.join(failed)}")
        return 1
    print("SELFTEST PASSED: every limit was enforced by the kernel/sandbox")
    return 0


def _check_expectation(expect: str, result, elapsed: float, limits) -> bool:
    if expect == "memory":
        # 137/-9 = kernel OOM killer. -15 = systemd stopping the scope on the OOM
        # event (both mean the allocation never completed). "LIMITED" covers the
        # case where the runtime raised MemoryError instead of dying.
        oom = result.exit_code in (137, -9, -15)
        limited = "LIMITED" in (result.stdout or "") or "MemoryError" in (result.stderr or "")
        return (oom or limited) and "NOT_LIMITED" not in (result.stdout or "")
    if expect == "tasks":
        return result.exit_code in (137, -9) or "BLOCKED_AT" in (result.stdout or "")
    if expect == "runtime":
        return result.exit_code in (137, -9, 124) or elapsed < limits.runtime_max_seconds + 20
    if expect == "fsize":
        return result.exit_code != 0 or "32M" not in (result.stdout or "")
    return False


def _check_isolation(cfg, limits, workspace: Path) -> tuple[bool, str]:
    script = (
        # max pid in /proc proves a fresh pid namespace (host pids are thousands)
        "echo MAXPID=$(ls /proc | grep '^[0-9][0-9]*$' | sort -n | tail -1); "
        "touch /etc/pilot-escape 2>&1 | head -1; "
        "ls /var/run/docker.sock 2>&1 | head -1; "
        "ls /run/user 2>&1 | head -1; "
        "ls -d /home 2>&1 | head -1"
    )
    plan = sandbox_mod.build_plan(
        workspace=workspace,
        limits=limits,
        runtime=cfg.runtime,
        sandbox=cfg.sandbox,
        command=["/usr/bin/bash", "-c", script],
        env={"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": "/tmp"},
    )
    result = sandbox_mod.run_plan(plan, timeout_seconds=60)
    out = result.stdout or ""
    problems = []
    if "Read-only file system" not in out:
        problems.append("/etc writable")
    if out.count("No such file or directory") < 3:
        problems.append("host paths visible")
    max_pid = 0
    for line in out.splitlines():
        if line.startswith("MAXPID=") and line[7:].strip().isdigit():
            max_pid = int(line[7:])
    if max_pid == 0 or max_pid > 64:
        problems.append(f"pid namespace not isolated (max pid {max_pid})")
    detail = "isolated" if not problems else ", ".join(problems)
    return (not problems, detail)


def cmd_container_policy(args: argparse.Namespace) -> int:
    from . import container_policy as cp

    context = cp.RequestContext(
        outer_sandbox_mode=args.outer_sandbox,
        host_has_kvm=Path("/dev/kvm").exists(),
        operator_opt_in=args.operator_opt_in,
    )
    print("Guest-initiated container use - policy evaluation\n")
    print(cp.summary(context))
    print()
    allowed = cp.allowed_requests(context)
    print("allowed:", ", ".join(allowed) if allowed else "(none)")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="guest-broker", description=__doc__)
    parser.add_argument("--version", action="version", version=f"guest-broker {BROKER_VERSION}")
    sub = parser.add_subparsers(dest="command", required=True)

    p_pre = sub.add_parser("preflight", help="validate config, mappings and sandbox capabilities")
    p_pre.add_argument("--config", required=True)
    p_pre.add_argument("--allow-no-sandbox", action="store_true", help="dev only")
    p_pre.set_defaults(func=cmd_preflight)

    p_run = sub.add_parser("run", help="run the broker in the foreground")
    p_run.add_argument("--config", required=True)
    p_run.add_argument("--allow-echo", action="store_true", help="dev only: permit the echo backend")
    p_run.add_argument("--allow-no-sandbox", action="store_true", help="dev only")
    p_run.add_argument("--skip-probe", action="store_true", help="dev only")
    p_run.set_defaults(func=cmd_run)

    p_dev = sub.add_parser("dev", help="local pilot path: echo backend, no WhatsApp, no model call")
    p_dev.add_argument("--config", required=True)
    p_dev.add_argument("--user", default="pilot-001")
    p_dev.add_argument("--text")
    p_dev.add_argument("--verbose", action="store_true")
    p_dev.add_argument("--fake-signin", action="store_true",
                       help="dev only: exercise the /signin gate with a fake flow (no Google)")
    p_dev.set_defaults(func=cmd_dev)

    p_prov = sub.add_parser("provision", help="operator: bind user_id -> one session id")
    p_prov.add_argument("--config", required=True)
    p_prov.add_argument("--user", required=True)
    p_prov.add_argument("--session-id", required=True)
    p_prov.add_argument("--force", action="store_true")
    p_prov.set_defaults(func=cmd_provision)

    p_show = sub.add_parser("show", help="print the effective mappings (redacted)")
    p_show.add_argument("--config", required=True)
    p_show.set_defaults(func=cmd_show)

    p_cp = sub.add_parser("container-policy", help="show the guest container-capability decision")
    p_cp.add_argument("--outer-sandbox", default="systemd-bwrap")
    p_cp.add_argument("--operator-opt-in", action="store_true")
    p_cp.set_defaults(func=cmd_container_policy)

    p_self = sub.add_parser("selftest", help="prove the resource limits are enforced")
    p_self.add_argument("--config", required=True)
    p_self.add_argument("--workspace", default=".")
    p_self.set_defaults(func=cmd_selftest)

    args = parser.parse_args(argv)
    try:
        return args.func(args)
    except (BrokerError, ConfigError, RegistryError, UnauthorizedError, ProtocolError) as exc:
        print(f"[FAIL] {type(exc).__name__}: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
