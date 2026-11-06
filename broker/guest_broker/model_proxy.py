"""Allowlisting, model-pinning HTTP forwarder for the guest's model endpoint.

The guest runs in its own network namespace, so it cannot open a socket to any
host service. The only network path it has is this proxy: a Unix socket bound
into the sandbox plus :mod:`guest-runtime/model-shim.py`, a tiny TCP-to-Unix
forwarder inside the namespace.

The proxy is deliberately narrow:

* one fixed upstream origin from config (loopback only, no base path);
* **POST only**, and only the exact model-execution endpoints in
  ``sandbox.model_proxy.allowed_paths`` (default ``/v1/chat/completions`` and
  ``/v1/completions``). Model listing (``GET /v1/models``), ``CONNECT`` and any
  other path are refused: the guest must not be able to enumerate or select
  another model through the raw API;
* the request body must be a JSON object, and its top-level ``model`` is
  **overwritten** with ``runtime.model``. The guest has a shell, so Pi's fixed
  argv alone is not a guarantee - the proxy is where the model is pinned;
* request/response sizes are capped; hop-by-hop headers are dropped and ``Host``
  is rewritten to the upstream.
"""

from __future__ import annotations

import json
import os
import socket
import socketserver
import stat
import sys
import threading
from http.client import HTTPConnection
from pathlib import Path
from typing import Iterable
from urllib.parse import urlsplit

from .errors import BrokerError

MAX_REQUEST_BYTES = 1 * 1024 * 1024
MAX_RESPONSE_BYTES = 32 * 1024 * 1024

#: Only the model *execution* endpoints are reachable. In particular
#: ``GET /v1/models`` is NOT allowed: enumerating or selecting another model must
#: not be possible through the raw endpoint, only through Pi's fixed argv.
DEFAULT_ALLOWED_PATHS = ("/v1/chat/completions", "/v1/completions")
ALLOWED_METHODS = ("POST",)
_DROP_REQUEST_HEADERS = {
    "host", "connection", "proxy-connection", "keep-alive", "transfer-encoding",
    "te", "trailer", "upgrade", "proxy-authorization", "proxy-authenticate",
}
_DROP_RESPONSE_HEADERS = {"connection", "keep-alive", "transfer-encoding", "trailer", "upgrade"}


class ProxyPolicy:
    """Pure decision logic, unit-testable without sockets."""

    def __init__(
        self,
        upstream: str,
        allowed_paths: Iterable[str] = DEFAULT_ALLOWED_PATHS,
        fixed_model: str | None = None,
    ) -> None:
        parts = urlsplit(upstream)
        if parts.scheme != "http":
            raise BrokerError(f"model proxy upstream must be http://loopback, got {upstream!r}")
        if parts.hostname not in ("127.0.0.1", "localhost", "::1"):
            raise BrokerError(f"model proxy upstream must be loopback, got {parts.hostname!r}")
        if parts.path not in ("", "/"):
            raise BrokerError(
                f"model proxy upstream must be an origin without a path, got {parts.path!r}; "
                "list the exact model endpoints in sandbox.model_proxy.allowed_paths instead"
            )
        self.host = parts.hostname or "127.0.0.1"
        self.port = parts.port or 80
        self.allowed_paths = tuple(allowed_paths)
        if not self.allowed_paths or not all(p.startswith("/") and p != "/" for p in self.allowed_paths):
            raise BrokerError("model proxy allowed_paths must be explicit absolute paths")
        for path in self.allowed_paths:
            if path.endswith("/"):
                raise BrokerError(
                    f"model proxy allowed_paths entry {path!r} is a prefix; list exact endpoints"
                )
            if path.rstrip("/").endswith("/models"):
                raise BrokerError("model proxy must not expose model listing endpoints")
        self.fixed_model = fixed_model
        #: Redacted notes about enforcement decisions (never message bodies).
        self.notes: list[str] = []

    def allow(self, method: str, target: str) -> tuple[bool, str]:
        if method.upper() not in ALLOWED_METHODS:
            return False, f"method {method!r} not allowed (model execution is POST-only)"
        if not target.startswith("/"):
            return False, "absolute-form or non-origin request target rejected"
        if "://" in target or target.startswith("//"):
            return False, "request target must be an origin path"
        if any(bad in target for bad in ("\r", "\n", " ")):
            return False, "control characters in request target"
        lowered = target.lower()
        if "%2e" in lowered or "%2f" in lowered or "%5c" in lowered:
            return False, "percent-encoded path separators are not allowed"
        if "\\" in target or "//" in target:
            return False, "path traversal is not allowed"
        segments = [part for part in target.split("?")[0].split("/") if part]
        if any(part in (".", "..") for part in segments):
            return False, "path traversal is not allowed"
        path = target.split("?", 1)[0]
        if path not in self.allowed_paths:
            return False, f"path {path!r} is not an allowlisted model endpoint"
        return True, ""

    def upstream_target(self, target: str) -> str:
        return target

    def sanitize_body(self, path: str, body: bytes) -> tuple[bool, str, bytes]:
        """Enforce the operator's fixed model on the request body.

        The guest has a shell and can therefore POST arbitrary JSON straight at
        the model API, bypassing Pi's argv. So the proxy - not Pi - is the place
        that pins the model: every model-execution request must be a JSON object
        and its top-level ``model`` is overwritten with the configured value.
        """
        if self.fixed_model is None:
            return True, "", body
        try:
            payload = json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            return False, f"model request body must be JSON: {exc}", b""
        if not isinstance(payload, dict):
            return False, "model request body must be a JSON object", b""
        requested = payload.get("model")
        if requested is not None and not isinstance(requested, str):
            return False, "model field must be a string", b""
        if requested != self.fixed_model:
            if isinstance(requested, str):
                self.notes.append(f"model_overridden:{requested[:40]}")
            payload["model"] = self.fixed_model
        try:
            return True, "", json.dumps(payload).encode("utf-8")
        except (TypeError, ValueError) as exc:
            return False, f"cannot re-encode model request: {exc}", b""


class _Handler(socketserver.BaseRequestHandler):
    server: "_ThreadingUnixServer"

    def handle(self) -> None:  # pragma: no cover - exercised via integration test
        try:
            self._serve()
        except Exception as exc:  # keep the proxy alive, log locally
            print(f"model-proxy: handler error: {type(exc).__name__}: {exc}", file=sys.stderr, flush=True)

    def _read_head(self) -> tuple[str, str, dict[str, str], bytes] | None:
        buffer = b""
        while b"\r\n\r\n" not in buffer:
            chunk = self.request.recv(8192)
            if not chunk:
                return None
            buffer += chunk
            if len(buffer) > MAX_REQUEST_BYTES:
                raise BrokerError("request head too large")
        head, _, rest = buffer.partition(b"\r\n\r\n")
        try:
            text = head.decode("iso-8859-1")
        except UnicodeDecodeError:
            raise BrokerError("request head is not latin-1")
        lines = text.split("\r\n")
        try:
            method, target, _version = lines[0].split(" ")
        except ValueError:
            raise BrokerError("malformed request line")
        headers: dict[str, str] = {}
        for line in lines[1:]:
            if not line:
                continue
            if ":" not in line:
                raise BrokerError("malformed header line")
            key, _, value = line.partition(":")
            headers[key.strip().lower()] = value.strip()
        return method, target, headers, rest

    def _serve(self) -> None:
        policy: ProxyPolicy = self.server.policy
        parsed = self._read_head()
        if parsed is None:
            return
        method, target, headers, body = parsed

        allowed, reason = policy.allow(method, target)
        if not allowed:
            self._respond(403, f"model proxy denied: {reason}")
            return

        length = int(headers.get("content-length") or 0)
        if length > MAX_REQUEST_BYTES:
            self._respond(413, "request body too large")
            return
        while len(body) < length:
            chunk = self.request.recv(min(65536, length - len(body)))
            if not chunk:
                break
            body += chunk
        if len(body) != length:
            self._respond(400, "truncated request body")
            return

        path = target.split("?", 1)[0]
        body_ok, body_reason, body = policy.sanitize_body(path, body)
        if not body_ok:
            self._respond(400, f"model proxy rejected the body: {body_reason}")
            return

        # Header names are lower-cased on read; rebuild them lower-case so the
        # rewritten Content-Length cannot coexist with a stale duplicate (which
        # would truncate the pinned-model body).
        out_headers = {
            k.lower(): v for k, v in headers.items() if k.lower() not in _DROP_REQUEST_HEADERS
        }
        out_headers["content-length"] = str(len(body))
        conn = HTTPConnection(policy.host, policy.port, timeout=120)
        try:
            conn.request(
                method.upper(),
                policy.upstream_target(target),
                body=body if body else None,
                headers={**out_headers, "Host": f"{policy.host}:{policy.port}"},
            )
            upstream = conn.getresponse()
            payload = upstream.read(MAX_RESPONSE_BYTES + 1)
            if len(payload) > MAX_RESPONSE_BYTES:
                self._respond(502, "upstream response too large")
                return
            resp_headers = [
                (k, v) for k, v in upstream.getheaders() if k.lower() not in _DROP_RESPONSE_HEADERS
            ]
            resp_headers.append(("Content-Length", str(len(payload))))
            resp_headers.append(("Connection", "close"))
            self.request.sendall(
                f"HTTP/1.1 {upstream.status} {upstream.reason}\r\n".encode("latin-1")
                + b"".join(f"{k}: {v}\r\n".encode("latin-1") for k, v in resp_headers)
                + b"\r\n"
                + payload
            )
        finally:
            conn.close()

    def _respond(self, status: int, message: str) -> None:
        body = message.encode("utf-8")
        self.request.sendall(
            f"HTTP/1.1 {status} Error\r\nContent-Type: text/plain\r\n"
            f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode("latin-1") + body
        )


class _ThreadingUnixServer(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, path: str, policy: ProxyPolicy) -> None:
        self.policy = policy
        super().__init__(path, _Handler)

    def server_bind(self) -> None:
        # Tight permissions before the socket becomes connectable.
        old_umask = os.umask(0o177)
        try:
            super().server_bind()
        finally:
            os.umask(old_umask)
        os.chmod(self.server_address, 0o600)


class ModelProxy:
    """Run the allowlisting proxy on a Unix socket (broker side)."""

    def __init__(
        self,
        upstream: str,
        socket_path: Path,
        allowed_paths: Iterable[str] = DEFAULT_ALLOWED_PATHS,
        fixed_model: str | None = None,
    ) -> None:
        self.policy = ProxyPolicy(upstream, allowed_paths, fixed_model)
        self.socket_path = Path(socket_path)
        self._server: _ThreadingUnixServer | None = None
        self._thread: threading.Thread | None = None

    def start(self) -> None:
        self.socket_path.parent.mkdir(parents=True, exist_ok=True)
        if self.socket_path.exists():
            if not stat.S_ISSOCK(self.socket_path.stat().st_mode):
                raise BrokerError(f"{self.socket_path} exists and is not a socket")
            self.socket_path.unlink()
        self._server = _ThreadingUnixServer(str(self.socket_path), self.policy)
        os.chmod(self.socket_path, 0o600)
        self._thread = threading.Thread(target=self._server.serve_forever, name="model-proxy", daemon=True)
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

    def __enter__(self) -> "ModelProxy":
        self.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self.stop()


def probe_upstream(upstream: str, timeout: float = 3.0) -> tuple[bool, str]:
    """Cheap reachability check for the model endpoint (no credentials sent)."""
    parts = urlsplit(upstream)
    host = parts.hostname or "127.0.0.1"
    port = parts.port or 80
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True, f"{host}:{port} reachable"
    except OSError as exc:
        return False, f"{host}:{port} unreachable: {exc}"
