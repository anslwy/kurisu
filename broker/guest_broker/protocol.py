"""NDJSON wire protocol between the whatsmeow gateway and the broker.

The gateway is *untrusted input*: it may be compromised by a bad pairing or a
WhatsApp-side bug. The protocol therefore:

* caps frame size and message length;
* rejects unknown top-level fields (so a future ``session_id`` smuggled by the
  gateway cannot be silently accepted);
* explicitly rejects selection fields such as ``session_id``, ``user_id``,
  ``model``, ``workspace`` and ``command`` with a distinct error;
* carries no credentials in either direction.
"""

from __future__ import annotations

import hashlib
import json
import re
import time
from dataclasses import dataclass, field
from typing import Any

from .errors import ProtocolError

PROTOCOL_VERSION = 1
MAX_FRAME_BYTES = 64 * 1024

INBOUND_FIELDS = {"v", "type", "msg_id", "wa_jid", "text", "ts"}
HELLO_FIELDS = {"v", "type", "account_jid", "allowlist_sha256", "gateway_version"}
REPLY_FIELDS = {"v", "type", "msg_id", "wa_jid", "text", "ts"}
ACK_FIELDS = {"v", "type", "msg_id", "status"}
ERROR_FIELDS = {"v", "type", "msg_id", "code", "detail"}

#: Fields a transport must never be able to set. Presence is a hard failure,
#: not something to ignore: it means the peer is trying to steer session choice.
FORBIDDEN_FIELDS = (
    "session_id", "session", "user_id", "user", "model", "provider",
    "workspace", "cwd", "command", "argv", "env", "limits",
)

_MSG_ID = re.compile(r"^[A-Za-z0-9_-]{6,64}$")


def _sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def allowlist_fingerprint(jids: list[str]) -> str:
    """Stable fingerprint so the broker can detect gateway/allowlist drift."""
    return _sha256("\n".join(sorted(jids)))


@dataclass(frozen=True)
class Inbound:
    msg_id: str
    wa_jid: str
    text: str
    ts: float


@dataclass(frozen=True)
class Hello:
    account_jid: str
    allowlist_sha256: str
    gateway_version: str


@dataclass(frozen=True)
class Reply:
    msg_id: str
    wa_jid: str
    text: str
    ts: float = field(default_factory=time.time)


def decode_line(raw: bytes | str, *, max_message_chars: int) -> dict[str, Any]:
    if isinstance(raw, bytes):
        if len(raw) > MAX_FRAME_BYTES:
            raise ProtocolError(f"frame too large ({len(raw)} bytes > {MAX_FRAME_BYTES})")
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise ProtocolError(f"frame is not valid UTF-8: {exc}") from exc
    else:
        text = raw
        if len(text.encode("utf-8")) > MAX_FRAME_BYTES:
            raise ProtocolError("frame too large")
    text = text.strip()
    if not text:
        raise ProtocolError("empty frame")
    try:
        data = json.loads(text)
    except json.JSONDecodeError as exc:
        raise ProtocolError(f"frame is not JSON: {exc}") from exc
    if not isinstance(data, dict):
        raise ProtocolError("frame must be a JSON object")

    for forbidden in FORBIDDEN_FIELDS:
        if forbidden in data:
            raise ProtocolError(
                f"forbidden field {forbidden!r}: the transport may never select a session, "
                "model, workspace or command"
            )
    if data.get("v") != PROTOCOL_VERSION:
        raise ProtocolError(f"unsupported protocol version {data.get('v')!r}")
    kind = data.get("type")
    if kind == "inbound":
        extra = set(data) - INBOUND_FIELDS
        if extra:
            raise ProtocolError(f"inbound: unknown field(s) {sorted(extra)}")
        msg_id = data.get("msg_id")
        if not isinstance(msg_id, str) or not _MSG_ID.fullmatch(msg_id):
            raise ProtocolError("inbound.msg_id: invalid id")
        jid = data.get("wa_jid")
        if not isinstance(jid, str) or not jid:
            raise ProtocolError("inbound.wa_jid: required string")
        body = data.get("text")
        if not isinstance(body, str):
            raise ProtocolError("inbound.text: required string")
        if len(body) > max_message_chars:
            raise ProtocolError(
                f"inbound.text: {len(body)} chars exceeds limit {max_message_chars}"
            )
        ts = data.get("ts", time.time())
        if not isinstance(ts, (int, float)):
            raise ProtocolError("inbound.ts: must be a number")
        return {"type": "inbound", "inbound": Inbound(msg_id=msg_id, wa_jid=jid, text=body, ts=float(ts))}
    if kind == "hello":
        extra = set(data) - HELLO_FIELDS
        if extra:
            raise ProtocolError(f"hello: unknown field(s) {sorted(extra)}")
        for key in ("account_jid", "allowlist_sha256", "gateway_version"):
            if not isinstance(data.get(key), str):
                raise ProtocolError(f"hello.{key}: required string")
        return {
            "type": "hello",
            "hello": Hello(
                account_jid=data["account_jid"],
                allowlist_sha256=data["allowlist_sha256"],
                gateway_version=data["gateway_version"],
            ),
        }
    raise ProtocolError(f"unknown frame type {kind!r}")


def encode(frame: Reply | dict[str, Any]) -> bytes:
    if isinstance(frame, Reply):
        payload: dict[str, Any] = {
            "v": PROTOCOL_VERSION,
            "type": "reply",
            "msg_id": frame.msg_id,
            "wa_jid": frame.wa_jid,
            "text": frame.text,
            "ts": frame.ts,
        }
    else:
        payload = {"v": PROTOCOL_VERSION, **frame}
    data = json.dumps(payload, ensure_ascii=False, sort_keys=True).encode("utf-8")
    if len(data) > MAX_FRAME_BYTES:
        raise ProtocolError(f"outbound frame too large ({len(data)} bytes)")
    return data + b"\n"


def encode_ack(msg_id: str, status: str) -> bytes:
    if status not in ("accepted", "denied", "failed", "rate-limited"):
        raise ProtocolError(f"invalid ack status {status!r}")
    return encode({"type": "ack", "msg_id": msg_id, "status": status})


def encode_error(code: str, detail: str, msg_id: str | None = None) -> bytes:
    payload: dict[str, Any] = {"type": "error", "code": code, "detail": detail[:500]}
    if msg_id is not None:
        payload["msg_id"] = msg_id
    return encode(payload)
