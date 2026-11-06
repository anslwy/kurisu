"""Unix-socket gateway server (trusted side).

The gateway process speaks NDJSON to this socket. The socket is mode 0600 and
owned by the broker's uid: the guest never has it bound into its sandbox, and the
transport has no way to ask for a different session.

Handshake rules:

1. The first frame must be ``hello`` within ``HANDSHAKE_TIMEOUT`` seconds.
2. The gateway's WhatsApp account JID is pinned on first use (trust-on-first-use)
   in ``state/gateway-account.json``. A later mismatch is refused, which stops a
   *second* account (for example the primary one) from ever attaching.
3. The gateway's allowlist fingerprint must match the broker's, so the two sides
   cannot silently disagree about who is allowed.
"""

from __future__ import annotations

import json
import os
import socket
import socketserver
import stat
import threading
import time
from pathlib import Path
from typing import Callable

from . import protocol
from .allowlist import Allowlist, normalize_jid
from .broker import Broker
from .errors import BrokerError, ProtocolError, UnauthorizedError

HANDSHAKE_TIMEOUT = 15.0


class GatewaySession:
    """Per-connection protocol state (separated from the socket for testing)."""

    def __init__(self, broker: Broker, allowlist: Allowlist, pin_path: Path, log: Callable[[str], None]) -> None:
        self.broker = broker
        self.allowlist = allowlist
        self.pin_path = pin_path
        self.log = log
        self.hello_seen = False

    def handle_hello(self, hello: protocol.Hello) -> None:
        try:
            account = normalize_jid(hello.account_jid)
        except UnauthorizedError as exc:
            raise ProtocolError(f"hello.account_jid: {exc}") from exc
        if self.allowlist.contains(account):
            raise ProtocolError(
                "hello.account_jid is an allowlisted guest identity: the transport must be "
                "the dedicated secondary account, never a guest or primary account"
            )
        expected_fp = protocol.allowlist_fingerprint(
            [self.allowlist.jid_for_user(u) or "" for u in self.allowlist.user_ids]
        )
        if hello.allowlist_sha256 != expected_fp:
            raise ProtocolError(
                "allowlist fingerprint mismatch between gateway and broker; "
                "both must load the same allowlist file"
            )
        self._pin_account(account)
        self.hello_seen = True
        self.log(f"gateway connected: account={_mask(account)} version={hello.gateway_version}")

    def _pin_account(self, account: str) -> None:
        self.pin_path.parent.mkdir(parents=True, exist_ok=True)
        if self.pin_path.is_file():
            pinned = json.loads(self.pin_path.read_text(encoding="utf-8")).get("account_jid")
            if pinned and pinned != account:
                raise ProtocolError(
                    f"gateway account {_mask(account)} != pinned {_mask(str(pinned))}; "
                    "refusing a different WhatsApp account (delete the pin file only as an explicit operator action)"
                )
            return
        fd = os.open(self.pin_path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump({"account_jid": account, "pinned_at": time.time()}, handle)
        self.log(f"pinned gateway account {_mask(account)}")

    def handle_line(self, line: bytes) -> list[bytes]:
        """Return the frames to send back for one inbound line."""
        if not self.hello_seen:
            frame = protocol.decode_line(line, max_message_chars=self.broker.settings.max_message_chars)
            if frame["type"] != "hello":
                raise ProtocolError("first frame must be hello")
            self.handle_hello(frame["hello"])
            return [protocol.encode({"type": "hello", "ok": True, "broker_version": "0.1.0"})]

        frame = protocol.decode_line(line, max_message_chars=self.broker.settings.max_message_chars)
        if frame["type"] != "inbound":
            # ack/error frames from the gateway are informational only
            return []
        inbound = frame["inbound"]
        outcome = self.broker.handle(inbound)
        status = {
            "reply": "accepted",
            "denied": "denied",
            "rate-limited": "rate-limited",
            "busy": "rate-limited",
            "failed": "failed",
        }.get(outcome.kind, "failed")
        frames: list[bytes] = [protocol.encode_ack(inbound.msg_id, status)]
        if outcome.text:
            frames.append(
                protocol.encode(
                    protocol.Reply(msg_id=inbound.msg_id, wa_jid=inbound.wa_jid, text=outcome.text)
                )
            )
        return frames


class _Handler(socketserver.BaseRequestHandler):
    server: "GatewayServer"

    def handle(self) -> None:  # pragma: no cover - integration-tested
        self.request.settimeout(HANDSHAKE_TIMEOUT)
        session = GatewaySession(
            broker=self.server.broker,
            allowlist=self.server.allowlist,
            pin_path=self.server.pin_path,
            log=self.server.log,
        )
        buffer = b""
        try:
            while True:
                chunk = self.request.recv(65536)
                if not chunk:
                    return
                buffer += chunk
                while b"\n" in buffer:
                    line, _, buffer = buffer.partition(b"\n")
                    if not line.strip():
                        continue
                    self.request.settimeout(None)
                    try:
                        frames = session.handle_line(line)
                    except (ProtocolError, UnauthorizedError) as exc:
                        self.server.log(f"protocol rejection: {exc}")
                        self.request.sendall(protocol.encode_error("denied", str(exc)))
                        return
                    except BrokerError as exc:
                        self.server.log(f"broker error: {exc}")
                        self.request.sendall(protocol.encode_error("failed", "internal broker error"))
                        continue
                    for frame in frames:
                        self.request.sendall(frame)
                if len(buffer) > protocol.MAX_FRAME_BYTES:
                    self.server.log("oversized frame without newline; closing")
                    return
        except (socket.timeout, ConnectionResetError):
            return


class _Server(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True

    def server_bind(self) -> None:
        old = os.umask(0o177)
        try:
            super().server_bind()
        finally:
            os.umask(old)
        os.chmod(self.server_address, 0o600)


class GatewayServer:
    def __init__(
        self,
        *,
        broker: Broker,
        allowlist: Allowlist,
        socket_path: Path,
        state_dir: Path,
        log: Callable[[str], None] = print,
    ) -> None:
        self.broker = broker
        self.allowlist = allowlist
        self.socket_path = Path(socket_path)
        self.pin_path = Path(state_dir) / "gateway-account.json"
        self.log = log
        self._server: _Server | None = None
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        self.socket_path.parent.mkdir(parents=True, exist_ok=True)
        if self.socket_path.exists():
            if not stat.S_ISSOCK(self.socket_path.stat().st_mode):
                raise BrokerError(f"{self.socket_path} exists and is not a socket")
            self.socket_path.unlink()
        self._server = _Server(str(self.socket_path), _Handler)
        self._server.broker = self.broker
        self._server.allowlist = self.allowlist
        self._server.pin_path = self.pin_path
        self._server.log = self.log
        os.chmod(self.socket_path, 0o600)
        self._thread = threading.Thread(target=self._server.serve_forever, name="gateway", daemon=True)
        self._thread.start()

    def serve_forever(self) -> None:
        assert self._server is not None
        self._server.serve_forever()

    def stop(self) -> None:
        if self._server is not None:
            self._server.shutdown()
            self._server.server_close()
            self._server = None
        try:
            if self.socket_path.exists():
                self.socket_path.unlink()
        except OSError:
            pass


def _mask(jid: str) -> str:
    user, _, server = jid.partition("@")
    return f"{user[:3]}***@{server}"
