"""Broker decision logic: attribution, single-session, rate/busy limits."""

from __future__ import annotations

import json
import time
from pathlib import Path

import pytest

from guest_broker import protocol
from guest_broker.backend import BackendResult, EchoBackend
from guest_broker.broker import Broker
from guest_broker.errors import SessionBackendError
from guest_broker.protocol import Inbound

from .conftest import GUEST_JID, SESSION_ID


def inbound(text: str = "hello", jid: str = GUEST_JID, msg_id: str = "msg0001") -> Inbound:
    return Inbound(msg_id=msg_id, wa_jid=jid, text=text, ts=time.time())


def test_unknown_identity_is_denied_before_any_work(broker: Broker):
    outcome = broker.handle(inbound(jid="19999999999@s.whatsapp.net"))
    assert outcome.kind == "denied"
    assert outcome.text == ""
    assert broker.backend.calls == []  # type: ignore[attr-defined]


def test_group_chat_is_denied(broker: Broker):
    outcome = broker.handle(inbound(jid="12345-67890@g.us"))
    assert outcome.kind == "denied"
    assert broker.backend.calls == []  # type: ignore[attr-defined]


def test_reply_comes_from_the_registry_session(broker: Broker):
    outcome = broker.handle(inbound("what is 2+2?"))
    assert outcome.kind == "reply"
    assert outcome.user_id == "pilot-001"
    assert outcome.session_id == SESSION_ID
    assert "pilot-001" in outcome.text


def test_oversize_message_is_rejected_without_backend_call(broker: Broker):
    outcome = broker.handle(inbound("x" * (broker.settings.max_message_chars + 1)))
    assert outcome.kind == "denied"
    assert "too long" in outcome.text
    assert broker.backend.calls == []  # type: ignore[attr-defined]


def test_rate_limit_then_recovery(broker: Broker):
    for index in range(broker.settings.per_user_rate):
        assert broker.handle(inbound(f"m{index}", msg_id=f"msg{index:04d}")).kind == "reply"
    limited = broker.handle(inbound("over", msg_id="msg9999"))
    assert limited.kind == "rate-limited"
    assert "too quickly" in limited.text


def test_reply_is_truncated_to_the_configured_ceiling(loaded):
    cfg = loaded["cfg"]

    class LongBackend(EchoBackend):
        def run(self, binding, message, **kwargs):
            return BackendResult(text="y" * 500, exit_code=0, duration_seconds=0.0, sandboxed=True)

    broker = Broker(
        settings=cfg.broker,
        limits=cfg.limits,
        sandbox=cfg.sandbox,
        runtime=cfg.runtime,
        registry=loaded["registry"],
        allowlist=loaded["allowlist"],
        backend=LongBackend(),
        base_dir=cfg.base_dir,
    )
    outcome = broker.handle(inbound())
    assert outcome.kind == "reply"
    assert len(outcome.text) <= cfg.broker.max_reply_chars
    assert outcome.text.endswith("…")


def test_backend_failure_is_reported_without_internal_detail(loaded):
    cfg = loaded["cfg"]

    class FailingBackend(EchoBackend):
        def run(self, binding, message, **kwargs):
            raise SessionBackendError("secret internal path /etc/shadow")

    broker = Broker(
        settings=cfg.broker,
        limits=cfg.limits,
        sandbox=cfg.sandbox,
        runtime=cfg.runtime,
        registry=loaded["registry"],
        allowlist=loaded["allowlist"],
        backend=FailingBackend(),
        base_dir=cfg.base_dir,
    )
    outcome = broker.handle(inbound())
    assert outcome.kind == "failed"
    assert "secret internal path" not in outcome.text
    assert "unavailable" in outcome.text


def test_concurrent_request_for_same_user_is_refused(loaded):
    cfg = loaded["cfg"]
    import threading

    started = threading.Event()
    release = threading.Event()

    class SlowBackend(EchoBackend):
        def run(self, binding, message, **kwargs):
            started.set()
            release.wait(5)
            return BackendResult(text="slow done", exit_code=0, duration_seconds=0.0, sandboxed=True)

    broker = Broker(
        settings=cfg.broker,
        limits=cfg.limits,
        sandbox=cfg.sandbox,
        runtime=cfg.runtime,
        registry=loaded["registry"],
        allowlist=loaded["allowlist"],
        backend=SlowBackend(),
        base_dir=cfg.base_dir,
        audit=lambda event: None,
    )
    results = {}

    def first() -> None:
        results["first"] = broker.handle(inbound("one", msg_id="msg000a"))

    thread = threading.Thread(target=first)
    thread.start()
    assert started.wait(5)
    second = broker.handle(inbound("two", msg_id="msg000b"))
    release.set()
    thread.join(5)
    assert second.kind == "busy"
    assert results["first"].kind == "reply"


def test_audit_log_never_contains_message_text(broker: Broker, loaded):
    broker.handle(inbound("my secret message"))
    audit_path: Path = loaded["cfg"].resolve(loaded["cfg"].broker.audit_path)
    assert audit_path.is_file()
    records = [json.loads(line) for line in audit_path.read_text(encoding="utf-8").splitlines()]
    assert any(record["event"] == "reply" for record in records)
    text = audit_path.read_text(encoding="utf-8")
    assert "my secret message" not in text
    assert all("text_sha256_12" in record or "detail" in record for record in records)


def test_startup_validation_flags_unmapped_allowlist_identity(layout, mappings, loaded):
    from .conftest import write_allowlist

    cfg = loaded["cfg"]
    write_allowlist(
        mappings["allowlist"],
        [
            {"wa_jid": GUEST_JID, "user_id": "pilot-001"},
            {"wa_jid": "15551230009@s.whatsapp.net", "user_id": "pilot-002"},
        ],
    )
    from guest_broker.allowlist import load_allowlist

    allowlist = load_allowlist(mappings["allowlist"])
    broker = Broker(
        settings=cfg.broker,
        limits=cfg.limits,
        sandbox=cfg.sandbox,
        runtime=cfg.runtime,
        registry=loaded["registry"],
        allowlist=allowlist,
        backend=EchoBackend(),
        base_dir=cfg.base_dir,
        audit=lambda event: None,
    )
    problems = broker.validate_startup()
    assert any("pilot-002" in problem and "no session" in problem for problem in problems)


def test_startup_validation_is_clean_for_the_expected_layout(broker: Broker):
    assert broker.validate_startup() == []
