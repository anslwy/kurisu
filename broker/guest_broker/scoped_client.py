"""Broker-side client for the scoped Omnigent gateway.

The broker never holds the operator token when this mode is active. It talks to
:mod:`guest_broker.scoped_gateway` over a 0600 Unix socket with a local shared
secret, and the gateway is the only process that can reach the Omnigent API.

The client duck-types the three methods :class:`OmnigentBackend` uses
(``session``, ``post_message``, ``list_items`` plus ``assistant_text``), so the
relay logic is unchanged and cannot accidentally address another session: every
call validates the requested id against the root it was constructed with.
"""

from __future__ import annotations

import hmac
import json
import socket
import urllib.parse
from http.client import HTTPConnection
from pathlib import Path
from typing import Any

from .errors import BrokerError
from .omnigent_api import Page, SessionInfo


class ScopedClientError(BrokerError):
    """The gateway refused or could not serve the request."""


class ScopedClient:
    """Minimal Unix-socket HTTP client, pinned to one guest root session."""

    def __init__(self, socket_path: Path, root_session_id: str, secret: str, *, timeout: float = 60.0) -> None:
        if not secret:
            raise ScopedClientError("the scoped client needs the gateway shared secret")
        self.socket_path = Path(socket_path)
        self.session_id = root_session_id
        self._secret = secret
        self.timeout = timeout

    # -- plumbing --------------------------------------------------------
    def _request(self, method: str, path: str, body: dict[str, Any] | None = None) -> dict[str, Any]:
        class _UnixConnection(HTTPConnection):
            def __init__(self, unix_path: str) -> None:
                super().__init__("localhost", timeout=self.timeout if hasattr(self, "timeout") else 60)
                self._unix_path = unix_path

            def connect(self) -> None:  # noqa: D401 - stdlib hook
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.settimeout(self.timeout)
                sock.connect(self._unix_path)
                self.sock = sock

        conn = _UnixConnection(str(self.socket_path))
        conn.timeout = self.timeout
        headers = {
            "Authorization": f"Bearer {self._secret}",
            "Content-Type": "application/json",
            "Accept": "application/json",
        }
        payload = json.dumps(body).encode() if body is not None else None
        try:
            conn.request(method, path, body=payload, headers=headers)
            response = conn.getresponse()
            raw = response.read()
        except OSError as exc:
            raise ScopedClientError(f"scoped gateway unreachable: {exc}") from exc
        finally:
            conn.close()
        try:
            decoded = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            decoded = {"_raw": raw[:200].decode("utf-8", "replace")}
        if response.status == 401:
            raise ScopedClientError("scoped gateway rejected the shared secret")
        if response.status >= 400:
            raise ScopedClientError(f"scoped gateway returned HTTP {response.status}")
        return decoded if isinstance(decoded, dict) else {}

    def _check(self, session_id: str) -> None:
        if session_id != self.session_id:
            raise ScopedClientError(
                f"session id {session_id!r} is not the provisioned guest root "
                f"({self.session_id!r})"
            )

    # -- OmnigentAPI-compatible surface ---------------------------------
    def session(self, session_id: str) -> SessionInfo:
        self._check(session_id)
        data = self._request("GET", "/v1/guest/session")
        return SessionInfo(
            session_id=self.session_id,
            runner_online=bool(data.get("runner_online")),
            agent=str(data.get("agent") or ""),
            model=str(data.get("model") or ""),
            raw=data,
        )

    def post_message(self, session_id: str, text: str, *, msg_id: str = "") -> dict[str, Any]:
        self._check(session_id)
        return self._request("POST", "/v1/guest/message", {"text": text, "msg_id": msg_id})

    def list_items(
        self, session_id: str, *, after: str | None = None, limit: int = 100, order: str = "asc"
    ) -> Page:
        self._check(session_id)
        if order not in ("asc", "desc"):
            raise ScopedClientError("order must be 'asc' or 'desc'")
        query = f"?limit={int(limit)}&order={order}"
        if after:
            query += "&after=" + urllib.parse.quote(str(after), safe="")
        data = self._request("GET", "/v1/guest/items" + query)
        items = data.get("data")
        if not isinstance(items, list):
            raise ScopedClientError("scoped gateway items response has no data list")
        return Page(
            data=[item for item in items if isinstance(item, dict)],
            first_id=data.get("first_id"),
            last_id=data.get("last_id"),
            has_more=bool(data.get("has_more")),
        )

    @staticmethod
    def assistant_text(item: Any) -> str:
        # Same extraction as the direct client; importing it keeps one definition.
        from .omnigent_api import OmnigentAPI

        return OmnigentAPI.assistant_text(item)

    # -- children (disabled until the checks are proven live) -----------
    def children(self) -> list[dict[str, Any]]:
        data = self._request("GET", "/v1/guest/children")
        items = data.get("data")
        return [item for item in items if isinstance(item, dict)] if isinstance(items, list) else []

    def create_child(self, *, parent_session_id: str, title: str = "") -> dict[str, Any]:
        if parent_session_id != self.session_id:
            # Descendants are requested through the root; the gateway assigns the
            # actual parent and enforces depth/count itself.
            raise ScopedClientError("children must be requested through the guest root session")
        return self._request("POST", "/v1/guest/children", {"title": title})


def secret_matches(provided: str, expected: str) -> bool:
    """Constant-time comparison helper (used by tests and the gateway)."""
    return hmac.compare_digest(provided, expected)
