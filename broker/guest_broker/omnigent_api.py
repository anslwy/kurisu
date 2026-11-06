"""Minimal, strict client for the Omnigent host API (broker side only).

Only what the pilot needs, and nothing else:

* ``GET  /v1/sessions/{id}``         - probe the bound session / runner state
* ``POST /v1/sessions/{id}/events``  - post one user turn
* ``GET  /v1/sessions/{id}/items``   - read conversation items with a cursor

Deliberate properties:

* **No session selection.** The session id is a constructor argument taken from
  the operator-owned registry; no method accepts a caller-supplied id, and there
  is no list/create/delete surface at all.
* **Token custody.** The bearer token comes from the broker's environment, is
  attached in :meth:`_request` only, and is never logged, returned, or embedded
  in an exception message.
* **Typed failures.** 401/403 -> :class:`OmnigentAuthError`, 404 ->
  :class:`OmnigentSessionNotFound`, everything else -> :class:`OmnigentAPIError`.
  Callers map these to a generic guest-facing message.
"""

from __future__ import annotations

import json
import re
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any, Callable, Mapping

from .errors import BrokerError

_SESSION_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9_-]{7,63}$")
MAX_ITEMS = 1000
MAX_REPLY_CHARS = 200_000

#: transport(method, url, headers, body) -> (status, decoded_json_or_text)
Transport = Callable[[str, str, Mapping[str, str], bytes | None], "tuple[int, Any]"]


class OmnigentError(BrokerError):
    """Base class for Omnigent API failures."""


class OmnigentAuthError(OmnigentError):
    """The operator token is missing, expired, or not authorized for the session."""


class OmnigentSessionNotFound(OmnigentError):
    """The provisioned session id does not exist (or is not visible to the token)."""


class OmnigentAPIError(OmnigentError):
    """Any other API failure (5xx, malformed response, network error)."""


def default_transport(
    method: str, url: str, headers: Mapping[str, str], body: bytes | None, timeout: float = 60.0
) -> tuple[int, Any]:
    request = urllib.request.Request(url, data=body, headers=dict(headers), method=method)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read()
            return response.status, _decode(raw)
    except urllib.error.HTTPError as exc:
        raw = exc.read()
        return exc.code, _decode(raw)
    except urllib.error.URLError as exc:
        raise OmnigentAPIError(f"cannot reach Omnigent at {urllib.parse.urlsplit(url).netloc}: {exc.reason}") from exc
    except TimeoutError as exc:
        raise OmnigentAPIError("Omnigent request timed out") from exc


def _decode(raw: bytes) -> Any:
    if not raw:
        return {}
    try:
        return json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return {"_raw": raw[:500].decode("utf-8", "replace")}


@dataclass(frozen=True)
class Page:
    data: list[dict[str, Any]]
    first_id: str | None
    last_id: str | None
    has_more: bool


@dataclass(frozen=True)
class SessionInfo:
    session_id: str
    runner_online: bool
    agent: str
    model: str
    raw: Mapping[str, Any]


def validate_session_id(session_id: str) -> str:
    if not _SESSION_ID.fullmatch(session_id or ""):
        raise OmnigentError(f"invalid session id {session_id!r}: expected an opaque id")
    return session_id


class OmnigentAPI:
    """Three endpoints, one pinned session, one bearer token."""

    def __init__(
        self,
        base_url: str,
        token: str,
        *,
        session_id: str | None = None,
        transport: Transport | None = None,
        timeout: float = 60.0,
        clock: Callable[[], float] = time.time,
    ) -> None:
        if not base_url.startswith(("http://127.0.0.1", "http://localhost", "https://")):
            raise OmnigentError("base_url must be loopback or https")
        if not token:
            raise OmnigentAuthError("no operator API token configured")
        self.base_url = base_url.rstrip("/")
        self._token = token
        self.session_id = validate_session_id(session_id) if session_id else None
        self._transport = transport or (lambda m, u, h, b: default_transport(m, u, h, b, timeout))
        self._clock = clock

    # -- plumbing --------------------------------------------------------
    def _url(self, session_id: str, suffix: str = "") -> str:
        session_id = validate_session_id(session_id)
        if self.session_id and session_id != self.session_id:
            raise OmnigentError(
                f"requested session id does not match the provisioned session "
                f"({session_id!r} != {self.session_id!r})"
            )
        return f"{self.base_url}/v1/sessions/{urllib.parse.quote(session_id, safe='')}{suffix}"

    def _request(self, method: str, url: str, body: Mapping[str, Any] | None = None) -> Any:
        headers = {
            "Authorization": f"Bearer {self._token}",
            "Accept": "application/json",
        }
        payload = None
        if body is not None:
            payload = json.dumps(body, separators=(",", ":")).encode()
            headers["Content-Type"] = "application/json"
        status, data = self._transport(method, url, headers, payload)
        if status in (401, 403):
            # Never echo the body: it can contain request echoes.
            raise OmnigentAuthError(f"Omnigent rejected the operator token (HTTP {status})")
        if status == 404:
            raise OmnigentSessionNotFound("the provisioned guest session was not found")
        if status >= 400:
            detail = ""
            if isinstance(data, dict):
                detail = str(data.get("detail") or data.get("error") or "")[:200]
            raise OmnigentAPIError(f"Omnigent returned HTTP {status}{': ' + detail if detail else ''}")
        return data

    # -- pinned-session helpers -----------------------------------------
    def session(self, session_id: str) -> SessionInfo:
        data = self._request("GET", self._url(session_id))
        if not isinstance(data, dict):
            raise OmnigentAPIError("session response was not an object")
        runner = data.get("runner") if isinstance(data.get("runner"), dict) else {}
        online = bool(
            data.get("runner_online")
            or (isinstance(runner, dict) and runner.get("online"))
            or data.get("status") == "running"
        )
        return SessionInfo(
            session_id=session_id,
            runner_online=online,
            agent=str(data.get("agent_name") or data.get("agent_id") or ""),
            model=str(data.get("model") or ""),
            raw=data,
        )

    def post_message(self, session_id: str, text: str, *, msg_id: str) -> dict[str, Any]:
        """Post one user turn. The body shape is the host's message event."""
        body = {
            "type": "message",
            "data": {
                "role": "user",
                "content": [{"type": "input_text", "text": text}],
                "user_authored": True,
            },
        }
        data = self._request("POST", self._url(session_id, "/events"), body)
        return data if isinstance(data, dict) else {"ok": True}

    def list_items(
        self, session_id: str, *, after: str | None = None, limit: int = 100, order: str = "asc"
    ) -> Page:
        if not 1 <= limit <= MAX_ITEMS:
            raise OmnigentError(f"limit must be within 1..{MAX_ITEMS}")
        if order not in ("asc", "desc"):
            raise OmnigentError("order must be 'asc' or 'desc'")
        url = self._url(session_id, "/items") + f"?limit={limit}&order={order}"
        if after:
            url += "&after=" + urllib.parse.quote(str(after), safe="")
        data = self._request("GET", url)
        if not isinstance(data, dict):
            raise OmnigentAPIError("items response was not an object")
        items = data.get("data")
        if not isinstance(items, list):
            raise OmnigentAPIError("items response has no 'data' list")
        return Page(
            data=[item for item in items if isinstance(item, dict)],
            first_id=data.get("first_id"),
            last_id=data.get("last_id"),
            has_more=bool(data.get("has_more")),
        )

    # -- assistant text extraction --------------------------------------
    @staticmethod
    def assistant_text(item: Mapping[str, Any]) -> str:
        """Best-effort extraction of assistant text from a conversation item.

        The host's item shape varies by harness (plain text parts, content blocks,
        or a nested message), so this recognises the documented shapes and returns
        "" for anything it does not understand - never a guess.
        """
        role = str(item.get("role") or "")
        if role not in ("assistant", "agent"):
            # Some shapes nest the message under 'message'
            nested = item.get("message")
            if isinstance(nested, dict) and str(nested.get("role")) in ("assistant", "agent"):
                return _text_from(nested)
            return ""
        text = _text_from(item)
        return text[:MAX_REPLY_CHARS]


def _text_from(container: Mapping[str, Any]) -> str:
    for key in ("text", "content", "blocks", "parts"):
        value = container.get(key)
        if isinstance(value, str):
            return value
        if isinstance(value, list):
            chunks: list[str] = []
            for part in value:
                if isinstance(part, str):
                    chunks.append(part)
                elif isinstance(part, dict):
                    for inner in ("text", "content", "value"):
                        candidate = part.get(inner)
                        if isinstance(candidate, str) and candidate:
                            chunks.append(candidate)
                            break
            joined = "\n".join(chunk for chunk in chunks if chunk)
            if joined:
                return joined
        if isinstance(value, dict):
            for inner in ("text", "content"):
                candidate = value.get(inner)
                if isinstance(candidate, str):
                    return candidate
    return ""
