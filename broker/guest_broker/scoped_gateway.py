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
  * child-session policy is a disabled/test-only hook; the shipped launcher always
    returns 501, with no live creator, persistent ancestry or verified affinity;
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

from .omnigent_api import (
    OmnigentAPI,
    OmnigentAuthError,
    OmnigentError,
    OmnigentSessionNotFound,
)


def _upstream_status(exc: OmnigentError) -> int:
    """Preserve the upstream taxonomy instead of flattening everything to 502.

    The broker's relay contract depends on 401/403/404 being definitive (clear
    pending) and everything else being "unknown" (never auto-repost).
    """
    if isinstance(exc, OmnigentAuthError):
        return 401
    if isinstance(exc, OmnigentSessionNotFound):
        return 404
    status = getattr(exc, "status", None)
    if isinstance(status, int) and 400 <= status <= 599:
        return status
    return 502


MAX_BODY_BYTES = 64 * 1024
MAX_TEXT_CHARS = 8000


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
        self._children_lock = threading.Lock()

    # -- lifecycle -------------------------------------------------------
    def start(self) -> None:
        self.socket_path.parent.mkdir(parents=True, exist_ok=True)
        if self.socket_path.exists() or self.socket_path.is_symlink():
            import stat as _stat

            if self.socket_path.is_symlink() or not _stat.S_ISSOCK(self.socket_path.stat().st_mode):
                raise ScopedAccessError(
                    f"refusing to replace {self.socket_path}: it exists and is not a socket"
                )
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
                # One request per connection. Besides simplifying the client, this
                # prevents rejected/truncated bodies from becoming a second request
                # through HTTP pipelining or request-smuggling desynchronization.
                self.send_header("Connection", "close")
                self.close_connection = True
                self.end_headers()
                self.wfile.write(raw)

            def _authorised(self) -> bool:
                header = self.headers.get("Authorization", "")
                expected = f"Bearer {gateway._secret}"
                return hmac.compare_digest(header, expected)

            def _body(self) -> tuple[bool, dict[str, Any]]:
                """(ok, body). Malformed/oversized/not-an-object bodies are 400s."""
                if self.headers.get("Transfer-Encoding") is not None:
                    return False, {"error": "Transfer-Encoding is not supported"}
                lengths = self.headers.get_all("Content-Length") or []
                if len(lengths) > 1:
                    return False, {"error": "duplicate Content-Length is not allowed"}
                try:
                    length = int(lengths[0]) if lengths else 0
                except ValueError:
                    return False, {"error": "invalid Content-Length"}
                if length < 0 or length > MAX_BODY_BYTES:
                    return False, {"error": "request body exceeds the 64 KiB limit"}
                if length == 0:
                    return True, {}
                content_type = self.headers.get("Content-Type", "").split(";", 1)[0].strip().lower()
                if content_type != "application/json":
                    return False, {"error": "Content-Type must be application/json"}
                raw = self.rfile.read(length)
                if len(raw) != length:
                    return False, {"error": "truncated request body"}
                try:
                    parsed = json.loads(raw)
                except (json.JSONDecodeError, UnicodeDecodeError):
                    return False, {"error": "body must be a JSON object"}
                if not isinstance(parsed, dict):
                    return False, {"error": "body must be a JSON object"}
                return True, parsed

            def do_GET(self) -> None:  # noqa: N802
                if not self._authorised():
                    self._send(401, {"error": "unauthorized"})
                    return
                parsed = urllib.parse.urlsplit(self.path)
                if parsed.path == "/v1/guest/session":
                    try:
                        info = gateway.api.session(gateway.state.root_session_id)
                    except OmnigentError as exc:
                        self._send(_upstream_status(exc), {"error": type(exc).__name__})
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
                    query = urllib.parse.parse_qs(parsed.query, keep_blank_values=True)
                    if set(query) - {"after", "limit", "order"} or any(
                        len(values) != 1 for values in query.values()
                    ):
                        self._send(400, {"error": "unknown or repeated items query parameter"})
                        return
                    after = (query.get("after") or [None])[0]
                    if after is not None and len(after) > 256:
                        self._send(400, {"error": "after cursor is too long"})
                        return
                    try:
                        limit = int((query.get("limit") or ["100"])[0])
                    except ValueError:
                        self._send(400, {"error": "limit must be an integer"})
                        return
                    if not 1 <= limit <= 1000:
                        self._send(400, {"error": "limit must be within 1..1000"})
                        return
                    order = (query.get("order") or ["asc"])[0]
                    if order not in ("asc", "desc"):
                        self._send(400, {"error": "order must be asc or desc"})
                        return
                    try:
                        page = gateway.api.list_items(
                            gateway.state.root_session_id, after=after, limit=limit, order=order
                        )
                    except OmnigentError as exc:
                        self._send(_upstream_status(exc), {"error": type(exc).__name__})
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
                ok, body = self._body()
                if not ok:
                    self._send(400, body)
                    return
                # The root is not caller-selectable.
                if "session_id" in body or "session" in body:
                    self._send(400, {"error": "session id is fixed by the gateway"})
                    return
                if path == "/v1/guest/message":
                    if set(body) - {"text", "msg_id"}:
                        self._send(400, {"error": "unknown message field"})
                        return
                    text = body.get("text")
                    msg_id = body.get("msg_id", "gateway")
                    if not isinstance(text, str) or not text.strip():
                        self._send(400, {"error": "text is required"})
                        return
                    if len(text) > MAX_TEXT_CHARS:
                        self._send(400, {"error": "text is too long"})
                        return
                    if not isinstance(msg_id, str) or not msg_id or len(msg_id) > 128:
                        self._send(400, {"error": "msg_id must be a string of 1..128 characters"})
                        return
                    try:
                        gateway.api.post_message(
                            gateway.state.root_session_id, text, msg_id=msg_id,
                        )
                    except OmnigentError as exc:
                        self._send(_upstream_status(exc), {"error": type(exc).__name__})
                        return
                    self._send(200, {"queued": True})
                    return
                if path == "/v1/guest/children":
                    if set(body) - {"title", "parent_session_id"}:
                        self._send(400, {"error": "unknown child-request field"})
                        return
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
        if not quotas.runner_affinity:
            # Without the root's runner there is no way to keep descendant tool
            # execution inside the guest container, so refuse rather than guess.
            return 501, {"error": "runner affinity for the guest root is not configured"}
        parent = body.get("parent_session_id") or self.state.root_session_id
        if parent not in (self.state.root_session_id, *self.state.children.keys()):
            return 403, {"error": "parent is not the guest root or one of its descendants"}
        with self._children_lock:
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
                return _upstream_status(exc), {"error": type(exc).__name__}
            if not isinstance(child_id, str) or not child_id.startswith("conv_"):
                return 502, {"error": "child creation returned an unusable session id"}
            self.state.children[child_id] = _Child(session_id=child_id, parent=parent, depth=depth)
        return 200, {
            "session_id": child_id,
            "parent": parent,
            "depth": depth,
            "runner_affinity": quotas.runner_affinity,
        }
