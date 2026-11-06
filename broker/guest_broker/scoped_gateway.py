"""Scoped Omnigent access layer (broker <-> guest root session).

PROBLEM
The Omnigent API authenticates a *user*, not a session: whoever holds the
operator token can list, create and control every session that user owns. The
broker needs only three things for one guest root. Handing the broker (or the
runner) that token is more authority than the pilot needs.

CHOICE FOR THE PoC: a scoped gateway (this module), not a second identity.
A second identity isolates *users* but still lets that identity enumerate and
control every session it owns, and it would require changing the live account
setup. The gateway is the smaller, safer change: it is the only holder of the
token, and it exposes exactly the root session plus its own descendants.

WHAT IT ENFORCES (server-side relative to the guest; not prompt-based)
  * exactly one root session id, fixed at startup from the registry;
  * POST message / GET items for that root only - any other id is refused
    before an upstream call is made;
  * no global list, no global create, no delete, no policy/agent endpoints;
  * child sessions (disabled by default) only when the caller asks for a child
    of the root, within depth/count quotas, and they inherit the root's runner
    affinity so tool execution stays in the same guest container;
  * the broker authenticates with a local shared secret over a Unix socket; the
    operator token never leaves this process.

WHAT IT DOES NOT DO
  * It cannot prove the runner container isolation (operator invariant), and it
    does not make the upstream token session-scoped - if someone else obtained
    the operator token directly, the gateway is bypassed. That is why the token
    must stay on this host and the gateway must be the only client.
  * Sub-session creation stays disabled until the parent/root/quota checks are
    exercised against the real API; the enforcement below is tested with a fake
    upstream only.
"""

from __future__ import annotations

import hmac
import json
import os
import socketserver
import threading
import urllib.parse
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler
from pathlib import Path
from typing import Any, Mapping

from .omnigent_api import OmnigentAPI, OmnigentError


class ScopedAccessError(Exception):
    """Request refused by the scoped layer (never forwarded upstream)."""


@dataclass
class Quotas:
    """Child-session quotas, enforced here rather than by prompts."""

    max_depth: int = 1
    max_children: int = 4
    #: runner/host binding inherited from the root, so tools stay in the same
    #: guest container. Empty means "let the server pick" (not allowed in prod).
    runner_affinity: str = ""
    children_enabled: bool = False


@dataclass
class _Child:
    session_id: str
    parent: str
    depth: int


@dataclass
class ScopedState:
    root_session_id: str
    quotas: Quotas = field(default_factory=Quotas)
    children: dict[str, _Child] = field(default_factory=dict)

    def descendants(self) -> list[dict[str, Any]]:
        return [
            {"session_id": child.session_id, "parent": child.parent, "depth": child.depth}
            for child in self.children.values()
        ]


class ScopedGateway:
    """Unix-socket HTTP gateway holding the operator token.

    Routes (all require the local shared secret):
      POST /v1/guest/message            {text, msg_id}
      GET  /v1/guest/items?after=&limit=
      GET  /v1/guest/session
      POST /v1/guest/children           {title}        (disabled by default)
      GET  /v1/guest/children
    Anything else -> 404. A session id in the path/body is a 400: the root is not
    caller-selectable.
    """

    def __init__(
        self,
        api: OmnigentAPI,
        root_session_id: str,
        secret: str,
        socket_path: Path,
        *,
        quotas: Quotas | None = None,
        create_child: Any | None = None,
    ) -> None:
        if not secret:
            raise ScopedAccessError("the gateway needs a local shared secret")
        self.api = api
        self.state = ScopedState(root_session_id=root_session_id, quotas=quotas or Quotas())
        self._secret = secret
        self.socket_path = Path(socket_path)
        self._create_child = create_child  # injected for tests; None => unsupported
        self._server: socketserver.ThreadingUnixStreamServer | None = None
        self._thread: threading.Thread | None = None

    # -- lifecycle -------------------------------------------------------
    def start(self) -> None:
        self.socket_path.parent.mkdir(parents=True, exist_ok=True)
        if self.socket_path.exists():
            self.socket_path.unlink()
        gateway = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args: object) -> None:  # keep stdout clean
                pass

            def _send(self, status: int, payload: Mapping[str, Any]) -> None:
                raw = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def _authorised(self) -> bool:
                header = self.headers.get("Authorization", "")
                expected = f"Bearer {gateway._secret}"
                return hmac.compare_digest(header, expected)

            def _body(self) -> dict[str, Any]:
                length = int(self.headers.get("Content-Length") or 0)
                if length <= 0:
                    return {}
                try:
                    parsed = json.loads(self.rfile.read(length) or b"{}")
                except json.JSONDecodeError:
                    return {}
                return parsed if isinstance(parsed, dict) else {}

            def do_GET(self) -> None:  # noqa: N802
                if not self._authorised():
                    self._send(401, {"error": "unauthorized"})
                    return
                parsed = urllib.parse.urlsplit(self.path)
                if parsed.path == "/v1/guest/session":
                    try:
                        info = gateway.api.session(gateway.state.root_session_id)
                    except OmnigentError as exc:
                        self._send(502, {"error": type(exc).__name__})
                        return
                    self._send(200, {
                        "session_id": info.session_id,
                        "runner_online": info.runner_online,
                        "agent": info.agent,
                        "model": info.model,
                        "isolation": "operator-attested",
                    })
                    return
                if parsed.path == "/v1/guest/items":
                    query = urllib.parse.parse_qs(parsed.query)
                    after = (query.get("after") or [None])[0]
                    limit = int((query.get("limit") or ["100"])[0])
                    order = (query.get("order") or ["asc"])[0]
                    if order not in ("asc", "desc"):
                        self._send(400, {"error": "order must be asc or desc"})
                        return
                    try:
                        page = gateway.api.list_items(
                            gateway.state.root_session_id, after=after, limit=limit, order=order
                        )
                    except OmnigentError as exc:
                        self._send(502, {"error": type(exc).__name__})
                        return
                    self._send(200, {
                        "data": page.data,
                        "first_id": page.first_id,
                        "last_id": page.last_id,
                        "has_more": page.has_more,
                    })
                    return
                if parsed.path == "/v1/guest/children":
                    self._send(200, {"data": gateway.state.descendants()})
                    return
                self._send(404, {"error": "not found"})

            def do_POST(self) -> None:  # noqa: N802
                if not self._authorised():
                    self._send(401, {"error": "unauthorized"})
                    return
                path = urllib.parse.urlsplit(self.path).path
                body = self._body()
                # The root is not caller-selectable.
                if "session_id" in body or "session" in body:
                    self._send(400, {"error": "session id is fixed by the gateway"})
                    return
                if path == "/v1/guest/message":
                    text = body.get("text")
                    if not isinstance(text, str) or not text.strip():
                        self._send(400, {"error": "text is required"})
                        return
                    try:
                        gateway.api.post_message(
                            gateway.state.root_session_id, text,
                            msg_id=str(body.get("msg_id") or "gateway"),
                        )
                    except OmnigentError as exc:
                        self._send(502, {"error": type(exc).__name__})
                        return
                    self._send(200, {"queued": True})
                    return
                if path == "/v1/guest/children":
                    self._send(*gateway._create_child_request(body))
                    return
                self._send(404, {"error": "not found"})

        class Server(socketserver.ThreadingUnixStreamServer):
            daemon_threads = True

            def server_bind(self) -> None:
                old = os.umask(0o177)
                try:
                    super().server_bind()
                finally:
                    os.umask(old)
                os.chmod(self.server_address, 0o600)

        self._server = Server(str(self.socket_path), Handler)
        os.chmod(self.socket_path, 0o600)
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

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

    def __enter__(self) -> "ScopedGateway":
        self.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self.stop()

    # -- child session policy -------------------------------------------
    def _create_child_request(self, body: Mapping[str, Any]) -> tuple[int, dict[str, Any]]:
        quotas = self.state.quotas
        if not quotas.children_enabled:
            return 501, {"error": "sub-sessions are disabled until server-side checks are proven"}
        parent = body.get("parent_session_id") or self.state.root_session_id
        if parent not in (self.state.root_session_id, *self.state.children.keys()):
            return 403, {"error": "parent is not the guest root or one of its descendants"}
        parent_depth = 0 if parent == self.state.root_session_id else self.state.children[parent].depth
        depth = parent_depth + 1
        if depth > quotas.max_depth:
            return 403, {"error": "max depth exceeded"}
        if len(self.state.children) >= quotas.max_children:
            return 403, {"error": "max children exceeded"}
        if self._create_child is None:
            return 501, {"error": "child creation is not wired to the host API yet"}
        try:
            child_id = self._create_child(
                parent=parent,
                runner_affinity=quotas.runner_affinity,
                title=str(body.get("title") or ""),
            )
        except OmnigentError as exc:
            return 502, {"error": type(exc).__name__}
        self.state.children[child_id] = _Child(session_id=child_id, parent=parent, depth=depth)
        return 200, {
            "session_id": child_id,
            "parent": parent,
            "depth": depth,
            "runner_affinity": quotas.runner_affinity,
        }
