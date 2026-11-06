"""Scoped Omnigent gateway: authorization, pinning, quotas, client, relay.

Everything runs against a local fake Omnigent server and a Unix socket. No live
API, no credentials, no services.
"""

from __future__ import annotations

import json
import socket
from pathlib import Path
from types import SimpleNamespace

import pytest

from guest_broker import config as config_mod
from guest_broker.errors import BrokerError
from guest_broker.omnigent_api import OmnigentAPI
from guest_broker.omnigent_backend import OmnigentBackend, RelayStore
from guest_broker.scoped_client import ScopedClient, ScopedClientError
from guest_broker.scoped_gateway import Quotas, ScopedGateway

from .conftest import make_config
from .test_omnigent import FakeOmnigent, _FAKE_LIMITS, _FAKE_RUNTIME, _FAKE_SANDBOX

SECRET = "gateway-shared-secret"


@pytest.fixture
def upstream():
    server = FakeOmnigent()
    server.base_url = server.start()  # type: ignore[attr-defined]
    yield server
    server.stop()


@pytest.fixture
def gateway(upstream, tmp_path):
    api = OmnigentAPI(upstream.base_url, upstream.token, session_id=upstream.session_id)
    gw = ScopedGateway(
        api,
        upstream.session_id,
        SECRET,
        tmp_path / "gateway.sock",
        quotas=Quotas(max_depth=1, max_children=2, runner_affinity="runner-guest-001"),
    )
    with gw:
        yield gw


def client(gw, *, secret: str = SECRET) -> ScopedClient:
    return ScopedClient(gw.socket_path, gw.state.root_session_id, secret)


# ------------------------------------------------------------- authorization
def test_gateway_rejects_wrong_secret(gateway):
    bad = client(gateway, secret="nope")
    with pytest.raises(ScopedClientError, match="rejected the shared secret"):
        bad.session(gateway.state.root_session_id)


def test_gateway_socket_is_private(gateway):
    assert gateway.socket_path.stat().st_mode & 0o777 == 0o600


def test_client_refuses_another_session_id(gateway):
    good = client(gateway)
    with pytest.raises(ScopedClientError, match="not the provisioned guest root"):
        good.post_message("conv_ffffffffffffffff", "hi")
    with pytest.raises(ScopedClientError, match="not the provisioned guest root"):
        good.list_items("conv_ffffffffffffffff")


def test_gateway_ignores_caller_supplied_session_id(gateway):
    """A body session id is refused before any upstream call."""
    payload = json.dumps({"text": "hi", "session_id": "conv_ffffffffffffffff"}).encode()
    request = (
        b"POST /v1/guest/message HTTP/1.1\r\nHost: localhost\r\n"
        b"Authorization: Bearer " + SECRET.encode() + b"\r\n"
        b"Content-Type: application/json\r\nContent-Length: "
        + str(len(payload)).encode() + b"\r\n\r\n" + payload
    )
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(str(gateway.socket_path))
    sock.sendall(request)
    response = sock.recv(4096)
    sock.close()
    assert b"400" in response


def test_no_global_surfaces(gateway):
    good = client(gateway)
    for path in ("/v1/sessions", "/v1/sessions/conv_x/events", "/v1/agents", "/v1/guest/delete"):
        with pytest.raises(ScopedClientError, match="HTTP 404"):
            good._request("GET", path)


# ------------------------------------------------------------- relay through
def test_message_items_and_session_forward_to_the_root(gateway, upstream):
    good = client(gateway)
    info = good.session(gateway.state.root_session_id)
    assert info.runner_online is True

    good.post_message(gateway.state.root_session_id, "hello", msg_id="m1")
    assert len(upstream.posts) == 1

    page = good.list_items(gateway.state.root_session_id)
    assert [item["role"] for item in page.data][-1] == "assistant"


def test_relay_backend_works_through_the_scoped_client(gateway, loaded):
    from guest_broker import config as cfg

    binding = loaded["registry"].get("pilot-001")
    backend = OmnigentBackend(
        client(gateway),  # duck-types OmnigentAPI
        RelayStore(Path(gateway.socket_path).parent / "relay.json"),
        poll_interval=0.01,
        turn_timeout=2.0,
    )
    runtime = cfg.RuntimeConfig(
        backend="omnigent", provider="ollama", model="m", pi_path="/bin/true",
        pi_agent_dir=Path("/tmp"), omnigent_mode="scoped_gateway",
        gateway_socket=str(gateway.socket_path), gateway_secret_env="KURISU_GW",
    )
    result = backend.run(
        binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
        runtime=runtime, msg_id="msg0001", sender_jid="15551230001@s.whatsapp.net",
    )
    assert result.text == "answer from the runner"
    assert result.sandboxed is False
    assert "operator-attested" in result.detail


# ------------------------------------------------------------- children
def test_child_creation_is_disabled_by_default(gateway):
    good = client(gateway)
    with pytest.raises(ScopedClientError, match="HTTP 501"):
        good.create_child(parent_session_id=gateway.state.root_session_id, title="task")


def test_child_creation_enforces_parent_depth_and_count(upstream, tmp_path):
    created: list[dict] = []

    def creator(*, parent: str, runner_affinity: str, title: str) -> str:
        created.append({"parent": parent, "runner_affinity": runner_affinity, "title": title})
        return f"conv_child{len(created):02d}"

    api = OmnigentAPI(upstream.base_url, upstream.token, session_id=upstream.session_id)
    gw = ScopedGateway(
        api, upstream.session_id, SECRET, tmp_path / "gw.sock",
        quotas=Quotas(max_depth=1, max_children=2, runner_affinity="runner-guest-001",
                      children_enabled=True),
        create_child=creator,
    )
    with gw:
        good = client(gw)
        root = gw.state.root_session_id

        # success: parent defaults to the root, affinity is forwarded
        child = good.create_child(parent_session_id=root, title="long task")
        assert child["parent"] == root and child["depth"] == 1
        assert created[0]["runner_affinity"] == "runner-guest-001"

        # depth: a grandchild is refused (max_depth = 1)
        with pytest.raises(ScopedClientError, match="HTTP 403"):
            good._request("POST", "/v1/guest/children", {"parent_session_id": child["session_id"]})

        # count: third child is refused (max_children = 2)
        good.create_child(parent_session_id=root, title="second")
        with pytest.raises(ScopedClientError, match="HTTP 403"):
            good.create_child(parent_session_id=root, title="third")

        # an unrelated parent is refused
        with pytest.raises(ScopedClientError, match="HTTP 403"):
            good._request("POST", "/v1/guest/children", {"parent_session_id": "conv_unrelated"})

        # listing returns only descendants, never the root or unrelated sessions
        rows = good.children()
        assert {row["parent"] for row in rows} == {root}
        assert all(row["session_id"] != root for row in rows)


# ------------------------------------------------------------- config
def test_scoped_mode_requires_socket_and_secret(layout, mappings):
    path = make_config(layout, mappings)
    text = (
        path.read_text(encoding="utf-8")
        .replace("backend: echo", "backend: omnigent")
        .replace('base_url: ""', "base_url: http://127.0.0.1:6767")
        .replace(
            "    token_env: OMNIGENT_OPERATOR_TOKEN",
            "    token_env: OMNIGENT_OPERATOR_TOKEN\n"
            "    relay_state_path: ../runtime/state/relay.json\n"
            "    mode: scoped_gateway",
        )
    )
    path.write_text(text, encoding="utf-8")
    with pytest.raises(config_mod.ConfigError, match="gateway_socket"):
        config_mod.load_config(path)


def test_sub_sessions_stay_disabled_even_in_scoped_mode(layout, mappings):
    path = make_config(layout, mappings)
    text = (
        path.read_text(encoding="utf-8")
        .replace("backend: echo", "backend: omnigent")
        .replace('base_url: ""', "base_url: http://127.0.0.1:6767")
        .replace(
            "    token_env: OMNIGENT_OPERATOR_TOKEN",
            "    token_env: OMNIGENT_OPERATOR_TOKEN\n"
            "    relay_state_path: ../runtime/state/relay.json\n"
            "    mode: scoped_gateway\n"
            "    gateway_socket: ../runtime/state/gw.sock\n"
            "    gateway_secret_env: KURISU_GW_SECRET\n"
            "    allow_sub_sessions: true",
        )
    )
    path.write_text(text, encoding="utf-8")
    with pytest.raises(config_mod.ConfigError, match="allow_sub_sessions is only supported with"):
        config_mod.load_config(path)


# ------------------------------------------------------- relay error taxonomy
@pytest.mark.parametrize("status,clears", [(400, True), (401, True), (403, True), (404, True),
                                            (409, False), (429, False), (500, False)])
def test_scoped_client_status_maps_to_relay_taxonomy(gateway, status, clears):
    """Definitive statuses clear pending; everything else stays unknown."""
    from guest_broker.omnigent_api import DEFINITIVE_REJECTION_STATUSES

    err = ScopedClientError("boom", status=status)
    assert err.outcome_unknown is (status not in DEFINITIVE_REJECTION_STATUSES)
    assert (not err.outcome_unknown) is clears


def test_scoped_network_error_is_unknown(tmp_path):
    missing = ScopedClient(tmp_path / "nope.sock", "conv_0123456789abcdef", SECRET)
    with pytest.raises(ScopedClientError) as excinfo:
        missing.list_items("conv_0123456789abcdef")
    assert excinfo.value.outcome_unknown is True


def test_scoped_post_401_clears_pending_and_500_keeps_it(gateway, loaded, tmp_path):
    """End-to-end through OmnigentBackend: 401 definitive, 500 uncertain."""
    from guest_broker.omnigent_backend import TurnOutcomeUnknown

    binding = loaded["registry"].get("pilot-001")
    state_path = tmp_path / "relay.json"

    bad_secret = ScopedClient(gateway.socket_path, gateway.state.root_session_id, "wrong")
    backend = OmnigentBackend(bad_secret, RelayStore(state_path), poll_interval=0.01, turn_timeout=0.3)
    with pytest.raises(BrokerError):
        backend.run(binding, "hi", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="m401")
    assert RelayStore(state_path).snapshot(binding.session_id).pending is None

    # a 500 from the gateway must keep pending and raise TurnOutcomeUnknown
    class Boom(ScopedClient):
        def post_message(self, session_id, text, *, msg_id=""):
            self._check(session_id)
            raise ScopedClientError("upstream 500", status=500)

    boom = Boom(gateway.socket_path, gateway.state.root_session_id, SECRET)
    backend2 = OmnigentBackend(boom, RelayStore(state_path), poll_interval=0.01, turn_timeout=0.3)
    with pytest.raises(TurnOutcomeUnknown):
        backend2.run(binding, "hi", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                     runtime=_FAKE_RUNTIME, msg_id="m500")
    assert RelayStore(state_path).snapshot(binding.session_id).pending is not None


# ------------------------------------------------------- strict protocol
def _raw_post(gateway, body: bytes, path: str = "/v1/guest/message") -> bytes:
    request = (
        f"POST {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {SECRET}\r\n"
        f"Content-Type: application/json\r\nContent-Length: {len(body)}\r\n\r\n"
    ).encode() + body
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(str(gateway.socket_path))
    sock.sendall(request)
    out = sock.recv(65536)
    sock.close()
    return out


def test_malformed_json_is_rejected(gateway):
    assert b"400" in _raw_post(gateway, b"{not json")


def test_non_object_json_is_rejected(gateway):
    assert b"400" in _raw_post(gateway, b"[1,2,3]")


def test_oversized_body_is_rejected(gateway):
    assert b"400" in _raw_post(gateway, b'{"text":"' + b"x" * (70 * 1024) + b'"}')


def test_overlong_text_is_rejected(gateway):
    assert b"400" in _raw_post(gateway, b'{"text":"' + b"x" * 9000 + b'"}')


def test_invalid_limit_is_a_400_not_a_crash(gateway):
    request = (
        f"GET /v1/guest/items?limit=abc HTTP/1.1\r\nHost: localhost\r\n"
        f"Authorization: Bearer {SECRET}\r\n\r\n"
    ).encode()
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(str(gateway.socket_path))
    sock.sendall(request)
    out = sock.recv(4096)
    sock.close()
    assert b"400" in out


def test_gateway_refuses_to_replace_a_regular_file(upstream, tmp_path):
    from guest_broker.scoped_gateway import ScopedAccessError

    target = tmp_path / "gateway.sock"
    target.write_text("not a socket")
    api = OmnigentAPI(upstream.base_url, upstream.token, session_id=upstream.session_id)
    gw = ScopedGateway(api, upstream.session_id, SECRET, target)
    with pytest.raises(ScopedAccessError, match="not a socket"):
        gw.start()
    assert target.read_text() == "not a socket"


def test_child_creation_requires_root_runner_affinity(upstream, tmp_path):
    api = OmnigentAPI(upstream.base_url, upstream.token, session_id=upstream.session_id)
    gw = ScopedGateway(
        api, upstream.session_id, SECRET, tmp_path / "gw.sock",
        quotas=Quotas(max_depth=1, max_children=2, runner_affinity="", children_enabled=True),
        create_child=lambda **_: "conv_child01",
    )
    with gw:
        with pytest.raises(ScopedClientError, match="HTTP 501"):
            client(gw).create_child(parent_session_id=gw.state.root_session_id)


def test_scoped_post_400_clears_pending(gateway, loaded, tmp_path):
    """A definitive 400 from the gateway must clear pending (no duplicate repost)."""
    from guest_broker.errors import BrokerError

    binding = loaded["registry"].get("pilot-001")
    state_path = tmp_path / "relay400.json"

    class BadRequest(ScopedClient):
        def post_message(self, session_id, text, *, msg_id=""):
            self._check(session_id)
            raise ScopedClientError("gateway 400", status=400)

    backend = OmnigentBackend(
        BadRequest(gateway.socket_path, gateway.state.root_session_id, SECRET),
        RelayStore(state_path), poll_interval=0.01, turn_timeout=0.3,
    )
    with pytest.raises(BrokerError):
        backend.run(binding, "hi", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="m400")
    assert RelayStore(state_path).snapshot(binding.session_id).pending is None


# ------------------------------------------------------- completion integration
def test_real_scoped_400_clears_pending_without_posting(gateway, upstream, loaded, tmp_path):
    """Exercise actual gateway status mapping, not only a fake client error."""
    binding = loaded["registry"].get("pilot-001")
    state_path = tmp_path / "real-400-relay.json"
    upstream.post_status_override = 400
    backend = OmnigentBackend(
        client(gateway), RelayStore(state_path), poll_interval=0.01, turn_timeout=0.3
    )
    with pytest.raises(ScopedClientError) as excinfo:
        backend.run(binding, "hi", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="real400")
    assert excinfo.value.status == 400
    assert RelayStore(state_path).snapshot(binding.session_id).pending is None
    assert upstream.posts == []


def test_real_scoped_500_keeps_pending_unknown(gateway, upstream, loaded, tmp_path):
    from guest_broker.omnigent_backend import TurnOutcomeUnknown

    binding = loaded["registry"].get("pilot-001")
    state_path = tmp_path / "real-500-relay.json"
    upstream.post_status_override = 500
    backend = OmnigentBackend(
        client(gateway), RelayStore(state_path), poll_interval=0.01, turn_timeout=0.3
    )
    with pytest.raises(TurnOutcomeUnknown):
        backend.run(binding, "hi", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="real500")
    assert RelayStore(state_path).snapshot(binding.session_id).pending is not None


def test_unknown_message_field_is_rejected_before_upstream(gateway, upstream):
    body = json.dumps({"text": "hello", "model": "attacker-selected"}).encode()
    assert b"400" in _raw_post(gateway, body)
    assert upstream.posts == []


def _scoped_config_text(path: Path) -> str:
    return (
        path.read_text(encoding="utf-8")
        .replace("backend: echo", "backend: omnigent")
        .replace('base_url: ""', "base_url: http://127.0.0.1:6767")
        .replace(
            "    token_env: OMNIGENT_OPERATOR_TOKEN",
            "    token_env: KURISU_TEST_OPERATOR_UNSET\n"
            "    relay_state_path: ../runtime/state/relay.json\n"
            "    mode: scoped_gateway\n"
            "    gateway_socket: ../runtime/state/gateway.sock\n"
            "    gateway_secret_env: KURISU_TEST_GW_SECRET",
        )
    )


def test_gateway_socket_is_config_relative_and_cannot_be_in_workspace(layout, mappings):
    path = make_config(layout, mappings)
    path.write_text(_scoped_config_text(path), encoding="utf-8")
    cfg = config_mod.load_config(path)
    assert cfg.runtime.gateway_socket == str(
        (path.parent / "../runtime/state/gateway.sock").resolve()
    )
    path.write_text(
        path.read_text().replace(
            "gateway_socket: ../runtime/state/gateway.sock",
            f"gateway_socket: {layout['workspace'] / 'gateway.sock'}",
        ),
        encoding="utf-8",
    )
    with pytest.raises(config_mod.ConfigError, match="inside a guest workspace"):
        config_mod.load_config(path)


def test_scoped_preflight_needs_no_operator_token(layout, mappings, monkeypatch):
    from guest_broker import cli as cli_mod

    path = make_config(layout, mappings)
    path.write_text(_scoped_config_text(path), encoding="utf-8")
    monkeypatch.setenv("KURISU_TEST_GW_SECRET", SECRET)
    monkeypatch.delenv("KURISU_TEST_OPERATOR_UNSET", raising=False)
    args = SimpleNamespace(
        config=str(path), allow_no_sandbox=False, allow_no_auth=True, probe_api=False
    )
    assert cli_mod.cmd_preflight(args) == 0


def test_gateway_command_does_not_require_google_or_build_broker(layout, mappings, monkeypatch):
    """Missing Omnigent token fails before sockets; no Google secret is needed."""
    from guest_broker import cli as cli_mod

    path = make_config(layout, mappings)
    text = _scoped_config_text(path) + (
        "\nauth:\n"
        "  enabled: true\n"
        "  google_client_id: fake.apps.googleusercontent.com\n"
        "  google_client_secret_env: KURISU_TEST_GOOGLE_UNSET\n"
        "  mappings_path: auth-mappings.json\n"
        "  tokens_path: auth-tokens.json\n"
    )
    path.write_text(text, encoding="utf-8")
    monkeypatch.setenv("KURISU_TEST_GW_SECRET", SECRET)
    monkeypatch.delenv("KURISU_TEST_OPERATOR_UNSET", raising=False)
    monkeypatch.delenv("KURISU_TEST_GOOGLE_UNSET", raising=False)
    # Reaches its own token check without constructing sign-in or opening a socket.
    assert cli_mod.main(["gateway", "--config", str(path)]) == 2