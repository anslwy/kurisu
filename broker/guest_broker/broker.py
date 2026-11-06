"""Broker core: authoritative identity resolution -> sandboxed guest reply.

Invariants enforced here (and pinned by tests):

* A message is attributed by *WhatsApp identity only*. The ``user_id`` comes from
  the allowlist, and the session id comes from the registry. Nothing the
  transport sends can influence either.
* Unknown or disabled identities are denied before any work happens.
* At most one guest run per user at a time; overflow is refused, not queued into
  unbounded memory.
* Per-user rate limits and a per-message character cap are enforced before the
  sandbox is started.
* Failures are reported to the guest as a short, non-leaky message; operator
  detail goes to the log and the audit file.
"""

from __future__ import annotations

import collections
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

from . import config as config_mod
from .backend import SessionBackend
from .errors import BrokerError, LimitExceededError, UnauthorizedError
from .protocol import Inbound
from .allowlist import normalize_jid
from .auth import SIGNIN_COMMAND, SigninManager
from .registry import GuestBinding, UserRegistry, append_audit

MAX_AUDIT_DETAIL = 200


@dataclass(frozen=True)
class Outcome:
    kind: str  # reply | denied | rate-limited | busy | failed
    text: str
    user_id: str | None = None
    session_id: str | None = None
    detail: str = ""

    @property
    def ok(self) -> bool:
        return self.kind == "reply"


class RateLimiter:
    def __init__(self, limit: int, window_seconds: int) -> None:
        self.limit = limit
        self.window = window_seconds
        self._events: dict[str, collections.deque[float]] = {}
        self._lock = threading.Lock()

    def check(self, key: str, now: float | None = None) -> bool:
        now = time.monotonic() if now is None else now
        with self._lock:
            bucket = self._events.setdefault(key, collections.deque())
            while bucket and now - bucket[0] > self.window:
                bucket.popleft()
            if len(bucket) >= self.limit:
                return False
            bucket.append(now)
            return True


class Broker:
    def __init__(
        self,
        *,
        settings: config_mod.BrokerSettings,
        limits: config_mod.Limits,
        sandbox: config_mod.SandboxConfig,
        runtime: config_mod.RuntimeConfig,
        registry: UserRegistry,
        allowlist,
        backend: SessionBackend,
        base_dir: Path,
        signin: SigninManager | None = None,
        audit: Callable[[dict], None] | None = None,
        log: Callable[[str], None] | None = None,
    ) -> None:
        self.settings = settings
        self.limits = limits
        self.sandbox = sandbox
        self.runtime = runtime
        self.registry = registry
        self.allowlist = allowlist
        self.backend = backend
        self.base_dir = base_dir
        self.signin = signin
        self._audit = audit or (lambda event: append_audit(self.settings.audit_path, event))
        self._log = log or (lambda msg: None)
        self._rate = RateLimiter(settings.per_user_rate, settings.per_user_rate_window)
        self._user_locks: dict[str, threading.Lock] = {}
        self._locks_guard = threading.Lock()
        self.proxy_socket: Path | None = None
        self.guest_io_dir: Path | None = None

    # -- startup validation ---------------------------------------------
    def validate_startup(self) -> list[str]:
        """Return a list of fatal configuration problems (empty means good).

        Cross-checks the allowlist against the registry so a message can never
        be accepted for a user that has no bound session.
        """
        problems: list[str] = []
        if len(self.registry) == 0:
            problems.append("registry has no users")
        if len(self.allowlist) == 0:
            problems.append("allowlist is empty: every message would be denied")
        for user_id in self.allowlist.user_ids:
            if user_id not in self.registry.user_ids:
                problems.append(f"allowlisted user {user_id!r} has no session in the registry")
        for user_id in self.registry.user_ids:
            if self.allowlist.jid_for_user(user_id) is None:
                problems.append(f"registry user {user_id!r} has no allowlisted WhatsApp identity")
            binding = self.registry.get(user_id)
            expected = (self.settings.guests_root / user_id / "workspace").resolve()
            if binding.workspace.resolve() != expected:
                problems.append(
                    f"user {user_id!r} workspace {binding.workspace} is not {expected}"
                )
        return problems

    # -- request handling ------------------------------------------------
    def _lock_for(self, user_id: str) -> threading.Lock:
        with self._locks_guard:
            return self._user_locks.setdefault(user_id, threading.Lock())

    def handle(self, inbound: Inbound) -> Outcome:
        started = time.monotonic()
        # Canonicalize the identity exactly once: allowlist lookups, sign-in
        # state and bindings must all key on the same string, otherwise case or
        # device-suffix variants could create duplicate bindings.
        try:
            wa_jid = normalize_jid(inbound.wa_jid)
        except UnauthorizedError as exc:
            self._audit_event("denied.identity", inbound=inbound, detail=str(exc))
            return Outcome(kind="denied", text="", detail=str(exc))
        inbound = Inbound(msg_id=inbound.msg_id, wa_jid=wa_jid, text=inbound.text, ts=inbound.ts)
        try:
            user_id = self.allowlist.resolve(wa_jid)
        except UnauthorizedError as exc:
            self._audit_event("denied.identity", inbound=inbound, detail=str(exc))
            self._log(f"denied inbound from non-allowlisted identity: {exc}")
            return Outcome(kind="denied", text="", detail=str(exc))

        try:
            binding = self.registry.get(user_id)
        except BrokerError as exc:
            self._audit_event("denied.session", inbound=inbound, user_id=user_id, detail=str(exc))
            return Outcome(kind="denied", text="", user_id=user_id, detail=str(exc))

        if binding.user_id != user_id:
            self._audit_event("denied.binding", inbound=inbound, user_id=user_id,
                              detail="registry binding does not match resolved user")
            return Outcome(kind="denied", text="", user_id=user_id, detail="binding mismatch")

        if len(inbound.text) > self.settings.max_message_chars:
            self._audit_event("denied.length", inbound=inbound, user_id=user_id, detail="message too long")
            return Outcome(
                kind="denied",
                text=f"That message is too long ({len(inbound.text)} characters). "
                     f"Please keep it under {self.settings.max_message_chars}.",
                user_id=user_id,
                session_id=binding.session_id,
                detail="message too long",
            )

        if not self._rate.check(user_id):
            self._audit_event("denied.rate", inbound=inbound, user_id=user_id, detail="rate limited")
            return Outcome(
                kind="rate-limited",
                text="You are sending messages too quickly. Please wait a moment and try again.",
                user_id=user_id,
                session_id=binding.session_id,
                detail="rate limited",
            )

        lock = self._lock_for(user_id)
        if not lock.acquire(blocking=False):
            self._audit_event("denied.busy", inbound=inbound, user_id=user_id, detail="busy")
            return Outcome(
                kind="busy",
                text="I am still working on your previous message. Please wait for the answer.",
                user_id=user_id,
                session_id=binding.session_id,
                detail="concurrent request refused",
            )
        try:
            gated = self._gate_signin(binding, inbound)
            if gated is not None:
                return gated
            return self._run_guest(binding, inbound, started)
        finally:
            lock.release()

    # -- Kurisu sign-in gate --------------------------------------------
    def _gate_signin(self, binding: GuestBinding, inbound: Inbound) -> Outcome | None:
        """Default-deny until the WhatsApp identity is verified by Google.

        Returns ``None`` to let the message through to the guest. Any other
        return value is the final answer for this message.
        """
        if self.signin is None:
            return None  # auth disabled (dev / echo mode); still sandboxed

        # A verified binding carries the user it was verified for. If the
        # allowlist is later repointed at a different user, the old Google
        # binding must not silently authorize the new one.
        auth_binding = self.signin.store.binding_for(inbound.wa_jid)
        if auth_binding is not None and auth_binding.user_id != binding.user_id:
            self._audit_event(
                "denied.auth_binding", inbound=inbound, user_id=binding.user_id,
                detail="verified Google binding belongs to a different user",
            )
            return Outcome(
                kind="denied",
                text="",
                user_id=binding.user_id,
                session_id=binding.session_id,
                detail="auth binding user mismatch",
            )

        text = inbound.text.strip()
        # The command is matched exactly (case-insensitive, no arguments) so a
        # guest prompt cannot smuggle extra instructions through it.
        if text.lower() == SIGNIN_COMMAND:
            return self._handle_signin(binding, inbound)

        if not self.signin.is_verified(inbound.wa_jid):
            self._audit_event("denied.unverified", inbound=inbound, user_id=binding.user_id,
                              detail="identity not verified by Google")
            return Outcome(
                kind="reply",
                text=(
                    "Kurisu: this WhatsApp account is not verified yet, so I cannot answer "
                    "anything else.\nSend /signin to sign in with Google."
                ),
                user_id=binding.user_id,
                session_id=binding.session_id,
                detail="sign-in required",
            )
        return None

    def _handle_signin(self, binding: GuestBinding, inbound: Inbound) -> Outcome:
        assert self.signin is not None
        wa_jid = inbound.wa_jid

        def reply(text: str, detail: str) -> Outcome:
            self._audit_event("signin", inbound=inbound, user_id=binding.user_id, detail=detail)
            return Outcome(
                kind="reply",
                text=text,
                user_id=binding.user_id,
                session_id=binding.session_id,
                detail=detail,
            )

        if self.signin.is_verified(wa_jid):
            existing = self.signin.store.binding_for(wa_jid)
            email = existing.email if existing else ""
            return reply(
                f"You are already signed in{' as ' + email if email else ''}. "
                "Send a message to talk to the assistant.",
                "already verified",
            )

        try:
            pending = self.signin.pending_for(wa_jid)
            if pending is None:
                pending = self.signin.start(wa_jid, binding.user_id)
                return reply(
                    "Sign in with Google to activate Kurisu:\n"
                    f"1) Open {pending.verification_url}\n"
                    f"2) Enter code: {pending.user_code}\n"
                    "Then send /signin again to confirm.",
                    "signin started",
                )
            status, message = self.signin.poll(wa_jid)
            return reply(message, f"signin poll: {status}")
        except BrokerError as exc:
            return reply(f"Sign-in is unavailable right now: {exc}", "signin error")
        except Exception as exc:  # never leak internals to WhatsApp
            self._log(f"signin failure for {binding.user_id}: {type(exc).__name__}: {exc}")
            return reply(
                "Sign-in could not be completed. The operator has been notified.",
                f"signin exception: {type(exc).__name__}",
            )

    def _run_guest(self, binding: GuestBinding, inbound: Inbound, started: float) -> Outcome:
        try:
            result = self.backend.run(
                binding,
                inbound.text,
                limits=self.limits,
                sandbox=self.sandbox,
                runtime=self.runtime,
                proxy_socket=self.proxy_socket,
                guest_io_dir=self.guest_io_dir,
            )
        except LimitExceededError as exc:
            self._audit_event("failed.limit", inbound=inbound, user_id=binding.user_id, detail=str(exc))
            return Outcome(
                kind="failed",
                text="That request hit the resource limit for this pilot account.",
                user_id=binding.user_id,
                session_id=binding.session_id,
                detail=str(exc),
            )
        except BrokerError as exc:
            self._audit_event("failed.backend", inbound=inbound, user_id=binding.user_id, detail=str(exc))
            self._log(f"backend error for {binding.user_id}: {exc}")
            return Outcome(
                kind="failed",
                text="The assistant is unavailable right now. The operator has been notified.",
                user_id=binding.user_id,
                session_id=binding.session_id,
                detail=str(exc)[:MAX_AUDIT_DETAIL],
            )

        text = result.text.strip() or (
            "I could not produce an answer for that message. Please rephrase it."
        )
        if len(text) > self.settings.max_reply_chars:
            text = text[: self.settings.max_reply_chars - 1].rstrip() + "…"

        duration = time.monotonic() - started
        self._audit_event(
            "reply",
            inbound=inbound,
            user_id=binding.user_id,
            detail=f"exit={result.exit_code} duration={duration:.1f}s sandboxed={result.sandboxed}",
        )
        return Outcome(
            kind="reply",
            text=text,
            user_id=binding.user_id,
            session_id=binding.session_id,
            detail=result.detail,
        )

    # -- audit -----------------------------------------------------------
    def _audit_event(
        self,
        event: str,
        *,
        inbound: Inbound | None = None,
        user_id: str | None = None,
        detail: str = "",
    ) -> None:
        """Audit records never contain message text or credentials."""
        record: dict[str, object] = {
            "event": event,
            "user_id": user_id,
            "detail": detail[:MAX_AUDIT_DETAIL],
        }
        if inbound is not None:
            record["msg_id"] = inbound.msg_id
            record["text_sha256_12"] = _short_hash(inbound.text)
            record["text_chars"] = len(inbound.text)
        try:
            self._audit(record)
        except OSError as exc:  # audit failure must not kill the loop
            self._log(f"audit write failed: {exc}")


def _short_hash(text: str) -> str:
    import hashlib

    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:12]
