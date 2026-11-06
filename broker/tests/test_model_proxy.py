"""Model proxy: allowlist decisions and real forwarding over a Unix socket."""

from __future__ import annotations

import http.server
import socket
import socketserver
import threading
from pathlib import Path

import pytest

from guest_broker import model_proxy
from guest_broker.errors import BrokerError


class _EchoHandler(http.server.BaseHTTPRequestHandler):
    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length)
        payload = b'{"upstream":"ok","path":"' + self.path.encode() + b'","len":' + str(len(body)).encode() + b"}"

        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self) -> None:  # noqa: N802
        payload = b'{"models":[]}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args: object) -> None:
        pass


@pytest.fixture
def upstream():
    server = socketserver.TCPServer(("127.0.0.1", 0), _EchoHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    host, port = server.server_address
    yield f"http://{host}:{port}"
    server.shutdown()
    server.server_close()


def _http_via_unix(socket_path: Path, request: bytes) -> bytes:
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.settimeout(10)
    client.connect(str(socket_path))
    client.sendall(request)
    chunks = []
    while True:
        chunk = client.recv(65536)
        if not chunk:
            break
        chunks.append(chunk)
    client.close()
    return b"".join(chunks)


def test_policy_allows_only_the_model_api():
    policy = model_proxy.ProxyPolicy("http://127.0.0.1:11434", ("/v1/",))
    assert policy.allow("POST", "/v1/chat/completions")[0]
    assert policy.allow("GET", "/v1/models")[0]
    assert not policy.allow("GET", "/")[0]
    assert not policy.allow("DELETE", "/v1/chat/completions")[0]
    assert not policy.allow("CONNECT", "/v1/")[0]
    assert not policy.allow("POST", "http://169.254.169.254/latest/meta-data")[0]
    assert not policy.allow("POST", "//evil/v1/chat")[0]
    # traversal variants must not slip past the /v1/ prefix check
    for evil in ("/v1/../../admin", "/v1/../etc/passwd", "/v1/%2e%2e/admin", "/v1//admin"):
        assert not policy.allow("GET", evil)[0], evil
    assert policy.upstream_target("/v1/models") == "/v1/models"


def test_policy_rejects_non_loopback_upstream():
    with pytest.raises(BrokerError):
        model_proxy.ProxyPolicy("http://10.0.0.5:11434", ("/v1/",))
    with pytest.raises(BrokerError):
        # a base path would silently widen what the guest can reach
        model_proxy.ProxyPolicy("http://127.0.0.1:11434/v1", ("/v1/",))


def test_socket_file_is_private(tmp_path: Path, upstream: str):
    socket_path = tmp_path / "proxy.sock"
    with model_proxy.ModelProxy(upstream, socket_path, ("/v1/",)):
        mode = socket_path.stat().st_mode & 0o777
        assert mode == 0o600


def test_forwards_allowed_request(tmp_path: Path, upstream: str):
    socket_path = tmp_path / "proxy.sock"
    with model_proxy.ModelProxy(upstream, socket_path, ("/v1/",)):
        body = b'{"model":"deepseek"}'
        response = _http_via_unix(
            socket_path,
            b"POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:11434\r\n"
            b"Content-Type: application/json\r\nContent-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body,
        )
    assert b"200 OK" in response
    assert b'"upstream":"ok"' in response
    assert b'"len":' in response


def test_denies_path_outside_allowlist(tmp_path: Path, upstream: str):
    socket_path = tmp_path / "proxy.sock"
    with model_proxy.ModelProxy(upstream, socket_path, ("/v1/",)):
        response = _http_via_unix(
            socket_path,
            b"GET /admin HTTP/1.1\r\nHost: 127.0.0.1:11434\r\n\r\n",
        )
    assert b"403" in response
    assert b"denied" in response


def test_denies_absolute_form_request(tmp_path: Path, upstream: str):
    socket_path = tmp_path / "proxy.sock"
    with model_proxy.ModelProxy(upstream, socket_path, ("/v1/",)):
        response = _http_via_unix(
            socket_path,
            b"GET http://169.254.169.254/latest/meta-data HTTP/1.1\r\nHost: x\r\n\r\n",
        )
    assert b"403" in response
