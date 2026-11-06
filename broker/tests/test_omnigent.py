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
from guest_broker.omnigent_backend import (
    OmnigentBackend,
    RelayStore,
    TurnOutcomeUnknown,
    TurnPendingError,
)

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
        self.post_status_override: int | None = None
        self.reflect_detail: str | None = None
        self.runner_online = True
        self.model = "deepseek-v4.1-flash:cloud"
        self.agent = "guest-whatsapp-pilot"
        self.post_ref_response: str | None = None
        self.emit_post_ref_item = True

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
                    detail = fake.reflect_detail or "injected failure"
                    self._send(fake.status_override, {"detail": detail})
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
                if fake.post_status_override:
                    self._send(fake.post_status_override, {"detail": "injected failure"})
                    return
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
                if fake.post_ref_response:
                    if fake.emit_post_ref_item:
                        fake.items.append({"id": fake.post_ref_response, "role": "tool", "text": ""})
                else:
                    fake.items.append({"id": f"item{len(fake.items) + 1}", "role": "user", "text": "posted"})
                if fake.auto_reply:
                    fake.items.append({
                        "id": f"item{len(fake.items) + 1}",
                        "role": "assistant",
                        "content": [{"type": "text", "text": fake.reply_text}],
                    })
                response: dict = {"queued": True}
                if fake.post_ref_response:
                    response["item_id"] = fake.post_ref_response
                self._send(200, response)

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
    state = RelayStore(tmp_path / "relay.json").snapshot(binding.session_id)
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
    state = store.snapshot(binding.session_id)
    state.pending = {"msg_id": "msg0001", "baseline_id": None, "post_ref": None, "posted_at": 1.0}
    store.commit(binding.session_id, state)

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


def test_ambiguous_post_keeps_pending_and_never_reposts(fake, tmp_path, loaded):
    """5xx/timeout may still have been accepted: keep the marker, do not re-post."""
    binding = loaded["registry"].get("pilot-001")
    fake.post_status_override = 500
    backend = _backend(fake, tmp_path)
    with pytest.raises(TurnOutcomeUnknown):
        backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="msg0001")
    state = RelayStore(tmp_path / "relay.json").snapshot(binding.session_id)
    assert state.pending is not None, "an ambiguous post must stay unresolved"

    # a retry of the same message reconciles instead of posting again
    fake.post_status_override = None
    fake.items = [
        {"id": "item1", "role": "user", "text": "hello"},
        {"id": "item2", "role": "assistant", "text": "reconciled answer"},
    ]
    result = backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                         runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert result.text == "reconciled answer"
    assert fake.posts == [], "must not re-post an ambiguous turn"


def test_definitive_rejection_clears_pending_so_a_retry_can_post(fake, tmp_path, loaded):
    binding = loaded["registry"].get("pilot-001")
    fake.post_status_override = 400
    backend = _backend(fake, tmp_path)
    with pytest.raises(OmnigentAPIError):
        backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert RelayStore(tmp_path / "relay.json").snapshot(binding.session_id).pending is None

    fake.post_status_override = None
    result = backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                         runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert result.text == "answer from the runner"
    assert len(fake.posts) == 1


def test_stale_history_answers_are_not_relayed(fake, tmp_path, loaded):
    """An assistant item older than our own turn is history, not an answer."""
    binding = loaded["registry"].get("pilot-001")
    # history: an old assistant answer already exists before we post
    fake.items = [{"id": "item1", "role": "assistant", "text": "old answer"}]
    fake.auto_reply = False
    backend = _backend(fake, tmp_path)
    # make the fake append only our user item (no answer yet)
    with pytest.raises(LimitExceededError):
        backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert fake.posts, "the turn was posted"
    # it must not have relayed the stale answer
    state = RelayStore(tmp_path / "relay.json").snapshot(binding.session_id)
    assert "msg0001" not in state.replies


def test_different_message_is_refused_while_a_turn_is_pending(fake, tmp_path, loaded):
    binding = loaded["registry"].get("pilot-001")
    store = RelayStore(tmp_path / "relay.json")
    state = store.snapshot(binding.session_id)
    state.pending = {"msg_id": "msg0001", "baseline_id": "item0", "post_ref": None, "posted_at": 1.0}
    store.commit(binding.session_id, state)

    backend = _backend(fake, tmp_path)
    with pytest.raises(TurnPendingError):
        backend.run(binding, "different message", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="msg0002")
    assert fake.posts == [], "a pending turn must not be reused for another message"


def test_post_ref_correlates_even_without_a_user_item(fake, tmp_path, loaded):
    """When the POST returns an item id, correlation uses it directly."""
    binding = loaded["registry"].get("pilot-001")
    fake.post_ref_response = "ref-item"
    fake.reply_text = "correlated"
    fake.items = []
    fake.auto_reply = True
    # opt-in: the operator confirmed the host's correlation contract
    backend = OmnigentBackend(
        make_api(fake), RelayStore(tmp_path / "relay.json"),
        poll_interval=0.01, turn_timeout=2.0, trust_post_event_id=True,
    )
    result = backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                         runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert result.text == "correlated"


def test_crash_after_poll_before_cache_does_not_lose_the_answer(fake, tmp_path, loaded):
    """Polling must not persist the cursor; only the atomic commit does."""
    binding = loaded["registry"].get("pilot-001")
    store_path = tmp_path / "relay.json"
    # pending turn whose answer is already in the session
    fake.items = [
        {"id": "item1", "role": "user", "text": "hello"},
        {"id": "item2", "role": "assistant", "text": "the answer"},
    ]
    store = RelayStore(store_path)
    state = store.snapshot(binding.session_id)
    state.pending = {"msg_id": "msg0001", "baseline_id": None, "post_ref": None, "posted_at": 1.0}
    store.commit(binding.session_id, state)

    backend = _backend(fake, tmp_path)
    result = backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                         runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert result.text == "the answer"
    state = RelayStore(store_path).snapshot(binding.session_id)
    assert state.cursor == "item2"
    assert state.replies["msg0001"]["text"] == "the answer"
    assert state.pending is None


def test_relayed_result_does_not_claim_verified_isolation(fake, tmp_path, loaded):
    binding = loaded["registry"].get("pilot-001")
    backend = _backend(fake, tmp_path)
    result = backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                         runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert result.sandboxed is False
    assert "operator-attested" in result.detail


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


def test_post_ref_not_observed_stays_uncertain(fake, tmp_path, loaded):
    """A promised correlation id that never appears must not fall back to history."""
    binding = loaded["registry"].get("pilot-001")
    fake.post_ref_response = "ref-missing"
    fake.emit_post_ref_item = False  # the promised id never shows up
    fake.reply_text = "unrelated history"
    fake.items = []
    fake.auto_reply = True
    backend = OmnigentBackend(
        make_api(fake), RelayStore(tmp_path / "relay.json"),
        poll_interval=0.01, turn_timeout=0.3, trust_post_event_id=True,
    )
    with pytest.raises(TurnOutcomeUnknown):
        backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="msg0001")
    state = RelayStore(tmp_path / "relay.json").snapshot(binding.session_id)
    assert state.pending is not None
    assert "msg0001" not in state.replies


def test_second_broker_process_cannot_start_a_turn(fake, tmp_path, loaded):
    """Whole-turn cross-process lock: no double-post from two broker processes."""
    binding = loaded["registry"].get("pilot-001")
    first = _backend(fake, tmp_path)
    second = _backend(fake, tmp_path)

    with first.store.turn_lock(binding.session_id):
        with pytest.raises(TurnPendingError, match="another broker process"):
            second.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                       runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert fake.posts == [], "the blocked process must not post"


def test_relay_lock_file_is_private(fake, tmp_path, loaded):
    binding = loaded["registry"].get("pilot-001")
    backend = _backend(fake, tmp_path)
    backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                runtime=_FAKE_RUNTIME, msg_id="msg0001")
    lock = tmp_path / "relay.json.lock"
    assert lock.is_file()
    assert lock.stat().st_mode & 0o777 == 0o600


# ------------------------------------------------- security regressions
def test_base_url_is_parsed_strictly():
    for bad in (
        "http://127.0.0.1.evil.example:6767",
        "http://user:pass@127.0.0.1:6767",
        "http://192.168.1.10:6767",
        "ftp://127.0.0.1:6767",
        "http://:6767",
    ):
        with pytest.raises(OmnigentError):
            OmnigentAPI(bad, TOKEN)
    # loopback http and remote https are fine
    OmnigentAPI("http://127.0.0.1:6767", TOKEN)
    OmnigentAPI("https://omnigent.example.test", TOKEN)


def test_reflected_token_in_error_body_is_not_surfaced(fake):
    fake.status_override = 500
    fake.reflect_detail = f"upstream said Authorization: Bearer {TOKEN} and text hello"
    api = make_api(fake)
    with pytest.raises(OmnigentAPIError) as excinfo:
        api.list_items(SESSION_ID)
    assert TOKEN not in str(excinfo.value)
    assert "hello" not in str(excinfo.value)


def test_relay_state_path_must_be_explicit_and_resolved(layout, mappings):
    # omnigent backend requires a non-empty relay path...
    path = make_config(layout, mappings)
    text = (
        path.read_text(encoding="utf-8")
        .replace("backend: echo", "backend: omnigent")
        .replace('base_url: ""', "base_url: http://127.0.0.1:6767")
        .replace("    token_env: OMNIGENT_OPERATOR_TOKEN", "    token_env: OMNIGENT_OPERATOR_TOKEN\n    relay_state_path: ''")
    )
    path.write_text(text, encoding="utf-8")
    with pytest.raises(config_mod.ConfigError, match="non-empty"):
        config_mod.load_config(path)

    # ...and when present it is resolved against the config directory, not the cwd
    path.write_text(
        text.replace("relay_state_path: ''", "relay_state_path: ../runtime/state/relay.json"),
        encoding="utf-8",
    )
    cfg = config_mod.load_config(path)
    assert cfg.runtime.relay_state_path.is_absolute()
    assert cfg.runtime.relay_state_path == (path.parent / "../runtime/state/relay.json").resolve()


def test_operator_token_cannot_be_injected_into_the_guest_env(layout, mappings):
    path = make_config(layout, mappings)
    text = path.read_text(encoding="utf-8").replace(
        "  extra_env: {}", "  extra_env:\n    OMNIGENT_OPERATOR_TOKEN: leaked"
    )
    path.write_text(text, encoding="utf-8")
    with pytest.raises(config_mod.ConfigError, match="must never be injected"):
        config_mod.load_config(path)


def test_guest_plan_never_contains_the_operator_token(loaded, tmp_path):
    """The relayed turn must not put the admin token (or its name) in the guest env."""
    from guest_broker import backend as backend_mod

    cfg = loaded["cfg"]
    env = backend_mod.PiCliBackend().build_env(cfg.runtime, tmp_path)
    assert "OMNIGENT_OPERATOR_TOKEN" not in env
    assert not any("TOKEN" in key for key in env)
    assert "127.0.0.1:6767" not in " ".join(env.values())


@pytest.mark.parametrize("status", [400, 401, 403, 404])
def test_definitive_statuses_clear_pending(fake, tmp_path, loaded, status):
    binding = loaded["registry"].get("pilot-001")
    fake.post_status_override = status
    backend = _backend(fake, tmp_path)
    expected = OmnigentAuthError if status in (401, 403) else (
        OmnigentSessionNotFound if status == 404 else OmnigentAPIError
    )
    with pytest.raises(expected):
        backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert RelayStore(tmp_path / "relay.json").snapshot(binding.session_id).pending is None


@pytest.mark.parametrize("status", [409, 429, 500, 503])
def test_other_statuses_keep_pending_unknown(fake, tmp_path, loaded, status):
    binding = loaded["registry"].get("pilot-001")
    fake.post_status_override = status
    backend = _backend(fake, tmp_path)
    with pytest.raises(TurnOutcomeUnknown):
        backend.run(binding, "hello", limits=_FAKE_LIMITS, sandbox=_FAKE_SANDBOX,
                    runtime=_FAKE_RUNTIME, msg_id="msg0001")
    assert RelayStore(tmp_path / "relay.json").snapshot(binding.session_id).pending is not None


def test_sub_sessions_are_disabled_and_fail_closed(layout, mappings):
    path = make_config(layout, mappings)
    text = (
        path.read_text(encoding="utf-8")
        .replace("backend: echo", "backend: omnigent")
        .replace('base_url: ""', "base_url: http://127.0.0.1:6767")
        .replace(
            "    token_env: OMNIGENT_OPERATOR_TOKEN",
            "    token_env: OMNIGENT_OPERATOR_TOKEN\n"
            "    relay_state_path: ../runtime/state/relay.json\n"
            "    allow_sub_sessions: true",
        )
    )
    path.write_text(text, encoding="utf-8")
    with pytest.raises(config_mod.ConfigError, match="allow_sub_sessions is not supported"):
        config_mod.load_config(path)
