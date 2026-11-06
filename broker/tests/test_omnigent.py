"""Omnigent API client + relay backend, against a fake Omnigent server.

Nothing here touches a live Omnigent instance: a local HTTP server implements the
three endpoints the broker uses, so the API contract, relay, dedup, restart and
failure paths are all exercised deterministically.
"""

from __future__ import annotations

import json
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from guest_broker import config as config_mod
from guest_broker import omnigent_api
from guest_broker.backend import build_backend
from guest_broker.errors import LimitExceededError, SessionBackendError
from guest_broker.omnigent_api import (
    OmnigentAPI,
    OmnigentAPIError,
    OmnigentAuthError,
    OmnigentError,
    OmnigentSessionNotFound,
)
from guest_broker.omnigent_backend import OmnigentBackend, RelayStore

from .conftest import make_config

SESSION_ID = "conv_0123456789abcdef"
TOKEN = "operator-secret-token"
OTHER_SESSION = "conv_ffffffffffffffff"


class FakeOmnigent:
    """Tiny stand-in for the host API: sessions, events, items."""

    def __init__(self) -> None:
        self.session_id = SESSION_ID
        self.token = TOKEN
        self.items: list[dict] = []
        self.posts: list[dict] = []
        self.requests: list[tuple[str, str]] = []
        self.reply_text = "answer from the runner"
        self.auto_reply = True
        self.status_override: int | None = None
        self.runner_online = True
        self.model = "deepseek-v4.1-flash:cloud"
        self.agent = "guest-whatsapp-pilot"

    # -- server -----------------------------------------------------------
    def start(self) -> str:
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def _send(self, status: int, payload: dict) -> None:
                raw = json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            def _authorised(self) -> bool:
                return self.headers.get("Authorization") == f"Bearer {fake.token}"

            def do_GET(self) -> None:  # noqa: N802
                fake.requests.append(("GET", self.path))
                if fake.status_override:
                    self._send(fake.status_override, {"detail": "injected failure"})
                    return
                if not self._authorised():
                    self._send(401, {"detail": "bad token"})
                    return
                parsed = urllib.parse.urlsplit(self.path)
                parts = parsed.path.strip("/").split("/")
                # v1 / sessions / {id} [/ items]
                if len(parts) >= 3 and parts[1] == "sessions":
                    if parts[2] != fake.session_id:
                        self._send(404, {"detail": "session not found"})
                        return
                    if len(parts) == 3:
                        self._send(200, {
                            "session_id": fake.session_id,
                            "runner": {"online": fake.runner_online},
                            "agent_name": fake.agent,
                            "model": fake.model,
                        })
                        return
                    if parts[3] == "items":
                        query = urllib.parse.parse_qs(parsed.query)
                        after = (query.get("after") or [None])[0]
                        data = fake.items
                        if after:
                            ids = [item["id"] for item in fake.items]
                            if after in ids:
                                data = fake.items[ids.index(after) + 1:]
                        self._send(200, {
                            "data": data,
                            "first_id": data[0]["id"] if data else None,
                            "last_id": data[-1]["id"] if data else None,
                            "has_more": False,
                        })
                        return
                self._send(404, {"detail": "no route"})

            def do_POST(self) -> None:  # noqa: N802
                fake.requests.append(("POST", self.path))
                if fake.status_override:
                    self._send(fake.status_override, {"detail": "injected failure"})
                    return
                if not self._authorised():
                    self._send(401, {"detail": "bad token"})
                    return
                length = int(self.headers.get("content-length") or 0)
                body = json.loads(self.rfile.read(length) or b"{}")
                parts = self.path.strip("/").split("/")
                if len(parts) != 4 or parts[1] != "sessions" or parts[3] != "events":
                    self._send(404, {"detail": "no route"})
                    return
                if parts[2] != fake.session_id:
                    self._send(404, {"detail": "session not found"})
                    return
                fake.posts.append(body)
                fake.items.append({"id": f"item{len(fake.items) + 1}", "role": "user", "text": "posted"})
                if fake.auto_reply:
                    fake.items.append({
                        "id": f"item{len(fake.items) + 1}",
                        "role": "assistant",
                        "content": [{"type": "text", "text": fake.reply_text}],
                    })
                self._send(200, {"queued": True})

            def log_message(self, *args: object) -> None:
                pass

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self._server.serve_forever, daemon=True).start()
        host, port = self._server.server_address
        return f"http://{host}:{port}"

    def stop(self) -> None:
        self._server.shutdown()
        self._server.server_close()


@pytest.fixture
def fake():
    server = FakeOmnigent()
    base_url = server.start()
    server.base_url = base_url  # type: ignore[attr-defined]
    yield server
    server.stop()


def make_api(fake, *, session_id: str = SESSION_ID, token: str = TOKEN) -> OmnigentAPI:
    return OmnigentAPI(fake.base_url, token, session_id=session_id)


# ------------------------------------------------------------- API contract
def test_post_message_uses_the_provisioned_session_and_bearer_token(fake):
    api = make_api(fake)
    api.post_message(SESSION_ID, "hello", msg_id="msg1")
    method, path = fake.requests[-1]
    assert method == "POST"
    assert path == f"/v1/sessions/{SESSION_ID}/events"
    body = fake.posts[-1]
    assert body["type"] == "message"
    assert body["data"]["role"] == "user"
    assert body["data"]["content"][0]["text"] == "hello"
    assert body["data"]["user_authored"] is True


def test_api_refuses_a_different_session_id(fake):
    api = make_api(fake)
    with pytest.raises(OmnigentError, match="does not match"):
        api.post_message(OTHER_SESSION, "hello", msg_id="msg1")
    with pytest.raises(OmnigentError, match="does not match"):
        api.list_items(OTHER_SESSION)
    with pytest.raises(OmnigentError, match="does not match"):
        api.session(OTHER_SESSION)
    assert fake.posts == []


def test_invalid_session_id_shape_is_rejected():
    with pytest.raises(OmnigentError, match="invalid session id"):
        OmnigentAPI("http://127.0.0.1:1", TOKEN, session_id="../../etc/passwd")


def test_auth_failures_are_typed_and_do_not_leak_the_token(fake):
    api = make_api(fake, token="wrong-token")
    with pytest.raises(OmnigentAuthError) as excinfo:
        api.list_items(SESSION_ID)
    assert "wrong-token" not in str(excinfo.value)


def test_missing_session_is_typed(fake):
    api = OmnigentAPI(fake.base_url, TOKEN, session_id=OTHER_SESSION)
    with pytest.raises(OmnigentSessionNotFound):
        api.list_items(OTHER_SESSION)


def test_server_errors_are_typed(fake):
    fake.status_override = 500
    api = make_api(fake)
    with pytest.raises(OmnigentAPIError) as excinfo:
        api.list_items(SESSION_ID)
    assert TOKEN not in str(excinfo.value)


def test_session_probe_reports_runner_state(fake):
    api = make_api(fake)
    info = api.session(SESSION_ID)
    assert info.runner_online is True
    assert info.model == "deepseek-v4.1-flash:cloud"
    fake.runner_online = False
    assert api.session(SESSION_ID).runner_online is False


def test_items_use_the_cursor(fake):
    fake.items = [
        {"id": "item1", "role": "user", "text": "a"},
        {"id": "item2", "role": "assistant", "text": "b"},
    ]
    api = make_api(fake)
    page = api.list_items(SESSION_ID, after="item1")
    assert [item["id"] for item in page.data] == ["item2"]
    assert "/items" in fake.requests[-1][1]
    assert "after=item1" in fake.requests[-1][1]


@pytest.mark.parametrize(
    "item,expected",
    [
        ({"role": "assistant", "text": "plain"}, "plain"),
        ({"role": "assistant", "content": [{"type": "text", "text": "blocks"}]}, "blocks"),
        ({"role": "assistant", "content": "string content"}, "string content"),
        ({"role": "user", "text": "nope"}, ""),
        ({"role": "assistant"}, ""),
    ],
)
def test_assistant_text_extraction(item, expected):
    assert OmnigentAPI.assistant_text(item) == expected


# ------------------------------------------------------------- relay backend
def _backend(fake, tmp_path: Path, **kwargs) -> OmnigentBackend:
    return OmnigentBackend(
        make_api(fake),
        RelayStore(tmp_path / "relay.json"),
        poll_interval=0.01,
        turn_timeout=2.0,
        **kwargs,
    )


from guest_broker import config as _cfg  # noqa: E402


def _fake_limits():
    return _cfg.Limits(
        memory_bytes=1024, memory_swap_bytes=0, cpu_quota="10%", tasks_max=8,
        runtime_max_seconds=5, file_size_bytes=1024, tmpfs_bytes=1024, nofile=64,
        io_weight=100, disk_quota_bytes=1024,
    )


_FAKE_LIMITS = _fake_limits()
_FAKE_SANDBOX = _cfg.SandboxConfig(
    mode="none", share_net=False, bwrap_path="bwrap", systemd_run_path="systemd-run",
    model_proxy=_cfg.ModelProxyConfig(enabled=False, upstream="http://127.0.0.1:1"),
)
_FAKE_RUNTIME = _cfg.RuntimeConfig(
    backend="omnigent", provider="ollama", model="deepseek-v4.1-flash:cloud",
    pi_path="/bin/true", pi_agent_dir=Path("/tmp"),
)


def test_relay_returns_the_assistant_answer(fake, tmp_path, loaded):
    binding = loaded["registry"].get("pilot-001")
    backend = _backend(fake, tmp_path)
    result = backend.run(
        binding, "hello",
        limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX, runtime=_FAKE_RUNTIME, msg_id="msg0001",
    )
    assert result.text == "answer from the runner"
    assert len(fake.posts) == 1
    state = RelayStore(tmp_path / "relay.json").get(binding.session_id)
    assert state.pending is None
    assert state.cursor is not None
    assert state.replies["msg0001"]["text"] == "answer from the runner"


def test_duplicate_delivery_is_answered_from_cache(fake, tmp_path, loaded):
    binding = loaded["registry"].get("pilot-001")
    backend = _backend(fake, tmp_path)
    first = backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                        runtime=_FAKE_RUNTIME, msg_id="msg0001")
    second = backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                         runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert first.text == second.text
    assert len(fake.posts) == 1, "a redelivered message must not run the turn twice"


def test_restart_reuses_cursor_and_cache(fake, tmp_path, loaded):
    binding = loaded["registry"].get("pilot-001")
    first_backend = _backend(fake, tmp_path)
    first_backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                      runtime=_FAKE_RUNTIME, msg_id="msg0001")

    # new process: fresh objects, same persistent state
    restarted = _backend(fake, tmp_path)
    cached = restarted.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                           runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert cached.text == "answer from the runner"
    assert len(fake.posts) == 1

    # a *new* message after restart still works and does not replay old items
    fake.reply_text = "second answer"
    fresh = restarted.run(binding, "again", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                          runtime=_FAKE_RUNTIME, msg_id="msg0002")
    assert fresh.text == "second answer"
    assert len(fake.posts) == 2


def test_crash_before_answer_resumes_without_reposting(fake, tmp_path, loaded):
    binding = loaded["registry"].get("pilot-001")
    store_path = tmp_path / "relay.json"
    store = RelayStore(store_path)
    state = store.get(binding.session_id)
    state.pending = {"msg_id": "msg0001", "posted_at": 1.0}
    store.update(binding.session_id, state)

    # the runner already answered; the broker crashed before reading it
    fake.items = [
        {"id": "item1", "role": "user", "text": "hello"},
        {"id": "item2", "role": "assistant", "text": "late answer"},
    ]
    backend = _backend(fake, tmp_path)
    result = backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                         runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert result.text == "late answer"
    assert fake.posts == [], "a resumed turn must not be posted again"


def test_failed_post_clears_pending_so_a_retry_can_run(fake, tmp_path, loaded):
    binding = loaded["registry"].get("pilot-001")
    fake.status_override = 500
    backend = _backend(fake, tmp_path)
    with pytest.raises(OmnigentAPIError):
        backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="msg0001")
    state = RelayStore(tmp_path / "relay.json").get(binding.session_id)
    assert state.pending is None

    fake.status_override = None
    result = backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                         runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert result.text == "answer from the runner"
    assert len(fake.posts) == 1


def test_turn_timeout_is_bounded(fake, tmp_path, loaded):
    binding = loaded["registry"].get("pilot-001")
    fake.auto_reply = False
    backend = OmnigentBackend(
        make_api(fake), RelayStore(tmp_path / "relay.json"),
        poll_interval=0.01, turn_timeout=0.2,
    )
    with pytest.raises(LimitExceededError):
        backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="msg0001")


def test_relay_state_file_is_private(fake, tmp_path, loaded):
    binding = loaded["registry"].get("pilot-001")
    backend = _backend(fake, tmp_path)
    backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                runtime=_FAKE_RUNTIME, msg_id="msg0001")
    mode = (tmp_path / "relay.json").stat().st_mode & 0o777
    assert mode == 0o600


# ------------------------------------------------------------- backend wiring
def test_build_backend_omnigent_uses_the_registry_session(loaded, monkeypatch, fake, tmp_path):
    cfg = loaded["cfg"]
    runtime = _cfg.RuntimeConfig(
        backend="omnigent", provider="ollama", model="deepseek-v4.1-flash:cloud",
        pi_path="/bin/true", pi_agent_dir=tmp_path,
        omnigent_base_url=fake.base_url, omnigent_token_env="KURISU_TEST_TOKEN",
        relay_state_path=tmp_path / "relay.json",
    )
    monkeypatch.setenv("KURISU_TEST_TOKEN", TOKEN)
    backend = build_backend(runtime, loaded["registry"])
    assert isinstance(backend, OmnigentBackend)
    assert backend.api.session_id == "conv_0123456789abcdef"


def test_build_backend_requires_the_operator_token(loaded, tmp_path, monkeypatch):
    runtime = _cfg.RuntimeConfig(
        backend="omnigent", provider="ollama", model="m", pi_path="/bin/true",
        pi_agent_dir=tmp_path, omnigent_base_url="http://127.0.0.1:1",
        omnigent_token_env="KURISU_ABSENT_TOKEN", relay_state_path=tmp_path / "relay.json",
    )
    monkeypatch.delenv("KURISU_ABSENT_TOKEN", raising=False)
    with pytest.raises(SessionBackendError, match="token is not set"):
        build_backend(runtime, loaded["registry"])


def test_local_pi_requires_explicit_dev_optin(loaded, tmp_path):
    runtime = _cfg.RuntimeConfig(
        backend="pi-cli", provider="ollama", model="m", pi_path="/bin/true",
        pi_agent_dir=tmp_path,
    )
    with pytest.raises(SessionBackendError, match="development-only"):
        build_backend(runtime, loaded["registry"])
