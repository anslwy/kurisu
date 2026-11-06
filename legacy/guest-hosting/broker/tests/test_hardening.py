"""Regression tests for the review findings A, G, H, I, J, M, N + watchdog."""

from __future__ import annotations

import dataclasses
import http.server
import json
import os
import socket
import socketserver
import subprocess
import sys
import threading
import time
from pathlib import Path
from types import SimpleNamespace

import pytest

from guest_broker import backend as backend_mod
from guest_broker import cli as cli_mod
from guest_broker import model_proxy
from guest_broker import sandbox as sandbox_mod
from guest_broker.allowlist import normalize_jid
from guest_broker.auth import FakeDeviceFlow, SigninManager, SigninStore
from guest_broker.backend import EchoBackend, PiCliBackend, prepare_guest_state
from guest_broker.broker import Broker
from guest_broker.errors import SessionBackendError
from guest_broker.protocol import Inbound

from .conftest import GUEST_JID, make_config
from .test_sandbox import requires_sandbox

FIXED_MODEL = "deepseek-v4.1-flash:cloud"


# ---------------------------------------------------------------- helpers
class _Upstream(http.server.BaseHTTPRequestHandler):
    seen: list[dict] = []

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length)
        try:
            parsed = json.loads(body.decode())
        except Exception:
            parsed = {"_raw": "<not json>"}
        _Upstream.seen.append({"path": self.path, "body": parsed})
        payload = b'{"choices":[{"message":{"content":"pong"}}]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args: object) -> None:
        pass


@pytest.fixture
def upstream():
    _Upstream.seen = []
    server = socketserver.TCPServer(("127.0.0.1", 0), _Upstream)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    host, port = server.server_address
    yield f"http://{host}:{port}"
    server.shutdown()
    server.server_close()


def _free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


CLIENT_SNIPPET = (
    "import json,sys,urllib.request\n"
    "port=int(sys.argv[1])\n"
    "req=urllib.request.Request('http://127.0.0.1:%d/v1/chat/completions' % port,"
    " data=json.dumps({'model':'kimi-k3.1-pro:cloud','messages':[{'role':'user','content':'hi'}]}).encode(),"
    " headers={'Content-Type':'application/json'})\n"
    "print(urllib.request.urlopen(req, timeout=10).read().decode())\n"
)


# ------------------------------------------------- A: shim reaches the proxy
def test_shim_is_placed_before_pi_in_the_bwrap_plan(loaded, tmp_path, monkeypatch):
    cfg = loaded["cfg"]
    guest_io = tmp_path / "guest-io"
    guest_io.mkdir()
    captured = {}

    def fake_run(plan, **kwargs):
        captured["plan"] = plan
        return sandbox_mod.RunResult(exit_code=0, stdout="ok", stderr="", duration_seconds=0.0)

    monkeypatch.setattr(backend_mod.sandbox_mod, "run_plan", fake_run)
    binding = loaded["registry"].get("pilot-001")
    backend = PiCliBackend()
    sandbox = dataclasses.replace(
        cfg.sandbox, model_proxy=dataclasses.replace(cfg.sandbox.model_proxy, enabled=True)
    )
    result = backend.run(
        binding, "hello",
        limits=cfg.limits, sandbox=sandbox, runtime=cfg.runtime,
        proxy_socket=guest_io / "model-proxy.sock", guest_io_dir=guest_io,
    )
    assert result.exit_code == 0
    argv = list(captured["plan"].argv)
    shim_index = next(i for i, tok in enumerate(argv) if tok.endswith("model-shim.py"))
    pi_index = argv.index(cfg.runtime.pi_path)
    assert shim_index < pi_index, "the shim must wrap pi, not follow it"
    assert "/run/guest-io/model-proxy.sock" in argv
    assert "--upstream-unix" in argv
    # both helper mounts must exist inside the namespace
    assert any(":/run/guest-runtime" in tok or tok == "/run/guest-runtime" for tok in argv)
    assert "/run/guest-io" in argv


def test_shim_mounts_exist_for_the_podman_plan(loaded, tmp_path, monkeypatch):
    cfg = loaded["cfg"]
    fake = tmp_path / "podman"
    fake.write_text("#!/bin/sh\nexit 0\n")
    fake.chmod(0o755)
    sandbox = dataclasses.replace(
        cfg.sandbox,
        mode="podman",
        image="kurisu/guest:1.0.0",
        podman_path=str(fake),
        model_proxy=dataclasses.replace(cfg.sandbox.model_proxy, enabled=True),
    )
    guest_io = tmp_path / "guest-io"
    guest_io.mkdir()
    captured = {}

    def fake_run(plan, **kwargs):
        captured["plan"] = plan
        return sandbox_mod.RunResult(exit_code=0, stdout="ok", stderr="", duration_seconds=0.0)

    monkeypatch.setattr(backend_mod.sandbox_mod, "run_plan", fake_run)
    binding = loaded["registry"].get("pilot-001")
    PiCliBackend().run(
        binding, "hello",
        limits=cfg.limits, sandbox=sandbox, runtime=cfg.runtime,
        proxy_socket=guest_io / "model-proxy.sock", guest_io_dir=guest_io,
    )
    argv = list(captured["plan"].argv)
    volumes = [argv[i + 1] for i, tok in enumerate(argv) if tok == "--volume"]
    assert any(v.endswith(":/run/guest-io:ro") for v in volumes), volumes
    assert any(v.endswith(":/run/guest-runtime:ro") for v in volumes), volumes
    assert any(tok.endswith("model-shim.py") for tok in argv)


def test_shim_and_proxy_end_to_end_without_a_namespace(tmp_path, upstream):
    """Real sockets: shim -> allowlisting proxy -> upstream, model pinned."""
    socket_path = tmp_path / "proxy.sock"
    port = _free_port()
    shim = Path(__file__).resolve().parents[2] / "guest-runtime" / "model-shim.py"
    with model_proxy.ModelProxy(upstream, socket_path, fixed_model=FIXED_MODEL):
        proc = subprocess.run(
            [sys.executable, str(shim), "--listen", f"127.0.0.1:{port}",
             "--upstream-unix", str(socket_path), "--", sys.executable, "-c", CLIENT_SNIPPET, str(port)],
            capture_output=True, text=True, timeout=60,
        )
    assert proc.returncode == 0, proc.stderr
    assert "pong" in proc.stdout
    assert _Upstream.seen and _Upstream.seen[0]["body"]["model"] == FIXED_MODEL


@requires_sandbox
def test_shim_works_inside_the_real_namespace(loaded, tmp_path, upstream):
    """The mount + namespace path, not just argv: inside bwrap, the shim listens."""
    cfg = loaded["cfg"]
    guest_io = tmp_path / "guest-io"
    guest_io.mkdir()
    socket_path = guest_io / "model-proxy.sock"
    port = _free_port()
    helpers = Path(__file__).resolve().parents[2] / "guest-runtime"
    workspace = cfg.broker.guests_root / "pilot-001" / "workspace"
    with model_proxy.ModelProxy(upstream, socket_path, fixed_model=FIXED_MODEL):
        plan = sandbox_mod.build_plan(
            workspace=workspace,
            limits=cfg.limits,
            runtime=cfg.runtime,
            sandbox=cfg.sandbox,
            command=["/usr/bin/python3", "/run/guest-runtime/model-shim.py",
                     "--listen", f"127.0.0.1:{port}",
                     "--upstream-unix", "/run/guest-io/model-proxy.sock",
                     "--", "/usr/bin/python3", "-c", CLIENT_SNIPPET, str(port)],
            env={"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": "/tmp"},
            extra_ro_binds=[(str(helpers), "/run/guest-runtime"),
                            (str(guest_io), "/run/guest-io")],
        )
        result = sandbox_mod.run_plan(plan, timeout_seconds=60)
    assert result.exit_code == 0, f"stdout={result.stdout} stderr={result.stderr}"
    assert "pong" in result.stdout
    assert _Upstream.seen and _Upstream.seen[0]["body"]["model"] == FIXED_MODEL


# ------------------------------------------------- G: state path containment
def test_state_dir_symlink_escape_is_refused(layout):
    workspace = layout["workspace"]
    outside = layout["root"] / "outside"
    outside.mkdir()
    target = outside / "registry.local.yaml"
    target.write_text("original: true\n")
    (workspace / ".agent-state").symlink_to(outside)

    with pytest.raises(SessionBackendError, match="symlink"):
        prepare_guest_state(workspace, SimpleNamespace(model=FIXED_MODEL))
    assert target.read_text() == "original: true\n"


def test_models_json_symlink_escape_is_refused(layout):
    workspace = layout["workspace"]
    state = workspace / ".agent-state"
    agent = state / "pi-agent"
    agent.mkdir(parents=True)
    outside = layout["root"] / "operator-secret.yaml"
    outside.write_text("registry: keep me\n")
    (agent / "models.json").symlink_to(outside)

    with pytest.raises(SessionBackendError, match="symlink"):
        prepare_guest_state(workspace, SimpleNamespace(model=FIXED_MODEL))
    assert outside.read_text() == "registry: keep me\n"


def test_settings_json_symlink_escape_is_refused(layout):
    workspace = layout["workspace"]
    agent = workspace / ".agent-state" / "pi-agent"
    agent.mkdir(parents=True)
    outside = layout["root"] / "auth-tokens.json"
    outside.write_text('{"tokens": {}}\n')
    (agent / "settings.json").symlink_to(outside)

    with pytest.raises(SessionBackendError, match="symlink"):
        prepare_guest_state(workspace, SimpleNamespace(model=FIXED_MODEL))
    assert outside.read_text() == '{"tokens": {}}\n'


# ------------------------------------------------- H: no stderr leaks
def test_runtime_failure_is_generic_and_detail_is_redacted(loaded, tmp_path, monkeypatch):
    cfg = loaded["cfg"]
    binding = loaded["registry"].get("pilot-001")
    secret = "ya29.SECRET-TOKEN /home/deck/.pi/agent/auth.json"  # lint-paths: allow (redaction fixture)

    def fake_run(plan, **kwargs):
        return sandbox_mod.RunResult(
            exit_code=1, stdout="", stderr=f"error: cannot read {secret}",
            duration_seconds=0.1,
        )

    monkeypatch.setattr(backend_mod.sandbox_mod, "run_plan", fake_run)
    result = PiCliBackend().run(
        binding, "hi", limits=cfg.limits, sandbox=cfg.sandbox, runtime=cfg.runtime,
        proxy_socket=None, guest_io_dir=None,
    )
    assert "SECRET-TOKEN" not in result.text
    assert "/home/deck" not in result.text  # lint-paths: allow
    assert "<redacted>" in result.detail
    assert len(result.detail) <= 200


def test_broker_does_not_forward_runtime_stderr(loaded, monkeypatch):
    cfg = loaded["cfg"]

    class LeakyBackend(EchoBackend):
        def run(self, binding, message, **kwargs):
            from guest_broker.backend import BackendResult

            return BackendResult(
                text="The assistant could not complete that request. Please try again.",
                exit_code=1,
                duration_seconds=0.1,
                sandboxed=True,
                detail="<redacted>",
            )

    broker = Broker(
        settings=cfg.broker, limits=cfg.limits, sandbox=cfg.sandbox, runtime=cfg.runtime,
        registry=loaded["registry"], allowlist=loaded["allowlist"], backend=LeakyBackend(),
        base_dir=cfg.base_dir, audit=lambda event: None,
    )
    outcome = broker.handle(Inbound(msg_id="msg0001", wa_jid=GUEST_JID, text="hi", ts=time.time()))
    assert "token" not in outcome.text.lower()
    assert "detail" not in outcome.text.lower()


# ------------------------------------------------- I: JID canonicalization
@pytest.mark.parametrize("raw", ["+15551230001@s.whatsapp.net", "+15551230001:3@s.whatsapp.net"])
def test_plus_prefixed_jids_are_canonicalised(raw):
    assert normalize_jid(raw) == "15551230001@s.whatsapp.net"


def test_allowlist_and_signin_agree_on_the_plus_variant(broker):
    outcome = broker.handle(
        Inbound(msg_id="msg0001", wa_jid="+15551230001@s.whatsapp.net", text="hi", ts=time.time())
    )
    assert outcome.kind == "reply"


# ------------------------------------------------- N: one canonical identity
def test_signin_state_is_shared_across_jid_variants(loaded, tmp_path):
    cfg = loaded["cfg"]
    store = SigninStore(tmp_path / "m.json", tmp_path / "t.json")
    manager = SigninManager(FakeDeviceFlow(), store)
    broker = Broker(
        settings=cfg.broker, limits=cfg.limits, sandbox=cfg.sandbox, runtime=cfg.runtime,
        registry=loaded["registry"], allowlist=loaded["allowlist"], backend=EchoBackend(),
        base_dir=cfg.base_dir, signin=manager, audit=lambda event: None,
    )
    first = broker.handle(Inbound(msg_id="msg0001", wa_jid="+15551230001@s.whatsapp.net", text="/signin", ts=time.time()))
    assert "DEV-CODE" in first.text
    # the device-suffixed / differently-cased variant must see the same pending state
    second = broker.handle(
        Inbound(msg_id="msg0002", wa_jid="15551230001:9@s.whatsapp.net", text="/signin", ts=time.time())
    )
    assert "DEV-CODE" not in second.text
    # same normalized identity => same pending state => it was polled, not duplicated
    assert "Signed in" in second.text
    assert store.is_verified("15551230001@s.whatsapp.net")
    assert manager.pending_count() == 0


def test_binding_must_match_resolved_user(loaded):
    cfg = loaded["cfg"]
    binding = loaded["registry"].get("pilot-001")
    mismatched = dataclasses.replace(binding, user_id="pilot-999")
    broker = Broker(
        settings=cfg.broker, limits=cfg.limits, sandbox=cfg.sandbox, runtime=cfg.runtime,
        registry=loaded["registry"], allowlist=loaded["allowlist"], backend=EchoBackend(),
        base_dir=cfg.base_dir, audit=lambda event: None,
    )
    broker.registry.get = lambda user_id: mismatched  # type: ignore[assignment]
    outcome = broker.handle(Inbound(msg_id="msg0001", wa_jid=GUEST_JID, text="hi", ts=time.time()))
    assert outcome.kind == "denied"
    assert broker.backend.calls == []  # type: ignore[attr-defined]


# ------------------------------------------------- M: auth is required
def test_auth_is_required_for_a_real_run(loaded):
    cfg = loaded["cfg"]
    assert cfg.auth.enabled is False
    problems = cli_mod.auth_requirement_errors(cfg, allow_no_auth=False)
    assert problems and "auth.enabled is false" in problems[0]
    assert cli_mod.auth_requirement_errors(cfg, allow_no_auth=True) == []
    enabled = dataclasses.replace(cfg, auth=dataclasses.replace(cfg.auth, enabled=True))
    assert cli_mod.auth_requirement_errors(enabled, allow_no_auth=False) == []


# ------------------------------------------------- J: selftest cannot false-pass
def _result(exit_code: int, stdout: str = "", stderr: str = "") -> sandbox_mod.RunResult:
    return sandbox_mod.RunResult(
        exit_code=exit_code, stdout=stdout, stderr=stderr, duration_seconds=0.1
    )


def test_selftest_rejects_launcher_failures(loaded):
    limits = loaded["cfg"].limits
    dead = _result(127, "", "bwrap: execvp /usr/bin/python3: No such file or directory")
    for expect in ("memory", "tasks", "runtime", "fsize"):
        assert not cli_mod._check_expectation(expect, dead, 0.05, limits), expect


def test_selftest_requires_the_probe_to_have_started(loaded):
    limits = loaded["cfg"].limits
    for expect, payload in (
        ("memory", "NOT_LIMITED 512\n"),
        ("tasks", "FORKED_ALL 64\n"),
        ("runtime", "STARTED\nFINISHED\n"),
        ("fsize", "SIZE=33554432\n"),
    ):
        result = _result(0, payload)
        assert not cli_mod._check_expectation(expect, result, 0.1, limits), expect


def test_selftest_accepts_proven_outcomes(loaded):
    limits = loaded["cfg"].limits
    assert cli_mod._check_expectation("memory", _result(-9, "STARTED\n"), 0.5, limits)
    assert cli_mod._check_expectation("memory", _result(0, "STARTED\nLIMITED\n"), 0.5, limits)
    assert cli_mod._check_expectation("tasks", _result(0, "STARTED\nBLOCKED_AT 7 11\n"), 0.5, limits)
    assert cli_mod._check_expectation("runtime", _result(-15, "STARTED\n"), 4.0, limits)
    assert cli_mod._check_expectation("fsize", _result(0, f"STARTED\nSIZE={limits.file_size_bytes}\n"), 0.5, limits)
    assert cli_mod._check_expectation("fsize", _result(1, "STARTED\nFile size limit exceeded\n"), 0.5, limits)


# ------------------------------------------------- watchdog: real termination
@requires_sandbox
def test_workspace_disk_watchdog_kills_the_turn(loaded):
    cfg = loaded["cfg"]
    workspace = cfg.broker.guests_root / "pilot-001" / "workspace"
    quota = 512 * 1024
    writer = (
        "import os\n"
        "print('STARTED', flush=True)\n"
        "i = 0\n"
        "while True:\n"
        "    with open(f'chunk-{i}.bin', 'wb') as fh:\n"
        "        fh.write(b'x' * 16384)\n"
        "    i += 1\n"
    )
    plan = sandbox_mod.build_plan(
        workspace=workspace,
        limits=dataclasses.replace(cfg.limits, runtime_max_seconds=120),
        runtime=cfg.runtime,
        sandbox=cfg.sandbox,
        command=["/usr/bin/python3", "-c", writer],
        env={"PATH": "/usr/bin:/bin", "HOME": "/tmp"},
    )
    result = sandbox_mod.run_plan(
        plan, timeout_seconds=120, disk_quota_bytes=quota, workspace=workspace, poll_seconds=0.2
    )
    assert result.limit_reason == "disk"
    assert result.exit_code != 0

    # proof the writer actually stopped: the file count must not keep growing
    before = len(list(workspace.glob("chunk-*.bin")))
    time.sleep(1.0)
    after = len(list(workspace.glob("chunk-*.bin")))
    assert after == before, f"writer survived the watchdog: {before} -> {after}"
    for path in workspace.glob("chunk-*.bin"):
        path.unlink()


@requires_sandbox
def test_watchdog_wall_clock_terminates_the_process_group(loaded):
    cfg = loaded["cfg"]
    workspace = cfg.broker.guests_root / "pilot-001" / "workspace"
    plan = sandbox_mod.build_plan(
        workspace=workspace,
        limits=dataclasses.replace(cfg.limits, runtime_max_seconds=90),
        runtime=cfg.runtime,
        sandbox=cfg.sandbox,
        command=["/usr/bin/bash", "-c",
                 "echo STARTED; (sleep 60; echo LEAKED > leaked.txt) & wait"],
        env={"PATH": "/usr/bin:/bin", "HOME": "/tmp"},
    )
    result = sandbox_mod.run_plan(plan, timeout_seconds=2, workspace=workspace, poll_seconds=0.2)
    assert result.limit_reason == "timeout"
    time.sleep(1.0)
    assert not (workspace / "leaked.txt").exists()


# ------------------------------------------------- D: podman stop on timeout
@requires_sandbox
def test_podman_timeout_stops_the_container(loaded, tmp_path):
    """The supervisor must stop the *container*, not just the podman CLI."""
    cfg = loaded["cfg"]
    workspace = cfg.broker.guests_root / "pilot-001" / "workspace"
    marker = tmp_path / "podman-calls.log"
    fake = tmp_path / "podman"
    fake.write_text(
        "#!/bin/sh\n"
        f'echo "$@" >> {marker}\n'
        'case "$1" in\n'
        '  run) exec sleep 60 ;;\n'
        '  *) exit 0 ;;\n'
        "esac\n"
    )
    fake.chmod(0o755)
    sandbox = dataclasses.replace(
        cfg.sandbox, mode="podman", image="kurisu/guest:1.0.0", podman_path=str(fake)
    )
    plan = sandbox_mod.build_plan(
        workspace=workspace,
        limits=dataclasses.replace(cfg.limits, runtime_max_seconds=60),
        runtime=cfg.runtime,
        sandbox=sandbox,
        command=["/bin/true"],
        env={"PATH": "/usr/bin:/bin"},
    )
    result = sandbox_mod.run_plan(plan, timeout_seconds=1, poll_seconds=0.2)
    assert result.limit_reason == "timeout"
    calls = marker.read_text()
    assert "run" in calls
    assert "kill" in calls and "rm" in calls, calls


# ------------------------------------------------- 1) binding race is closed
def test_concurrent_same_sub_binding_yields_exactly_one_winner(tmp_path):
    """Two JIDs racing on the same Google sub must not both bind."""
    for attempt in range(10):
        store = SigninStore(tmp_path / f"m{attempt}.json", tmp_path / f"t{attempt}.json")
        barrier = threading.Barrier(2)
        outcomes: list[str] = []
        lock = threading.Lock()

        def attempt_bind(jid: str) -> None:
            barrier.wait()
            try:
                store.bind(wa_jid=jid, user_id="pilot-001", sub="sub-shared", email="a@example.com")
                with lock:
                    outcomes.append("ok")
            except Exception:
                with lock:
                    outcomes.append("denied")

        threads = [
            threading.Thread(target=attempt_bind, args=("15551230001@s.whatsapp.net",)),
            threading.Thread(target=attempt_bind, args=("15551230002@s.whatsapp.net",)),
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(5)
        assert sorted(outcomes) == ["denied", "ok"], (attempt, outcomes)
        assert store.owner_of_sub("sub-shared") is not None


# ------------------------------------------------- 2) auth binding user match
def test_stored_binding_for_another_user_is_denied(loaded, tmp_path):
    cfg = loaded["cfg"]
    store = SigninStore(tmp_path / "m.json", tmp_path / "t.json")
    # verified for a *different* user, then the allowlist is repointed at pilot-001
    store.bind(wa_jid=GUEST_JID, user_id="pilot-002", sub="sub-other", email="a@example.com")
    manager = SigninManager(FakeDeviceFlow(), store)
    broker = Broker(
        settings=cfg.broker, limits=cfg.limits, sandbox=cfg.sandbox, runtime=cfg.runtime,
        registry=loaded["registry"], allowlist=loaded["allowlist"], backend=EchoBackend(),
        base_dir=cfg.base_dir, signin=manager, audit=lambda event: None,
    )
    outcome = broker.handle(Inbound(msg_id="msg0001", wa_jid=GUEST_JID, text="hello", ts=time.time()))
    assert outcome.kind == "denied"
    assert outcome.text == ""
    assert broker.backend.calls == []  # type: ignore[attr-defined]

    # and it stays denied after a restart (binding is reloaded from disk)
    reloaded = SigninStore(tmp_path / "m.json", tmp_path / "t.json")
    manager2 = SigninManager(FakeDeviceFlow(), reloaded)
    broker2 = Broker(
        settings=cfg.broker, limits=cfg.limits, sandbox=cfg.sandbox, runtime=cfg.runtime,
        registry=loaded["registry"], allowlist=loaded["allowlist"], backend=EchoBackend(),
        base_dir=cfg.base_dir, signin=manager2, audit=lambda event: None,
    )
    assert broker2.handle(
        Inbound(msg_id="msg0002", wa_jid=GUEST_JID, text="hello again", ts=time.time())
    ).kind == "denied"


# ------------------------------------------------- 3) pi-cli needs a model route
def test_pi_cli_without_model_proxy_is_a_problem(loaded):
    cfg = loaded["cfg"]
    broken = dataclasses.replace(
        cfg,
        runtime=dataclasses.replace(cfg.runtime, backend="pi-cli"),
        sandbox=dataclasses.replace(
            cfg.sandbox, model_proxy=dataclasses.replace(cfg.sandbox.model_proxy, enabled=False)
        ),
    )
    problems = cli_mod.runtime_requirement_errors(broken)
    assert problems and "model_proxy" in problems[0]
    echo = dataclasses.replace(broken, runtime=dataclasses.replace(cfg.runtime, backend="echo"))
    assert cli_mod.runtime_requirement_errors(echo) == []


# ------------------------------------------------- 6) mode-aware pi path check
def test_pi_path_check_is_mode_aware(loaded):
    cfg = loaded["cfg"]
    podman = dataclasses.replace(cfg.sandbox, mode="podman", image="kurisu/guest:1.0.0")
    image_cfg = dataclasses.replace(cfg, sandbox=podman)
    ok, detail = cli_mod.pi_binary_problem(image_cfg)
    assert ok, detail
    assert "image contents not inspected" in detail
    assert "podman run --rm kurisu/guest:1.0.0" in detail

    broken_host = dataclasses.replace(
        cfg, runtime=dataclasses.replace(cfg.runtime, pi_path="/nonexistent/pi")
    )
    ok_host, detail_host = cli_mod.pi_binary_problem(broken_host)
    assert not ok_host and "not found" in detail_host
