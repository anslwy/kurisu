"""Scoped Omnigent gateway: authorization, pinning, quotas, client, relay.

Everything runs against a local fake Omnigent server and a Unix socket. No live
API, no credentials, no services.
"""

from __future__ import annotations

import json
import socket
from pathlib import Path

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
    with pytest.raises(config_mod.ConfigError, match="allow_sub_sessions is not supported"):
        config_mod.load_config(path)
