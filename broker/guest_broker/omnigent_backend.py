"""Broker -> Omnigent session backend (the intended PoC path).

The broker posts the guest's message to the **one** session id from the
operator-owned registry and relays the assistant's answer back. Pi, Node, the
fixed DeepSeek model and the session workspace all live in the Omnigent runner on
the host side.

CORRELATION CONTRACT
====================
The host API does not return the created item id from ``POST /events`` today, and
there is no idempotency key, so exactly-once is not achievable. The contract is:

    at-most-once POST  +  correlated answer selection  +  explicit uncertainty

1. baseline: read the newest item id *before* posting and persist it in ``pending``.
2. post: attempt the turn. If the response carries an item/event/message id, store
   it as ``post_ref`` for strict correlation.
3. select: only assistant items strictly newer than the baseline are considered,
   and only after a *new user item* (our turn) has been observed - or after
   ``post_ref`` is seen. Assistant items newer than the baseline but older than our
   user turn are ignored (they belong to earlier history or another writer).
4. commit: cursor + seen + reply cache + clear-pending are written in **one**
   atomic store update, only after the answer was selected. A crash before that
   write leaves ``pending`` intact, so a restart re-selects the same item instead
   of losing it.

UNCERTAINTY
===========
* definitive rejection (400/401/403/404): the turn was not created -> clear
  pending and raise;
* unknown outcome (timeout, connection error, 5xx, 408/429): keep ``pending`` and
  raise :class:`TurnOutcomeUnknown`. The broker tells the guest the status is
  unknown. The turn is **never** re-posted automatically; only an explicit
  operator action clears the marker.
* a different inbound msg id while ``pending`` exists is refused
  (:class:`TurnPendingError`): the pending turn belongs to the earlier message and
  its answer must never be delivered to a different WhatsApp message.
"""

from __future__ import annotations

import fcntl
import json
import os
import threading
import time
from contextlib import contextmanager
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from .backend import BackendResult
from .config import Limits, RuntimeConfig, SandboxConfig
from .errors import LimitExceededError, SessionBackendError
from .omnigent_api import OmnigentAPI, OmnigentAPIError, OmnigentError
from .registry import GuestBinding

MAX_SEEN = 500
MAX_CACHED_REPLIES = 200
USER_ROLES = {"user", "human"}
ASSISTANT_ROLES = {"assistant", "agent", "model"}


class TurnPendingError(SessionBackendError):
    """Another message's turn is unresolved; refuse rather than reuse its answer."""


class TurnOutcomeUnknown(SessionBackendError):
    """The POST may or may not have been accepted; reconcile, never re-post."""


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

    def reload(self) -> None:
        """Re-read the file (used after acquiring the cross-process turn lock)."""
        with self._lock:
            self._state = {}
            self._load()

    @contextmanager
    def turn_lock(self, session_id: str):
        """Non-blocking exclusive lock for the whole turn.

        Two broker processes sharing relay.json would otherwise read the same
        cursor and post the same turn twice. The lock file is separate so the state
        file itself stays a pure atomic replace target.
        """
        lock_path = self.path.with_suffix(self.path.suffix + ".lock")
        lock_path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(lock_path, os.O_RDWR | os.O_CREAT, 0o600)
        try:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except OSError as exc:
                raise TurnPendingError(
                    "another broker process is handling a turn for this session"
                ) from exc
            self.reload()
            try:
                yield
            finally:
                self.commit_unlocked()
        finally:
            try:
                fcntl.flock(fd, fcntl.LOCK_UN)
            except OSError:
                pass
            os.close(fd)

    def commit_unlocked(self) -> None:
        """Persist current state (used on lock release)."""
        with self._lock:
            self._save()

    def snapshot(self, session_id: str) -> RelayState:
        """A detached copy: callers must not mutate shared state in place."""
        with self._lock:
            state = self._state.setdefault(session_id, RelayState())
            return RelayState(
                cursor=state.cursor,
                seen=list(state.seen),
                pending=dict(state.pending) if state.pending else None,
                replies=dict(state.replies),
            )

    def commit(self, session_id: str, state: RelayState) -> None:
        state.trim()
        with self._lock:
            self._state[session_id] = state
            self._save()

    def cached_reply(self, session_id: str, msg_id: str) -> str | None:
        if not msg_id:
            return None
        with self._lock:
            entry = self._state.get(session_id, RelayState()).replies.get(msg_id)
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
        trust_post_event_id: bool = False,
        clock=time.time,
        sleep=time.sleep,
    ) -> None:
        self.api = api
        self.store = store
        self.poll_interval = max(0.05, poll_interval)
        self.turn_timeout = turn_timeout
        # Only trust a correlation id from the POST response if the host contract
        # documents that it shares the /items id namespace. It is off by default:
        # the mandatory path is baseline + "new user item observed" gating.
        self.trust_post_event_id = trust_post_event_id
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
        with self.store.turn_lock(session_id):
            return self._run_locked(session_id, message, msg_id, started)

    def _run_locked(self, session_id: str, message: str, msg_id: str, started: float) -> BackendResult:
        cached = self.store.cached_reply(session_id, msg_id)
        if cached is not None:
            return self._result(cached, 0.0, "relayed from cache (duplicate delivery)")

        state = self.store.snapshot(session_id)
        pending = state.pending

        if pending is not None:
            pending_msg = str(pending.get("msg_id") or "")
            if pending_msg and msg_id and pending_msg != msg_id:
                # The earlier turn is unresolved; its answer must not be handed to
                # a different WhatsApp message.
                raise TurnPendingError(
                    "the previous turn is unresolved; refusing to answer a different message"
                )
            text, ref_item = self._wait_for_answer(session_id, pending)
            self._commit_answer(session_id, msg_id or pending_msg, text, ref_item)
            return self._result(text, self._clock() - started, "relayed after resuming an interrupted turn")

        baseline = self._baseline_item(session_id)
        pending = {
            "msg_id": msg_id or f"relay-{int(started * 1000)}",
            "baseline_id": baseline,
            "post_ref": None,
            "posted_at": started,
        }
        state.pending = pending
        self.store.commit(session_id, state)  # marker BEFORE the POST

        try:
            response = self.api.post_message(session_id, message, msg_id=pending["msg_id"])
        except OmnigentError as exc:
            # The error hierarchy decides: only 400/401/403/404 are definitive.
            unknown = exc.outcome_unknown
            if not unknown:
                # Definitive rejection: no turn exists, so a retry may post.
                fresh = self.store.snapshot(session_id)
                fresh.pending = None
                self.store.commit(session_id, fresh)
                raise
            # Ambiguous: keep the marker; reconcile later, never re-post blindly.
            raise TurnOutcomeUnknown(
                "the turn may or may not have been accepted; not re-posting automatically"
            ) from exc

        post_ref = self._post_ref(response)
        if post_ref:
            fresh = self.store.snapshot(session_id)
            if fresh.pending:
                fresh.pending["post_ref"] = post_ref
                self.store.commit(session_id, fresh)
            pending["post_ref"] = post_ref

        text, ref_item = self._wait_for_answer(session_id, pending)
        self._commit_answer(session_id, pending["msg_id"], text, ref_item)
        return self._result(text, self._clock() - started, "relayed from the Omnigent session")

    # -- helpers ---------------------------------------------------------
    def _result(self, text: str, duration: float, detail: str) -> BackendResult:
        return BackendResult(
            text=text,
            exit_code=0,
            duration_seconds=duration,
            # The broker cannot verify the runner's isolation: it is operator-attested.
            sandboxed=False,
            detail=f"{detail}; runner isolation operator-attested (unverified)",
            isolation="operator-attested",
        )

    def _baseline_item(self, session_id: str) -> str | None:
        page = self.api.list_items(session_id, limit=1, order="desc")
        if page.data:
            return str(page.data[0].get("id") or "") or page.last_id
        return page.last_id

    def _post_ref(self, response: Any) -> str | None:
        """Only used when the operator confirmed the host's correlation contract."""
        if not self.trust_post_event_id or not isinstance(response, dict):
            return None
        for key in ("item_id", "event_item_id"):
            value = response.get(key)
            if isinstance(value, str) and value:
                return value
        return None

    def _wait_for_answer(self, session_id: str, pending: dict[str, Any]) -> tuple[str, str | None]:
        """Select the answer for *this* turn. Writes nothing: commit is atomic."""
        deadline = self._clock() + self.turn_timeout
        baseline = pending.get("baseline_id")
        post_ref = pending.get("post_ref")
        saw_own_turn = False
        after = baseline

        while self._clock() < deadline:
            page = self.api.list_items(session_id, after=after, limit=100, order="asc")
            for item in page.data:
                item_id = str(item.get("id") or "")
                if not item_id:
                    continue
                after = item_id
                if post_ref and item_id == post_ref:
                    saw_own_turn = True
                    continue
                role = str(item.get("role") or "").lower()
                if role in USER_ROLES:
                    saw_own_turn = True
                    continue
                if role in ASSISTANT_ROLES or self.api.assistant_text(item):
                    # Only accept an answer once our own turn has been observed.
                    if not saw_own_turn:
                        continue
                    text = self.api.assistant_text(item)
                    if text:
                        return text, item_id
            if page.last_id and page.last_id != after:
                after = page.last_id
            if not page.has_more:
                self._sleep(self.poll_interval)
        if post_ref:
            # We were told which item correlates but never saw it: do not fall back
            # to an unrelated assistant item. Stay uncertain and keep pending.
            raise TurnOutcomeUnknown(
                "the posted item was never observed in the session; outcome unresolved"
            )
        raise LimitExceededError(
            f"no correlated answer from the guest session within {int(self.turn_timeout)}s"
        )

    def _commit_answer(self, session_id: str, msg_id: str, text: str, ref_item: str | None) -> None:
        """One atomic write: cursor + seen + reply cache + clear pending."""
        state = self.store.snapshot(session_id)
        if ref_item:
            state.cursor = ref_item
            if ref_item not in state.seen:
                state.seen.append(ref_item)
        state.replies[msg_id or "last"] = {"text": text, "at": self._clock()}
        state.pending = None
        self.store.commit(session_id, state)


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
