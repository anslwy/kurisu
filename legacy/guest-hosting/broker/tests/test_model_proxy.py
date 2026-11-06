"""Model proxy: strict endpoint allowlist + fixed-model enforcement.

Adversarial focus: the guest has a shell, so it can POST arbitrary JSON straight
at the model API. Pi's fixed argv is therefore not enough on its own - the proxy
must pin the model and must not expose listing or other endpoints.
"""

from __future__ import annotations

import http.server
import json
import socket
import socketserver
import threading
from pathlib import Path

import pytest

from guest_broker import model_proxy
from guest_broker.errors import BrokerError

FIXED_MODEL = "deepseek-v4.1-flash:cloud"


class _EchoHandler(http.server.BaseHTTPRequestHandler):
    received: list[dict] = []

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length)
        try:
            parsed = json.loads(body.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError):
            parsed = {"_raw": "<not json>"}
        _EchoHandler.received.append({"path": self.path, "body": parsed, "len": len(body)})
        payload = b'{"upstream":"ok","path":"' + self.path.encode() + b'"}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self) -> None:  # noqa: N802
        payload = b'{"data":[{"id":"kimi-k3.1-pro:cloud"}]}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *args: object) -> None:
        pass


@pytest.fixture
def upstream():
    _EchoHandler.received = []
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


def _post(socket_path: Path, path: str, body: dict) -> bytes:
    raw = json.dumps(body).encode()
    return _http_via_unix(
        socket_path,
        f"POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n"
        f"Content-Length: {len(raw)}\r\n\r\n".encode() + raw,
    )


# ------------------------------------------------------------- policy
def test_policy_allows_only_exact_model_endpoints():
    policy = model_proxy.ProxyPolicy("http://127.0.0.1:11434", fixed_model=FIXED_MODEL)
    assert policy.allow("POST", "/v1/chat/completions")[0]
    assert policy.allow("POST", "/v1/completions")[0]
    # listing models must not be possible through the raw endpoint
    assert not policy.allow("GET", "/v1/models")[0]
    assert not policy.allow("POST", "/v1/models")[0]
    assert not policy.allow("GET", "/v1/chat/completions")[0]
    assert not policy.allow("POST", "/v1/embeddings")[0]
    assert not policy.allow("POST", "/v1/")[0]
    assert not policy.allow("CONNECT", "/v1/chat/completions")[0]
    assert not policy.allow("POST", "http://169.254.169.254/v1/chat/completions")[0]
    for evil in ("/v1/../../admin", "/v1/../etc/passwd", "/v1/%2e%2e/admin", "//v1/chat/completions"):
        assert not policy.allow("POST", evil)[0], evil


def test_policy_rejects_bad_upstream_and_prefix_config():
    with pytest.raises(BrokerError):
        model_proxy.ProxyPolicy("http://10.0.0.5:11434")
    with pytest.raises(BrokerError):
        model_proxy.ProxyPolicy("http://127.0.0.1:11434/v1")
    with pytest.raises(BrokerError, match="prefix"):
        model_proxy.ProxyPolicy("http://127.0.0.1:11434", ("/v1/",))
    with pytest.raises(BrokerError, match="listing"):
        model_proxy.ProxyPolicy("http://127.0.0.1:11434", ("/v1/models",))


# ------------------------------------------------------------- body pinning
def test_sanitize_body_overwrites_any_model_the_guest_asks_for():
    policy = model_proxy.ProxyPolicy("http://127.0.0.1:11434", fixed_model=FIXED_MODEL)
    ok, reason, body = policy.sanitize_body(
        "/v1/chat/completions",
        json.dumps({"model": "kimi-k3.1-pro:cloud", "messages": [{"role": "user", "content": "hi"}]}).encode(),
    )
    assert ok, reason
    assert json.loads(body)["model"] == FIXED_MODEL
    assert any(note.startswith("model_overridden:") for note in policy.notes)


def test_sanitize_body_injects_model_when_absent():
    policy = model_proxy.ProxyPolicy("http://127.0.0.1:11434", fixed_model=FIXED_MODEL)
    ok, _, body = policy.sanitize_body("/v1/completions", b'{"prompt":"hi"}')
    assert ok and json.loads(body)["model"] == FIXED_MODEL


def test_sanitize_body_rejects_malformed_or_wrong_shapes():
    policy = model_proxy.ProxyPolicy("http://127.0.0.1:11434", fixed_model=FIXED_MODEL)
    assert not policy.sanitize_body("/v1/chat/completions", b"not json")[0]
    assert not policy.sanitize_body("/v1/chat/completions", b"[1,2,3]")[0]
    assert not policy.sanitize_body("/v1/chat/completions", b'{"model": 42}')[0]
    assert not policy.sanitize_body("/v1/chat/completions", b"\xff\xfe")[0]


# ------------------------------------------------------------- live proxy
def test_socket_file_is_private(tmp_path: Path, upstream: str):
    socket_path = tmp_path / "proxy.sock"
    with model_proxy.ModelProxy(upstream, socket_path, fixed_model=FIXED_MODEL):
        assert socket_path.stat().st_mode & 0o777 == 0o600


def test_forwards_allowed_request_and_pins_the_model(tmp_path: Path, upstream: str):
    socket_path = tmp_path / "proxy.sock"
    with model_proxy.ModelProxy(upstream, socket_path, fixed_model=FIXED_MODEL):
        response = _post(
            socket_path,
            "/v1/chat/completions",
            {"model": "kimi-k3.1-pro:cloud", "messages": [{"role": "user", "content": "hi"}]},
        )
    assert b"200 OK" in response
    assert _EchoHandler.received[0]["body"]["model"] == FIXED_MODEL


def test_denies_model_listing_through_the_raw_endpoint(tmp_path: Path, upstream: str):
    socket_path = tmp_path / "proxy.sock"
    with model_proxy.ModelProxy(upstream, socket_path, fixed_model=FIXED_MODEL):
        response = _http_via_unix(socket_path, b"GET /v1/models HTTP/1.1\r\nHost: x\r\n\r\n")
    assert b"403" in response
    assert not _EchoHandler.received  # never reached upstream


def test_denies_malformed_model_body(tmp_path: Path, upstream: str):
    socket_path = tmp_path / "proxy.sock"
    with model_proxy.ModelProxy(upstream, socket_path, fixed_model=FIXED_MODEL):
        response = _http_via_unix(
            socket_path,
            b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Length: 8\r\n\r\nnot json",
        )
    assert b"400" in response
    assert not _EchoHandler.received


def test_denies_path_outside_allowlist(tmp_path: Path, upstream: str):
    socket_path = tmp_path / "proxy.sock"
    with model_proxy.ModelProxy(upstream, socket_path, fixed_model=FIXED_MODEL):
        response = _http_via_unix(socket_path, b"POST /admin HTTP/1.1\r\nHost: x\r\n\r\n")
    assert b"403" in response


def test_denies_absolute_form_request(tmp_path: Path, upstream: str):
    socket_path = tmp_path / "proxy.sock"
    with model_proxy.ModelProxy(upstream, socket_path, fixed_model=FIXED_MODEL):
        response = _http_via_unix(
            socket_path,
            b"POST http://169.254.169.254/v1/chat/completions HTTP/1.1\r\nHost: x\r\n\r\n",
        )
    assert b"403" in response
