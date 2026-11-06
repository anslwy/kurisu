"""Gateway server handshake, pinning and end-to-end reply flow."""

from __future__ import annotations

import json
import socket
import time
from pathlib import Path

import pytest

from guest_broker import protocol
from guest_broker.errors import ProtocolError
from guest_broker.server import GatewayServer, GatewaySession

from .conftest import GUEST_JID, SESSION_ID


def _session(broker, loaded, state_dir: Path) -> GatewaySession:
    return GatewaySession(
        broker=broker,
        allowlist=loaded["allowlist"],
        pin_path=state_dir / "gateway-account.json",
        log=lambda message: None,
    )


def _hello(account: str, allowlist) -> bytes:
    jids = [allowlist.jid_for_user(user) or "" for user in allowlist.user_ids]
    return protocol.encode(
        {
            "type": "hello",
            "account_jid": account,
            "allowlist_sha256": protocol.allowlist_fingerprint(jids),
            "gateway_version": "test",
        }
    )


def test_handshake_pins_the_secondary_account(broker, loaded, tmp_path):
    session = _session(broker, loaded, tmp_path)
    frames = session.handle_line(_hello("15559990001@s.whatsapp.net", loaded["allowlist"]))
    assert json.loads(frames[0])["ok"] is True
    assert session.hello_seen


def test_handshake_refuses_a_guest_identity_as_the_transport(broker, loaded, tmp_path):
    session = _session(broker, loaded, tmp_path)
    with pytest.raises(ProtocolError, match="secondary account"):
        session.handle_line(_hello(GUEST_JID, loaded["allowlist"]))


def test_handshake_refuses_a_different_account_after_pinning(broker, loaded, tmp_path):
    _session(broker, loaded, tmp_path).handle_line(_hello("15559990001@s.whatsapp.net", loaded["allowlist"]))
    second = _session(broker, loaded, tmp_path)
    with pytest.raises(ProtocolError, match="!= pinned"):
        second.handle_line(_hello("15559990002@s.whatsapp.net", loaded["allowlist"]))


def test_handshake_refuses_allowlist_drift(broker, loaded, tmp_path):
    frame = protocol.encode(
        {
            "type": "hello",
            "account_jid": "15559990001@s.whatsapp.net",
            "allowlist_sha256": "0" * 64,
            "gateway_version": "test",
        }
    )
    with pytest.raises(ProtocolError, match="fingerprint mismatch"):
        _session(broker, loaded, tmp_path).handle_line(frame)


def test_first_frame_must_be_hello(broker, loaded, tmp_path):
    inbound = protocol.encode(protocol.Reply(msg_id="msg0001", wa_jid=GUEST_JID, text="hi"))
    # a reply frame decodes as an unknown type for the broker direction
    with pytest.raises(ProtocolError):
        _session(broker, loaded, tmp_path).handle_line(inbound)


def test_inbound_produces_ack_and_reply(broker, loaded, tmp_path):
    session = _session(broker, loaded, tmp_path)
    session.handle_line(_hello("15559990001@s.whatsapp.net", loaded["allowlist"]))
    frame = protocol.encode(
        {
            "type": "inbound",
            "msg_id": "msg0123",
            "wa_jid": GUEST_JID,
            "text": "ping",
            "ts": time.time(),
        }
    )
    frames = session.handle_line(frame)
    assert len(frames) == 2
    ack = json.loads(frames[0])
    reply = json.loads(frames[1])
    assert ack == {"v": 2, "type": "ack", "msg_id": "msg0123", "status": "accepted"}
    assert reply["type"] == "reply"
    assert reply["wa_jid"] == GUEST_JID
    assert reply["msg_id"] == "msg0123"


def test_inbound_from_non_allowlisted_jid_is_denied(broker, loaded, tmp_path):
    session = _session(broker, loaded, tmp_path)
    session.handle_line(_hello("15559990001@s.whatsapp.net", loaded["allowlist"]))
    frame = protocol.encode(
        {
            "type": "inbound",
            "msg_id": "msg0124",
            "wa_jid": "15550000000@s.whatsapp.net",
            "text": "let me in",
            "ts": time.time(),
        }
    )
    frames = session.handle_line(frame)
    ack = json.loads(frames[0])
    assert ack["status"] == "denied"
    # v2 contract: ack + exactly one reply frame, with EMPTY text, so no answer
    # content ever leaks to a non-allowlisted sender and the gateway stays in sync
    assert len(frames) == 2
    reply = json.loads(frames[1])
    assert reply["type"] == "reply" and reply["text"] == ""


def test_inbound_cannot_smuggle_a_session_id(broker, loaded, tmp_path):
    session = _session(broker, loaded, tmp_path)
    session.handle_line(_hello("15559990001@s.whatsapp.net", loaded["allowlist"]))
    frame = (
        b'{"v":1,"type":"inbound","msg_id":"msg0125","wa_jid":"' + GUEST_JID.encode() + b'",'
        b'"text":"hi","ts":1,"session_id":"conv_attacker"}'
    )
    with pytest.raises(ProtocolError, match="forbidden field"):
        session.handle_line(frame)


def test_end_to_end_over_a_real_unix_socket(broker, loaded, tmp_path):
    socket_path = tmp_path / "broker.sock"
    server = GatewayServer(
        broker=broker,
        allowlist=loaded["allowlist"],
        socket_path=socket_path,
        state_dir=tmp_path / "state",
        log=lambda message: None,
    )
    server.start()
    try:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(10)
        client.connect(str(socket_path))
        client.sendall(_hello("15559990001@s.whatsapp.net", loaded["allowlist"]))
        assert json.loads(client.recv(65536))["ok"] is True
        client.sendall(
            protocol.encode(
                {
                    "type": "inbound",
                    "msg_id": "msg0200",
                    "wa_jid": GUEST_JID,
                    "text": "hello over the socket",
                    "ts": time.time(),
                }
            )
        )
        buffer = b""
        while buffer.count(b"\n") < 2:
            buffer += client.recv(65536)
        client.close()
        frames = [json.loads(line) for line in buffer.strip().split(b"\n")]
        assert frames[0]["type"] == "ack"
        assert frames[1]["type"] == "reply"
        assert "hello over the socket" in frames[1]["text"]
    finally:
        server.stop()


def test_socket_permissions_are_owner_only(broker, loaded, tmp_path):
    socket_path = tmp_path / "broker.sock"
    server = GatewayServer(
        broker=broker,
        allowlist=loaded["allowlist"],
        socket_path=socket_path,
        state_dir=tmp_path / "state",
        log=lambda message: None,
    )
    server.start()
    try:
        assert socket_path.stat().st_mode & 0o777 == 0o600
    finally:
        server.stop()
