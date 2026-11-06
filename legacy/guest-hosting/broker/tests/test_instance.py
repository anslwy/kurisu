"""Dedicated per-guest Omnigent instance: isolation, validation, identity, acceptance.

No Omnigent server, container, credential or network service is used: the
instance API is a fake transport and the relay targets a loopback echo socket.
"""

from __future__ import annotations

import json
import os
import socket
import stat
import subprocess
import sys
import threading
from pathlib import Path

import pytest

from guest_broker import config as config_mod
from guest_broker import instance as inst
from guest_broker.cli import main as cli_main
from guest_broker.omnigent_api import OmnigentAPI, OmnigentError

from .conftest import make_config, write_registry

REPO = Path(__file__).resolve().parents[2]


@pytest.fixture
def groot(tmp_path: Path) -> Path:
    root = tmp_path / "guests"
    root.mkdir()
    return root


@pytest.fixture
def protected(tmp_path: Path) -> tuple[Path, ...]:
    main = tmp_path / "operator-home" / ".omnigent"
    main.mkdir(parents=True)
    return (main,)


def _mode(path: Path) -> int:
    return stat.S_IMODE(path.stat().st_mode)


# --------------------------------------------------------------- validation
@pytest.mark.parametrize("host", ["0.0.0.0", "localhost", "192.168.1.5", "::1", "::", "127.0.0.2", "example.com"])
def test_only_ipv4_loopback_bind_is_accepted(host):
    with pytest.raises(inst.InstanceError):
        inst.validate_host(host)
    assert inst.validate_host("127.0.0.1") == "127.0.0.1"


@pytest.mark.parametrize("user", ["", "../x", "Pilot", "a/b", "-x", "x" * 33, "a b"])
def test_user_id_validation(user):
    with pytest.raises(inst.InstanceError):
        inst.validate_user_id(user)


@pytest.mark.parametrize("port", [6767, 11434, 80, 65535, inst.DEFAULT_PORT_BASE - 1,
                                  inst.DEFAULT_PORT_BASE + inst.DEFAULT_PORT_SPAN])
def test_port_validation_rejects_reserved_and_out_of_window(port):
    with pytest.raises(inst.InstanceError):
        inst.validate_port(port)


def test_port_validation_honours_operator_reserved_ports():
    port = inst.DEFAULT_PORT_BASE + 5
    assert inst.validate_port(port) == port
    with pytest.raises(inst.InstanceError, match="reserved"):
        inst.validate_port(port, reserved=[port])


def test_port_allocation_is_deterministic_and_unique():
    a = inst.allocate_port("pilot-001", {})
    assert a == inst.allocate_port("pilot-001", {})
    b = inst.allocate_port("pilot-002", {"pilot-001": a})
    assert a != b
    # collision: the hash start is taken -> probe moves on, never reuses
    c = inst.allocate_port("pilot-001", {"other": a})
    assert c != a
    inst.validate_port(c)


def test_spec_rejects_relative_root_and_public_host(groot):
    with pytest.raises(inst.InstanceError):
        inst.InstanceSpec(user_id="pilot-001", guests_root=Path("guests"), port=27101)
    with pytest.raises(inst.InstanceError):
        inst.InstanceSpec(user_id="pilot-001", guests_root=groot, port=27101, host="0.0.0.0")


def test_paths_must_not_overlap_main_omnigent_state(tmp_path):
    main = tmp_path / ".omnigent"
    groot = main / "guests"            # guests root inside the main state dir
    with pytest.raises(inst.InstanceError, match="protected"):
        inst.provision("pilot-001", groot, protected=(main,))
    # and the reverse: a protected path inside the guest tree
    groot2 = tmp_path / "g2"
    with pytest.raises(inst.InstanceError, match="protected"):
        inst.provision("pilot-001", groot2, protected=(groot2 / "pilot-001" / "instance" / "data",))


def test_default_protected_paths_include_main_home_and_overrides(tmp_path):
    env = {"HOME": str(tmp_path), "OMNIGENT_DATA_DIR": str(tmp_path / "d")}
    paths = inst.default_protected_paths(env)
    assert (tmp_path / ".omnigent").resolve() in paths
    assert (tmp_path / "d").resolve() in paths


# ------------------------------------------------------------- provisioning
def test_provision_creates_private_tree_and_identity(groot, protected):
    spec, state = inst.provision("pilot-001", groot, protected=protected)
    for path in spec.all_dirs():
        assert path.is_dir() and _mode(path) == 0o700, path
    assert _mode(spec.state_file) == 0o600
    assert state.model == inst.FIXED_MODEL_ID == "ollama/deepseek-v4.1-flash:cloud"
    assert state.root_session_id == ""
    assert spec.db_path.parent == spec.data_dir
    manifest = json.loads((spec.config_home / "kurisu-instance.json").read_text())
    assert manifest["bind"] == f"127.0.0.1:{spec.port}"
    assert manifest["model_selection"] == "disabled"


def test_provision_is_idempotent_and_keeps_port_and_root(groot, protected):
    spec, state = inst.provision("pilot-001", groot, protected=protected)
    state.root_session_id = "conv_root0001"
    inst.save_state(spec, state)
    spec2, state2 = inst.provision("pilot-001", groot, protected=protected)
    assert spec2.port == spec.port
    assert state2.root_session_id == "conv_root0001"
    with pytest.raises(inst.InstanceError, match="refusing to move"):
        inst.provision("pilot-001", groot, port=spec.port + 1, protected=protected)


def test_two_guests_share_no_port_or_path(groot, protected):
    a, _ = inst.provision("pilot-001", groot, protected=protected)
    b, _ = inst.provision("pilot-002", groot, protected=protected)
    assert a.port != b.port
    a_paths = {str(p) for p in a.all_dirs()} | {str(a.db_path), str(a.relay_socket)}
    b_paths = {str(p) for p in b.all_dirs()} | {str(b.db_path), str(b.relay_socket)}
    assert not (a_paths & b_paths)
    for pa in a.all_dirs():
        for pb in b.all_dirs():
            assert not str(pb).startswith(str(a.user_root) + os.sep)
            assert not str(pa).startswith(str(b.user_root) + os.sep)
    a_argv, a_env = inst.server_plan(a, source_env={})
    b_argv, b_env = inst.server_plan(b, source_env={})
    assert a_env["OMNIGENT_DATA_DIR"] != b_env["OMNIGENT_DATA_DIR"]
    assert a_argv[a_argv.index("--database-uri") + 1] != b_argv[b_argv.index("--database-uri") + 1]


def test_provision_refuses_port_owned_by_another_guest(groot, protected):
    a, _ = inst.provision("pilot-001", groot, protected=protected)
    with pytest.raises(inst.InstanceError, match="already used"):
        inst.provision("pilot-002", groot, port=a.port, protected=protected)


def test_state_file_is_validated_on_load(groot, protected):
    spec, state = inst.provision("pilot-001", groot, protected=protected)
    os.chmod(spec.state_file, 0o644)
    with pytest.raises(inst.InstanceError, match="0600"):
        inst.load_state(spec)
    os.chmod(spec.state_file, 0o600)
    data = json.loads(spec.state_file.read_text())
    data["model"] = "openai/gpt-x"
    spec.state_file.write_text(json.dumps(data))
    with pytest.raises(inst.InstanceError, match="only ollama/deepseek"):
        inst.load_state(spec)


# ------------------------------------------------------------ command plans
def test_server_plan_is_dedicated_private_and_clean(groot, protected):
    spec, _ = inst.provision("pilot-001", groot, protected=protected)
    operator_env = {
        "PATH": "/usr/bin", "HOME": "/home/operator",
        "OMNIGENT_OPERATOR_TOKEN": "x", "OMNIGENT_SERVER_URL": "http://127.0.0.1:6767",
        "OMNIGENT_CONFIG_HOME": "/home/operator/.omnigent", "TELEGRAM_BOT_TOKEN": "x",
        "OLLAMA_HOST": "http://127.0.0.1:11434",
    }
    argv, env = inst.server_plan(spec, source_env=operator_env, provider_env=["OLLAMA_HOST"])
    assert argv[argv.index("--host") + 1] == "127.0.0.1"
    assert argv[argv.index("--port") + 1] == str(spec.port)
    assert argv[argv.index("--database-uri") + 1] == f"sqlite:///{spec.db_path}"
    assert argv[argv.index("--artifact-location") + 1] == str(spec.artifacts_dir)
    assert "--config" not in argv and "--background" not in argv and "--no-open" in argv
    assert env["OMNIGENT_CONFIG_HOME"] == str(spec.config_home)
    assert env["OMNIGENT_DATA_DIR"] == str(spec.data_dir)
    assert env["HOME"] == str(spec.server_home)
    assert env["OMNIGENT_LOCAL_SINGLE_USER"] == "1"
    assert env["OLLAMA_HOST"] == "http://127.0.0.1:11434"
    for leaked in ("OMNIGENT_OPERATOR_TOKEN", "OMNIGENT_SERVER_URL", "TELEGRAM_BOT_TOKEN"):
        assert leaked not in env
    with pytest.raises(inst.InstanceError):
        inst.server_plan(spec, source_env=operator_env, provider_env=["OMNIGENT_SERVER_URL"])


def test_runner_plan_is_network_none_and_guest_mounts_only(groot, protected):
    spec, _ = inst.provision("pilot-001", groot, protected=protected)
    argv = inst.runner_plan(spec, image="localhost/kurisu-guest-runner:1")
    inst.assert_runner_plan_safe(argv, spec)
    assert "--network=none" in argv and "--cap-drop=ALL" in argv and "--read-only" in argv
    vols = [argv[i + 1] for i, a in enumerate(argv) if a == "--volume"]
    assert all(v.split(":")[0].startswith(str(spec.user_root)) for v in vols)
    assert f"{spec.io_dir}:/run/kurisu-io:ro" in vols
    joined = " ".join(argv)
    assert f"http://127.0.0.1:{spec.port}" in joined
    assert "6767" not in joined
    assert "OMNIGENT_SERVER_URL" not in joined and "TOKEN" not in joined


@pytest.mark.parametrize("key", ["OMNIGENT_OPERATOR_TOKEN", "KURISU_GW_SECRET", "OMNIGENT_SERVER_URL",
                                 "OMNIGENT_RUNNER_TUNNEL_BINDING_TOKEN", "GOOGLE_API_KEY"])
def test_runner_plan_refuses_credentials_in_env(groot, protected, key):
    spec, _ = inst.provision("pilot-001", groot, protected=protected)
    with pytest.raises(inst.InstanceError):
        inst.runner_plan(spec, image="localhost/x:1", extra_env={key: "v"})


@pytest.mark.parametrize("mutation", [
    lambda a, s: [x.replace("--network=none", "--network=host") for x in a],
    lambda a, s: a[:3] + ["--privileged"] + a[3:],
    lambda a, s: a[:3] + ["--volume", "/run/user/1000/podman/podman.sock:/s"] + a[3:],
    lambda a, s: a[:3] + ["--volume", "/home/operator:/h:rw"] + a[3:],
    lambda a, s: [x.replace(f"{s.io_dir}:/run/kurisu-io:ro", f"{s.io_dir}:/run/kurisu-io:rw") for x in a],
])
def test_runner_plan_guard_catches_widening(groot, protected, mutation):
    spec, _ = inst.provision("pilot-001", groot, protected=protected)
    argv = mutation(inst.runner_plan(spec, image="localhost/x:1"), spec)
    with pytest.raises(inst.InstanceError):
        inst.assert_runner_plan_safe(argv, spec)


def test_runner_plan_rejects_bad_image(groot, protected):
    spec, _ = inst.provision("pilot-001", groot, protected=protected)
    for image in ("", "--privileged", "img;rm -rf /", "IMG with space"):
        with pytest.raises(inst.InstanceError):
            inst.runner_plan(spec, image=image)


# ----------------------------------------------------- root session identity
class FakeInstance:
    def __init__(self, hosts=("host_container",)) -> None:
        self.sessions: dict[str, dict] = {}
        self.hosts = list(hosts)
        self.calls: list[tuple[str, str, dict | None]] = []
        self.counter = 0

    def __call__(self, method, url, headers, body):
        assert "Authorization" not in headers  # single-user instance: no credential
        payload = json.loads(body) if body else None
        path = url.split("127.0.0.1:", 1)[1].split("/", 1)[1]
        self.calls.append((method, "/" + path, payload))
        if method == "GET" and path == "v1/hosts":
            return 200, {"hosts": [{"host_id": h} for h in self.hosts]}
        if method == "GET" and path.startswith("v1/sessions/"):
            sid = path.split("/")[2]
            return (200, self.sessions[sid]) if sid in self.sessions else (404, {})
        if method == "POST" and path == "v1/sessions":
            self.counter += 1
            sid = f"conv_root{self.counter:04d}"
            self.sessions[sid] = {"id": sid, "runner_id": "runner_c", "parent_session_id": None,
                                  "model": payload["model_override"]}
            return 201, self.sessions[sid]
        return 404, {}


def test_root_session_created_once_then_reused_across_restarts(groot, protected):
    spec, _ = inst.provision("pilot-001", groot, protected=protected)
    fake = FakeInstance()
    state, created = inst.ensure_root_session(spec, inst.InstanceClient(spec, fake), agent_id="ag_pi")
    assert created and state.root_session_id == "conv_root0001"
    post = [c for c in fake.calls if c[0] == "POST"][0][2]
    assert post["host_id"] == "host_container"
    assert post["model_override"] == inst.FIXED_MODEL_ID
    assert post["workspace"] == "/workspace"

    # "restart": fresh process, spec reloaded from disk, same fake DB
    spec2 = inst.spec_for("pilot-001", groot)
    fake.calls.clear()
    state2, created2 = inst.ensure_root_session(spec2, inst.InstanceClient(spec2, fake), agent_id="ag_pi")
    assert not created2 and state2.root_session_id == "conv_root0001"
    assert all(c[0] == "GET" for c in fake.calls)
    # re-provisioning keeps it too
    _, state3 = inst.provision("pilot-001", groot, protected=protected)
    assert state3.root_session_id == "conv_root0001"


def test_missing_recorded_root_is_refused_not_silently_recreated(groot, protected):
    spec, _ = inst.provision("pilot-001", groot, protected=protected)
    fake = FakeInstance()
    inst.ensure_root_session(spec, inst.InstanceClient(spec, fake), agent_id="ag")
    fake.sessions.clear()  # e.g. DB wiped / wrong instance
    with pytest.raises(inst.InstanceError, match="refusing to create"):
        inst.ensure_root_session(spec, inst.InstanceClient(spec, fake), agent_id="ag")
    state, created = inst.ensure_root_session(spec, inst.InstanceClient(spec, fake), agent_id="ag", recreate=True)
    assert created and state.root_session_id == "conv_root0002"


@pytest.mark.parametrize("hosts", [(), ("host_a", "host_b")])
def test_root_session_requires_exactly_one_container_host(groot, protected, hosts):
    spec, _ = inst.provision("pilot-001", groot, protected=protected)
    with pytest.raises(inst.InstanceError, match="exactly one"):
        inst.ensure_root_session(spec, inst.InstanceClient(spec, FakeInstance(hosts)), agent_id="ag")


# ------------------------------------------------------------- acceptance
def _tree():
    m = inst.FIXED_MODEL_ID
    return [
        {"id": "conv_root", "parent_session_id": None, "runner_id": "r1", "model": m},
        {"id": "conv_child", "parent_session_id": "conv_root", "runner_id": "r1", "model": m},
        {"id": "conv_grand", "parent_session_id": "conv_child", "runner_id": "r1", "model": m},
    ]


def test_session_tree_accepts_contained_children():
    checks = inst.check_session_tree(_tree(), "conv_root", "r1")
    assert all(c.ok for c in checks), [c for c in checks if not c.ok]


@pytest.mark.parametrize("bad,name", [
    ({"id": "conv_x", "parent_session_id": None, "runner_id": "r1", "model": inst.FIXED_MODEL_ID}, "tree:conv_x"),
    ({"id": "conv_x", "parent_session_id": "conv_root", "runner_id": "r_host", "model": inst.FIXED_MODEL_ID},
     "runner:conv_x"),
    ({"id": "conv_x", "parent_session_id": "conv_root", "model": inst.FIXED_MODEL_ID}, "runner:conv_x"),
    ({"id": "conv_x", "parent_session_id": "conv_root", "runner_id": "r1", "model": "openai/gpt"}, "model:conv_x"),
    ({"id": "conv_x", "parent_session_id": "conv_ghost", "runner_id": "r1", "model": inst.FIXED_MODEL_ID},
     "tree:conv_x"),
])
def test_session_tree_fails_closed(bad, name):
    checks = {c.name: c for c in inst.check_session_tree(_tree() + [bad], "conv_root", "r1")}
    assert not checks[name].ok


def test_session_tree_requires_root_and_runner():
    assert not inst.check_session_tree(_tree(), "conv_missing", "r1")[0].ok
    assert not all(c.ok for c in inst.check_session_tree(_tree(), "conv_root", ""))


def test_listener_check():
    port = 27150
    ok = f"LISTEN 0 2048 127.0.0.1:{port} 0.0.0.0:*\nLISTEN 0 1 127.0.0.1:6767 0.0.0.0:*"
    assert inst.check_listeners(ok, port).ok
    assert not inst.check_listeners(f"LISTEN 0 2048 0.0.0.0:{port} 0.0.0.0:*", port).ok
    assert not inst.check_listeners(ok + f"\nLISTEN 0 5 [::]:{port} [::]:*", port).ok
    assert not inst.check_listeners("", port).ok


def test_hosts_check():
    assert inst.check_hosts([{"host_id": "h1"}], "h1").ok
    assert not inst.check_hosts([{"host_id": "h1"}, {"host_id": "h2"}], "h1").ok
    assert not inst.check_hosts([], "h1").ok


def test_static_preflight_passes_for_fresh_instance(groot, protected):
    spec, _ = inst.provision("pilot-001", groot, protected=protected)
    checks = {c.name: c for c in inst.static_preflight(spec, protected=protected, source_env={})}
    assert all(c.ok for n, c in checks.items() if n != "root-session"), checks
    assert not checks["root-session"].ok  # honest: not created until the live step
    (spec.config_home / "config.yaml").write_text("managed_sandboxes:\n  provider: modal\n")
    checks = {c.name: c for c in inst.static_preflight(spec, protected=protected, source_env={})}
    assert not checks["no-managed-sandbox"].ok


# -------------------------------------------------------------- host relay
def test_host_relay_reaches_only_the_instance_port(tmp_path):
    groot = tmp_path / "g"
    # find a free port inside the instance window and bind a loopback echo server on it
    echo = None
    for port in range(inst.DEFAULT_PORT_BASE + 800, inst.DEFAULT_PORT_BASE + inst.DEFAULT_PORT_SPAN):
        try:
            echo = socket.create_server(("127.0.0.1", port))
            break
        except OSError:
            continue
    assert echo is not None
    spec, _ = inst.provision("pilot-001", groot, port=port, protected=())

    def serve():
        conn, _ = echo.accept()
        conn.sendall(b"instance:" + conn.recv(100))
        conn.close()

    threading.Thread(target=serve, daemon=True).start()
    with inst.HostRelay(spec) as relay:
        assert relay.target == ("127.0.0.1", spec.port)
        assert _mode(spec.relay_socket) == 0o600
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.settimeout(5)
        c.connect(str(spec.relay_socket))
        c.sendall(b"ping")
        c.shutdown(socket.SHUT_WR)
        data = b""
        while chunk := c.recv(100):
            data += chunk
        c.close()
    echo.close()
    assert data == b"instance:ping"
    assert not spec.relay_socket.exists()


def test_container_relay_refuses_non_loopback_or_foreign_socket():
    script = REPO / "guest-runtime" / "instance-relay.py"
    for fwd in ("0.0.0.0:1=/run/kurisu-io/omnigent.sock", "127.0.0.1:1=/var/run/docker.sock"):
        proc = subprocess.run([sys.executable, "-I", str(script), "--forward", fwd, "--", "true"],
                              capture_output=True, text=True, timeout=30)
        assert proc.returncode != 0 and "refusing forward" in proc.stderr


# ----------------------------------------------------- API + config wiring
def test_unauthenticated_api_only_for_ipv4_loopback_instance():
    seen = {}

    def transport(method, url, headers, body):
        seen.update(headers)
        return 200, {"id": "conv_root0001"}

    api = OmnigentAPI("http://127.0.0.1:27101", "", session_id="conv_root0001",
                      transport=transport, unauthenticated_loopback=True)
    api._request("GET", api._url("conv_root0001"))
    assert "Authorization" not in seen
    with pytest.raises(OmnigentError):
        OmnigentAPI("https://example.com", "", unauthenticated_loopback=True)
    with pytest.raises(OmnigentError):
        OmnigentAPI("http://127.0.0.1:27101", "tok", unauthenticated_loopback=True)
    with pytest.raises(OmnigentError):
        api._url("conv_other")


def _instance_config(layout, mappings, extra_lines: str, base_url: str) -> Path:
    path = make_config(layout, mappings)
    text = (
        path.read_text(encoding="utf-8")
        .replace("backend: echo", "backend: omnigent")
        .replace('base_url: ""', f"base_url: {base_url}")
        .replace("    token_env: OMNIGENT_OPERATOR_TOKEN",
                 "    token_env: \"\"\n"
                 "    relay_state_path: ../runtime/state/relay.json\n"
                 "    mode: scoped_gateway\n"
                 "    gateway_socket: ../runtime/state/gw.sock\n"
                 "    gateway_secret_env: KURISU_GW_SECRET\n" + extra_lines)
    )
    path.write_text(text, encoding="utf-8")
    return path


def test_config_pins_broker_to_the_guest_instance(layout, mappings):
    spec, _ = inst.provision("pilot-001", layout["guests_root"], protected=())
    good = _instance_config(layout, mappings, "    instance_user: pilot-001\n    allow_sub_sessions: true",
                            spec.base_url)
    cfg = config_mod.load_config(good)
    assert cfg.runtime.instance_user == "pilot-001" and cfg.runtime.allow_sub_sessions
    # main server URL or another port: refused
    for url in ("http://127.0.0.1:6767", f"http://127.0.0.1:{spec.port + 1}"):
        bad = _instance_config(layout, mappings, "    instance_user: pilot-001", url)
        with pytest.raises(config_mod.ConfigError, match="must be the guest instance"):
            config_mod.load_config(bad)
    # unprovisioned guest: refused
    bad = _instance_config(layout, mappings, "    instance_user: pilot-009", spec.base_url)
    with pytest.raises(config_mod.ConfigError, match="missing or unreadable"):
        config_mod.load_config(bad)


def test_cli_provision_and_preflight(groot, capsys, monkeypatch, tmp_path):
    monkeypatch.setenv("HOME", str(tmp_path / "operator-home"))
    monkeypatch.delenv("OMNIGENT_CONFIG_HOME", raising=False)
    monkeypatch.delenv("OMNIGENT_DATA_DIR", raising=False)
    assert cli_main(["instance", "provision", "--guests-root", str(groot), "--user", "pilot-001"]) == 0
    out = json.loads(capsys.readouterr().out)
    assert out["base_url"].startswith("http://127.0.0.1:")
    # preflight fails only on the not-yet-created root session (live step)
    rc = cli_main(["instance", "preflight", "--guests-root", str(groot), "--user", "pilot-001"])
    text = capsys.readouterr().out
    assert rc == 1 and "[FAIL] root-session" in text
    assert text.count("[FAIL]") == 1
    assert cli_main(["instance", "plan", "--guests-root", str(groot), "--user", "pilot-001",
                     "--image", "localhost/kurisu-guest-runner:1"]) == 0
    plan = capsys.readouterr().out
    assert "--network=none" in plan and "OMNIGENT_LOCAL_SINGLE_USER=1" in plan


# ------------------------------------------------------- server confinement
def test_server_sandbox_confines_to_instance_dir(groot, protected, tmp_path):
    spec, _ = inst.provision("pilot-001", groot, protected=protected)
    tools = tmp_path / "tools" / "omnigent"
    tools.mkdir(parents=True)
    argv, env = inst.server_plan(spec, source_env={"PATH": "/usr/bin"})
    wrapped = inst.server_sandbox_plan(spec, argv, env, ro_paths=[tools], protected=protected)
    assert wrapped[wrapped.index("--") + 1:] == argv
    assert "--clearenv" in wrapped and "--share-net" in wrapped
    binds = [(wrapped[i], wrapped[i + 1]) for i, a in enumerate(wrapped) if a in ("--bind", "--ro-bind")]
    rw = [src for flag, src in binds if flag == "--bind"]
    assert rw == [str(spec.instance_dir)]
    assert str(protected[0]) not in " ".join(wrapped)
    assert str(groot / "pilot-002") not in " ".join(wrapped)


@pytest.mark.parametrize("bad", ["/", "GUESTS", "PROTECTED", "/run/user/1000", "/home"])
def test_server_sandbox_rejects_broad_or_foreign_ro_paths(groot, protected, bad):
    spec, _ = inst.provision("pilot-001", groot, protected=protected)
    path = {"GUESTS": str(groot), "PROTECTED": str(protected[0])}.get(bad, bad)
    argv, env = inst.server_plan(spec, source_env={})
    with pytest.raises(inst.InstanceError):
        inst.server_sandbox_plan(spec, argv, env, ro_paths=[path], protected=protected)
