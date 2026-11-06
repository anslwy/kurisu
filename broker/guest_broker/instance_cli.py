"""``guest-broker instance ...``: provision / plan / preflight / serve / runner /
root-session / acceptance for one guest's dedicated Omnigent instance.

Every command takes ``--guests-root`` and ``--user``. Only ``serve`` and
``runner`` start processes, and both stay in the foreground (Ctrl-C stops
them). Nothing here installs a unit, enables a service, or touches the main
Omnigent server/config or the Telegram bridge.
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import signal
import subprocess
import sys
import threading
from pathlib import Path

from . import instance as inst


def _print_checks(checks: list[inst.Check]) -> int:
    failed = 0
    for check in checks:
        print(f"[{'OK' if check.ok else 'FAIL'}] {check.name}: {check.detail}")
        failed += not check.ok
    print(f"\n{len(checks) - failed} passed, {failed} failed")
    return 0 if failed == 0 else 1


def _redact_env(env: dict[str, str]) -> dict[str, str]:
    return {k: ("<set>" if k not in ("PATH", "LANG", "HOME") and not k.startswith("OMNIGENT_") else v)
            for k, v in env.items()}


def cmd_provision(args: argparse.Namespace) -> int:
    spec, state = inst.provision(
        args.user, Path(args.guests_root), port=args.port, reserved=args.reserve_port,
    )
    print(json.dumps({
        "user_id": spec.user_id,
        "base_url": spec.base_url,
        "instance_dir": str(spec.instance_dir),
        "db": str(spec.db_path),
        "root_session_id": state.root_session_id or None,
        "model": state.model,
    }, indent=2))
    return 0


def cmd_plan(args: argparse.Namespace) -> int:
    spec = inst.spec_for(args.user, Path(args.guests_root))
    argv, env = inst.server_plan(spec, omnigent_bin=args.omnigent_bin, provider_env=args.provider_env)
    runner = inst.runner_plan(spec, image=args.image, podman=args.podman)
    inst.assert_runner_plan_safe(runner, spec)
    print("# instance server (host, foreground)")
    print(" ".join(f"{k}={shlex.quote(v)}" for k, v in _redact_env(env).items()))
    print(shlex.join(argv))
    print("\n# guest runner (container, foreground)")
    print(shlex.join(runner))
    return 0


def cmd_preflight(args: argparse.Namespace) -> int:
    spec = inst.spec_for(args.user, Path(args.guests_root))
    checks = inst.static_preflight(spec)
    runner = inst.runner_plan(spec, image=args.image or "localhost/kurisu-guest-runner:placeholder")
    try:
        inst.assert_runner_plan_safe(runner, spec)
        checks.append(inst.Check("runner-plan", True, "network=none, guest mounts only, io dir read-only"))
    except inst.InstanceError as exc:
        checks.append(inst.Check("runner-plan", False, str(exc)))
    claimed = inst.known_instance_ports(spec.guests_root, exclude_user=spec.user_id)
    clash = [u for u, p in claimed.items() if p == spec.port]
    checks.append(inst.Check("port-unique", not clash, f"port {spec.port}" + (f" also used by {clash}" if clash else "")))
    return _print_checks(checks)


def cmd_serve(args: argparse.Namespace) -> int:
    """Foreground: instance server + host relay (+ optional model proxy)."""
    spec = inst.spec_for(args.user, Path(args.guests_root))
    problems = [c for c in inst.static_preflight(spec) if not c.ok and c.name != "root-session"]
    if problems:
        return _print_checks(problems)
    argv, env = inst.server_plan(spec, omnigent_bin=args.omnigent_bin, provider_env=args.provider_env)
    if args.sandbox == "bwrap":
        argv = inst.server_sandbox_plan(spec, argv, env, ro_paths=args.ro_path, bwrap=args.bwrap)
    else:
        print("WARNING: --sandbox none runs the instance server unconfined on the host (dev only)",
              file=sys.stderr)
    proxy = None
    if args.model_upstream:
        from .model_proxy import ModelProxy

        proxy = ModelProxy(args.model_upstream, spec.model_socket,
                           ("/v1/chat/completions", "/v1/completions", "/api/chat"),
                           fixed_model=inst.FIXED_MODEL)
        proxy.start()
    log_path = spec.logs_dir / "server.log"
    fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    with os.fdopen(fd, "ab") as log:
        proc = subprocess.Popen(argv, env=env, stdout=log, stderr=subprocess.STDOUT,
                                cwd=str(spec.server_home), start_new_session=True)
    relay = inst.HostRelay(spec)
    relay.start()
    print(f"instance {spec.user_id}: server pid {proc.pid} on {spec.base_url}; "
          f"relay {spec.relay_socket}; log {log_path}", file=sys.stderr)
    stop = threading.Event()
    signal.signal(signal.SIGINT, lambda *_: stop.set())
    signal.signal(signal.SIGTERM, lambda *_: stop.set())
    try:
        while not stop.is_set() and proc.poll() is None:
            stop.wait(1.0)
    finally:
        relay.stop()
        if proxy is not None:
            proxy.stop()
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=20)
            except subprocess.TimeoutExpired:
                proc.kill()
    return proc.returncode or 0


def cmd_runner(args: argparse.Namespace) -> int:
    spec = inst.spec_for(args.user, Path(args.guests_root))
    argv = inst.runner_plan(spec, image=args.image, podman=args.podman)
    inst.assert_runner_plan_safe(argv, spec)
    if not spec.relay_socket.exists():
        print(f"[FAIL] {spec.relay_socket} missing: start `instance serve` first", file=sys.stderr)
        return 2
    print(shlex.join(argv), file=sys.stderr)
    return subprocess.call(argv)


def cmd_root_session(args: argparse.Namespace) -> int:
    spec = inst.spec_for(args.user, Path(args.guests_root))
    state, created = inst.ensure_root_session(
        spec, inst.InstanceClient(spec), agent_id=args.agent_id, recreate=args.recreate,
    )
    print(json.dumps({"root_session_id": state.root_session_id, "created": created,
                      "host_id": state.host_id}, indent=2))
    print("\nbind the broker registry to it (idempotent on reuse):\n"
          f"  guest-broker provision --config <broker.local.yaml> --user {spec.user_id} "
          f"--session-id {state.root_session_id}", file=sys.stderr)
    return 0


def cmd_acceptance(args: argparse.Namespace) -> int:
    """Read-only live checks against the running instance (no message is sent)."""
    spec = inst.spec_for(args.user, Path(args.guests_root))
    state = inst.load_state(spec)
    checks: list[inst.Check] = []
    if state is None or not state.root_session_id:
        print("[FAIL] no recorded root session", file=sys.stderr)
        return 1
    try:
        ss = subprocess.run(["ss", "-ltnH"], capture_output=True, text=True, timeout=10).stdout
        checks.append(inst.check_listeners(ss, spec.port))
    except (OSError, subprocess.SubprocessError) as exc:
        checks.append(inst.Check("listener", False, f"cannot run ss: {exc}"))
    client = inst.InstanceClient(spec)
    status, root = client.get(f"/v1/sessions/{state.root_session_id}")
    checks.append(inst.Check("root-reused", status == 200, f"HTTP {status} for recorded root"))
    root_runner = str((root or {}).get("runner_id") or state.runner_id) if isinstance(root, dict) else state.runner_id
    status, hosts = client.get("/v1/hosts")
    checks.append(inst.check_hosts(inst._items(hosts, "hosts"), state.host_id) if status == 200
                  else inst.Check("hosts", False, f"HTTP {status}"))
    status, sessions = client.get("/v1/sessions?limit=1000&kind=any&include_archived=true")
    if status == 200:
        checks.extend(inst.check_session_tree(inst._items(sessions, "sessions"), state.root_session_id, root_runner))
    else:
        checks.append(inst.Check("sessions", False, f"HTTP {status}"))
    return _print_checks(checks)


def register(sub: argparse._SubParsersAction) -> None:
    p = sub.add_parser("instance", help="dedicated per-guest Omnigent instance (operator)")
    isub = p.add_subparsers(dest="instance_command", required=True)

    def common(sp: argparse.ArgumentParser) -> argparse.ArgumentParser:
        sp.add_argument("--guests-root", required=True, help="absolute guests root, e.g. /path/to/omni/guests")
        sp.add_argument("--user", required=True, help="guest id, e.g. pilot-001")
        return sp

    sp = common(isub.add_parser("provision", help="create/validate the instance tree (no processes)"))
    sp.add_argument("--port", type=int, default=None, help="explicit port; default: allocated, then persisted")
    sp.add_argument("--reserve-port", type=int, action="append", default=[],
                    help="port never to allocate (repeatable; e.g. the main server's port)")
    sp.set_defaults(func=cmd_provision)

    for name, func, helptext in (
        ("plan", cmd_plan, "print server + runner commands (nothing runs)"),
        ("serve", cmd_serve, "FOREGROUND: run the instance server + host relay"),
    ):
        sp = common(isub.add_parser(name, help=helptext))
        sp.add_argument("--omnigent-bin", default="omnigent")
        sp.add_argument("--provider-env", action="append", default=[],
                        help="name of a provider env var to pass to the instance (value never printed)")
        if name == "plan":
            sp.add_argument("--image", required=True)
            sp.add_argument("--podman", default="podman")
        else:
            sp.add_argument("--model-upstream", default="",
                            help="loopback model origin for the allowlisting proxy, e.g. http://127.0.0.1:11434")
            sp.add_argument("--sandbox", choices=("bwrap", "none"), default="bwrap",
                            help="confine the instance server (default bwrap; none is dev only)")
            sp.add_argument("--bwrap", default="bwrap")
            sp.add_argument("--ro-path", action="append", default=[],
                            help="read-only install tree the server needs (omnigent tool dir, python)")
        sp.set_defaults(func=func)

    sp = common(isub.add_parser("preflight", help="static checks: paths, perms, identity, plans"))
    sp.add_argument("--image", default="")
    sp.set_defaults(func=cmd_preflight)

    sp = common(isub.add_parser("runner", help="FOREGROUND: run the guest runner container"))
    sp.add_argument("--image", required=True)
    sp.add_argument("--podman", default="podman")
    sp.set_defaults(func=cmd_runner)

    sp = common(isub.add_parser("root-session", help="create once / reuse the root session"))
    sp.add_argument("--agent-id", required=True)
    sp.add_argument("--recreate", action="store_true", help="operator-reviewed only")
    sp.set_defaults(func=cmd_root_session)

    sp = common(isub.add_parser("acceptance", help="read-only live isolation checks"))
    sp.set_defaults(func=cmd_acceptance)
