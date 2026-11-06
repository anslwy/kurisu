"""Broker -> Omnigent session backend (the intended PoC path).

The broker posts the guest's message to the **one** session id from the
operator-owned registry and relays the assistant's answer back. Pi, Node, the
fixed DeepSeek model and the session workspace all live in the Omnigent runner on
the host side; the WhatsApp client and this broker never run a model.

Restart / dedup design (persisted, 0600, atomic replace):

* ``pending`` - written *before* posting: ``{msg_id, posted_at}``. After a crash
  the broker resumes polling instead of posting the same turn twice.
* ``cursor``  - last item id consumed, so a restart does not replay old answers.
* ``seen``    - bounded list of relayed item ids (page re-reads cannot duplicate).
* ``replies`` - bounded ``msg_id -> {text, at}`` cache, so a WhatsApp redelivery of
  the same message is answered from cache instead of running the turn again.
"""

from __future__ import annotations

import json
import os
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .backend import BackendResult
from .config import Limits, RuntimeConfig, SandboxConfig
from .errors import LimitExceededError, SessionBackendError
from .omnigent_api import OmnigentAPI, OmnigentError
from .registry import GuestBinding

MAX_SEEN = 500
MAX_CACHED_REPLIES = 200


@dataclass
class RelayState:
    """Per-session relay bookkeeping (persisted)."""

    cursor: str | None = None
    seen: list[str] = field(default_factory=list)
    pending: dict[str, Any] | None = None
    replies: dict[str, dict[str, Any]] = field(default_factory=dict)

    def trim(self) -> None:
        self.seen = self.seen[-MAX_SEEN:]
        if len(self.replies) > MAX_CACHED_REPLIES:
            for key in list(self.replies)[:-MAX_CACHED_REPLIES]:
                self.replies.pop(key, None)


class RelayStore:
    """Atomic, lock-protected JSON store for :class:`RelayState`."""

    def __init__(self, path: Path) -> None:
        self.path = Path(path)
        self._lock = threading.RLock()
        self._state: dict[str, RelayState] = {}
        self._load()

    def _load(self) -> None:
        if not self.path.is_file():
            return
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8") or "{}")
        except (OSError, json.JSONDecodeError) as exc:
            raise SessionBackendError(f"relay state {self.path} is unreadable: {exc}") from exc
        for session_id, entry in (raw.get("sessions") or {}).items():
            if not isinstance(entry, dict):
                continue
            self._state[session_id] = RelayState(
                cursor=entry.get("cursor"),
                seen=list(entry.get("seen") or [])[-MAX_SEEN:],
                pending=entry.get("pending") if isinstance(entry.get("pending"), dict) else None,
                replies={
                    str(key): value
                    for key, value in (entry.get("replies") or {}).items()
                    if isinstance(value, dict)
                },
            )

    def _save(self) -> None:
        payload = {
            "version": 1,
            "sessions": {
                session_id: {
                    "cursor": state.cursor,
                    "seen": state.seen[-MAX_SEEN:],
                    "pending": state.pending,
                    "replies": dict(list(state.replies.items())[-MAX_CACHED_REPLIES:]),
                }
                for session_id, state in self._state.items()
            },
        }
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(self.path.suffix + ".tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(payload, handle, indent=2, sort_keys=True)
        os.chmod(tmp, 0o600)
        os.replace(tmp, self.path)
        os.chmod(self.path, 0o600)

    def get(self, session_id: str) -> RelayState:
        with self._lock:
            return self._state.setdefault(session_id, RelayState())

    def update(self, session_id: str, state: RelayState) -> None:
        state.trim()
        with self._lock:
            self._state[session_id] = state
            self._save()

    def cached_reply(self, session_id: str, msg_id: str) -> str | None:
        if not msg_id:
            return None
        with self._lock:
            entry = self.get(session_id).replies.get(msg_id)
        if isinstance(entry, dict) and isinstance(entry.get("text"), str):
            return entry["text"]
        return None


class OmnigentBackend:
    """Relays one guest message to the bound session and returns the answer."""

    name = "omnigent"

    def __init__(
        self,
        api: OmnigentAPI,
        store: RelayStore,
        *,
        poll_interval: float = 1.0,
        turn_timeout: float = 300.0,
        clock=time.time,
        sleep=time.sleep,
    ) -> None:
        self.api = api
        self.store = store
        self.poll_interval = max(0.05, poll_interval)
        self.turn_timeout = turn_timeout
        self._clock = clock
        self._sleep = sleep

    # -- SessionBackend --------------------------------------------------
    def run(
        self,
        binding: GuestBinding,
        message: str,
        *,
        limits: Limits,
        sandbox: SandboxConfig,
        runtime: RuntimeConfig,
        proxy_socket: Path | None = None,
        guest_io_dir: Path | None = None,
        msg_id: str = "",
    ) -> BackendResult:
        session_id = binding.session_id  # authoritative; never taken from the message
        started = self._clock()

        cached = self.store.cached_reply(session_id, msg_id)
        if cached is not None:
            return BackendResult(
                text=cached,
                exit_code=0,
                duration_seconds=0.0,
                sandboxed=True,
                detail="relayed from cache (duplicate delivery)",
            )

        state = self.store.get(session_id)
        resume = state.pending is not None
        if not resume:
            state.pending = {"msg_id": msg_id or f"relay-{int(started * 1000)}", "posted_at": started}
            self.store.update(session_id, state)
            try:
                self.api.post_message(session_id, message, msg_id=state.pending["msg_id"])
            except OmnigentError:
                state = self.store.get(session_id)
                state.pending = None
                self.store.update(session_id, state)
                raise

        text, item_id = self._wait_for_answer(session_id)

        state = self.store.get(session_id)
        pending = state.pending or {}
        reply_key = str(pending.get("msg_id") or msg_id or "last")
        state.pending = None
        state.replies[reply_key] = {"text": text, "at": self._clock()}
        if item_id and item_id not in state.seen:
            state.seen.append(item_id)
        self.store.update(session_id, state)

        return BackendResult(
            text=text,
            exit_code=0,
            duration_seconds=self._clock() - started,
            sandboxed=True,
            detail="relayed from Omnigent session",
        )

    # -- polling ---------------------------------------------------------
    def _wait_for_answer(self, session_id: str) -> tuple[str, str | None]:
        deadline = self._clock() + self.turn_timeout
        while self._clock() < deadline:
            state = self.store.get(session_id)
            page = self.api.list_items(session_id, after=state.cursor, limit=100, order="asc")
            for item in page.data:
                item_id = str(item.get("id") or "")
                if not item_id or item_id in state.seen:
                    continue
                text = self.api.assistant_text(item)
                if not text:
                    continue
                state.cursor = item_id
                self.store.update(session_id, state)
                return text, item_id
            if page.last_id and page.last_id != state.cursor:
                state.cursor = page.last_id
                self.store.update(session_id, state)
            if not page.has_more:
                self._sleep(self.poll_interval)
        raise LimitExceededError(
            f"no answer from the guest session within {int(self.turn_timeout)}s"
        )


def build_omnigent_backend(
    base_url: str,
    token: str,
    session_id: str,
    state_path: Path,
    *,
    poll_interval: float = 1.0,
    turn_timeout: float = 300.0,
) -> OmnigentBackend:
    api = OmnigentAPI(base_url, token, session_id=session_id)
    return OmnigentBackend(
        api, RelayStore(state_path), poll_interval=poll_interval, turn_timeout=turn_timeout
    )
