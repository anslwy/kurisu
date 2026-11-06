"""Kurisu sign-in: Google OAuth device-code flow + verified identity binding.

Why the device flow: the pilot must not expose a public callback URL. Google's
OAuth 2.0 "TVs and Limited Input devices" flow lets the guest visit a Google URL
on any device and type a short code, while the broker polls the token endpoint.

Security shape:

* **default deny** - until a WhatsApp identity is verified, the only accepted
  command is ``/signin``. Everything else is refused before any guest work.
* **one-to-one binding** - one verified Google ``sub`` maps to exactly one
  WhatsApp identity and to exactly one ``user_id`` (``pilot-001``). A second
  identity cannot claim the same Google account and vice versa.
* **short-lived pending state** - each device code expires, is single-use, and is
  polled at the interval Google asks for (``slow_down`` respected).
* **tokens stay with the broker** - access/refresh tokens are written to a
  broker-only file (0600) and are never placed in a guest prompt, a guest
  session, the WhatsApp wire protocol, or the audit log.

All network calls go through an injectable transport so the whole state machine
is unit-testable without Google credentials.
"""

from __future__ import annotations

import json
import os
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Mapping

GOOGLE_DEVICE_ENDPOINT = "https://oauth2.googleapis.com/device/code"
GOOGLE_TOKEN_ENDPOINT = "https://oauth2.googleapis.com/token"
GOOGLE_USERINFO_ENDPOINT = "https://openidconnect.googleapis.com/v1/userinfo"
DEVICE_SCOPE = "openid email profile"

#: transport(url, form_fields) -> (http_status, decoded_json)
Transport = Callable[[str, Mapping[str, str]], "tuple[int, dict[str, Any]]"]

SIGNIN_COMMAND = "/signin"

# Poll statuses.
PENDING = "pending"
SLOW_DOWN = "slow_down"
AUTHORIZED = "authorized"
DENIED = "denied"
EXPIRED = "expired"
ERROR = "error"


class AuthError(Exception):
    """Sign-in could not proceed."""


def default_transport(url: str, fields: Mapping[str, str]) -> tuple[int, dict[str, Any]]:
    data = urllib.parse.urlencode(dict(fields)).encode("utf-8")
    request = urllib.request.Request(
        url,
        data=data,
        headers={"Content-Type": "application/x-www-form-urlencoded", "Accept": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return response.status, json.loads(response.read().decode("utf-8") or "{}")
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8", "replace")
        try:
            return exc.code, json.loads(body or "{}")
        except json.JSONDecodeError:
            return exc.code, {"error": "http_error", "error_description": body[:200]}


def userinfo_transport(access_token: str) -> dict[str, Any]:
    request = urllib.request.Request(
        GOOGLE_USERINFO_ENDPOINT,
        headers={"Authorization": f"Bearer {access_token}", "Accept": "application/json"},
        method="GET",
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return json.loads(response.read().decode("utf-8") or "{}")


@dataclass(frozen=True)
class DeviceCode:
    device_code: str
    user_code: str
    verification_url: str
    expires_in: int
    interval: int


@dataclass
class Pending:
    wa_jid: str
    user_id: str
    device_code: str
    user_code: str
    verification_url: str
    expires_at: float
    interval: int
    next_poll_at: float


@dataclass(frozen=True)
class Binding:
    wa_jid: str
    user_id: str
    sub: str
    email: str
    verified_at: float


class DeviceFlow:
    """Google device-code client."""

    def __init__(
        self,
        client_id: str,
        client_secret: str | None = None,
        *,
        transport: Transport = default_transport,
        userinfo: Callable[[str], dict[str, Any]] = userinfo_transport,
        clock: Callable[[], float] = time.time,
    ) -> None:
        if not client_id:
            raise AuthError("google client_id is required for sign-in")
        self.client_id = client_id
        self.client_secret = client_secret or ""
        self._transport = transport
        self._userinfo = userinfo
        self._clock = clock

    def start(self) -> DeviceCode:
        status, body = self._transport(
            GOOGLE_DEVICE_ENDPOINT,
            {"client_id": self.client_id, "scope": DEVICE_SCOPE},
        )
        if status != 200 or "device_code" not in body:
            raise AuthError(f"device authorization failed ({status}): {body.get('error', body)}")
        return DeviceCode(
            device_code=str(body["device_code"]),
            user_code=str(body["user_code"]),
            verification_url=str(body.get("verification_url") or body.get("verification_uri") or ""),
            expires_in=int(body.get("expires_in", 1800)),
            interval=max(5, int(body.get("interval", 5))),
        )

    def poll(self, device_code: str) -> tuple[str, dict[str, Any]]:
        fields = {
            "client_id": self.client_id,
            "device_code": device_code,
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
        }
        if self.client_secret:
            fields["client_secret"] = self.client_secret
        status, body = self._transport(GOOGLE_TOKEN_ENDPOINT, fields)
        if status == 200 and body.get("access_token"):
            return AUTHORIZED, body
        error = str(body.get("error", ""))
        if error == "authorization_pending":
            return PENDING, body
        if error == "slow_down":
            return SLOW_DOWN, body
        if error in ("access_denied", "user_cancelled"):
            return DENIED, body
        if error == "expired_token":
            return EXPIRED, body
        return ERROR, body

    def fetch_identity(self, access_token: str) -> dict[str, Any]:
        return self._userinfo(access_token)


class SigninStore:
    """Verified bindings + broker-only Google tokens (both persisted 0600)."""

    def __init__(self, mappings_path: Path, tokens_path: Path) -> None:
        self.mappings_path = Path(mappings_path)
        self.tokens_path = Path(tokens_path)
        self._bindings: dict[str, Binding] = {}
        self._by_sub: dict[str, str] = {}
        # The broker server is threaded and the per-user lock is keyed by user_id,
        # so two JIDs verifying the same Google `sub` concurrently would otherwise
        # both pass the "already linked" check. All mutations go through this lock.
        self._lock = threading.RLock()
        self._load()

    # -- persistence -----------------------------------------------------
    def _load(self) -> None:
        if not self.mappings_path.is_file():
            return
        raw = json.loads(self.mappings_path.read_text(encoding="utf-8") or "{}")
        for entry in raw.get("bindings", []):
            binding = Binding(
                wa_jid=entry["wa_jid"],
                user_id=entry["user_id"],
                sub=entry["sub"],
                email=entry.get("email", ""),
                verified_at=float(entry.get("verified_at", 0)),
            )
            self._bindings[binding.wa_jid] = binding
            self._by_sub[binding.sub] = binding.wa_jid

    def _save(self) -> None:
        self.mappings_path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "version": 1,
            "bindings": [
                {
                    "wa_jid": b.wa_jid,
                    "user_id": b.user_id,
                    "sub": b.sub,
                    "email": b.email,
                    "verified_at": b.verified_at,
                }
                for b in sorted(self._bindings.values(), key=lambda b: b.wa_jid)
            ],
        }
        _write_private(self.mappings_path, json.dumps(payload, indent=2, sort_keys=True))

    def _save_tokens(self, wa_jid: str, token_body: Mapping[str, Any]) -> None:
        existing: dict[str, Any] = {"version": 1, "tokens": {}}
        if self.tokens_path.is_file():
            try:
                existing = json.loads(self.tokens_path.read_text(encoding="utf-8") or "{}")
            except json.JSONDecodeError:
                existing = {"version": 1, "tokens": {}}
        tokens = existing.setdefault("tokens", {})
        tokens[wa_jid] = {
            "access_token": token_body.get("access_token", ""),
            "refresh_token": token_body.get("refresh_token", ""),
            "expires_at": time.time() + float(token_body.get("expires_in", 0) or 0),
            "scope": token_body.get("scope", ""),
            "token_type": token_body.get("token_type", "Bearer"),
        }
        _write_private(self.tokens_path, json.dumps(existing, indent=2, sort_keys=True))

    # -- queries ---------------------------------------------------------
    def binding_for(self, wa_jid: str) -> Binding | None:
        return self._bindings.get(wa_jid)

    def is_verified(self, wa_jid: str) -> bool:
        return wa_jid in self._bindings

    def owner_of_sub(self, sub: str) -> str | None:
        return self._by_sub.get(sub)

    def public_view(self) -> list[dict[str, Any]]:
        return [
            {"user_id": b.user_id, "wa_jid": _mask(b.wa_jid), "email": _mask_email(b.email), "sub_present": bool(b.sub)}
            for b in sorted(self._bindings.values(), key=lambda b: b.user_id)
        ]

    # -- mutation --------------------------------------------------------
    def bind(self, *, wa_jid: str, user_id: str, sub: str, email: str) -> Binding:
        with self._lock:
            return self._bind_locked(wa_jid=wa_jid, user_id=user_id, sub=sub, email=email)

    def _bind_locked(self, *, wa_jid: str, user_id: str, sub: str, email: str) -> Binding:
        if not sub:
            raise AuthError("Google identity did not include a subject (sub)")
        owner = self._by_sub.get(sub)
        if owner is not None and owner != wa_jid:
            raise AuthError("this Google account is already linked to a different WhatsApp identity")
        # One guest = one bound WhatsApp identity. A second test user would
        # otherwise share the single session and its history.
        for other_jid, other in self._bindings.items():
            if other_jid != wa_jid and other.user_id == user_id:
                raise AuthError(
                    "this guest is already bound to another WhatsApp identity; an operator "
                    "must clear that binding first"
                )
        existing = self._bindings.get(wa_jid)
        if existing is not None and existing.sub != sub:
            raise AuthError(
                "this WhatsApp identity is already verified with a different Google account; "
                "an operator must clear the binding"
            )
        binding = Binding(wa_jid=wa_jid, user_id=user_id, sub=sub, email=email, verified_at=time.time())
        self._bindings[wa_jid] = binding
        self._by_sub[sub] = wa_jid
        self._save()
        return binding

    def store_tokens(self, wa_jid: str, token_body: Mapping[str, Any]) -> None:
        with self._lock:
            self._save_tokens(wa_jid, token_body)

    def revoke(self, wa_jid: str) -> bool:
        with self._lock:
            binding = self._bindings.pop(wa_jid, None)
            if binding is None:
                return False
            self._by_sub.pop(binding.sub, None)
            self._save()
            return True


class SigninManager:
    """Owns pending device codes and turns them into verified bindings."""

    def __init__(
        self,
        flow: DeviceFlow,
        store: SigninStore,
        *,
        clock: Callable[[], float] = time.time,
        pending_ttl_seconds: int = 1800,
        max_pending: int = 32,
        cooldown_seconds: int = 30,
        max_starts_per_window: int = 5,
        window_seconds: int = 900,
    ) -> None:
        self.flow = flow
        self.store = store
        self._clock = clock
        self.pending_ttl = pending_ttl_seconds
        self.max_pending = max_pending
        self._pending: dict[str, Pending] = {}
        # Per-WhatsApp-identity throttle for /signin. Because any one-to-one
        # sender may start a sign-in, this is the only thing between a stranger
        # and a stream of device codes.
        self._starts: dict[str, list[float]] = {}
        # Open mode means any sender can hit start/poll concurrently; the broker's
        # per-user lock does not cover them (different JIDs, same guest).
        self._lock = threading.RLock()
        self.cooldown_seconds = max(0, cooldown_seconds)
        self.max_starts_per_window = max(1, max_starts_per_window)
        self.window_seconds = max(60, window_seconds)

    # -- queries ---------------------------------------------------------
    def is_verified(self, wa_jid: str) -> bool:
        return self.store.is_verified(wa_jid)

    def pending_for(self, wa_jid: str) -> Pending | None:
        with self._lock:
            return self._pending_for_locked(wa_jid)

    def _pending_for_locked(self, wa_jid: str) -> Pending | None:
        pending = self._pending.get(wa_jid)
        if pending is None:
            return None
        if self._clock() >= pending.expires_at:
            self._pending.pop(wa_jid, None)
            return None
        return pending

    def pending_count(self) -> int:
        with self._lock:
            self._expire_locked()
            return len(self._pending)

    def _expire(self) -> None:
        with self._lock:
            self._expire_locked()

    def _expire_locked(self) -> None:
        now = self._clock()
        for jid in [j for j, p in self._pending.items() if now >= p.expires_at]:
            self._pending.pop(jid, None)

    # -- commands --------------------------------------------------------
    def _check_signin_rate(self, wa_jid: str, now: float) -> None:
        starts = [stamp for stamp in self._starts.get(wa_jid, []) if now - stamp <= self.window_seconds]
        if starts and now - starts[-1] < self.cooldown_seconds:
            wait = int(self.cooldown_seconds - (now - starts[-1])) + 1
            raise AuthError(f"please wait {wait}s before requesting another sign-in code")
        if len(starts) >= self.max_starts_per_window:
            raise AuthError("too many sign-in attempts from this number; try again later")
        starts.append(now)
        self._starts[wa_jid] = starts[-self.max_starts_per_window:]

    def start(self, wa_jid: str, user_id: str) -> Pending:
        with self._lock:
            return self._start_locked(wa_jid, user_id)

    def _start_locked(self, wa_jid: str, user_id: str) -> Pending:
        if self.store.is_verified(wa_jid):
            raise AuthError("this WhatsApp identity is already signed in")
        self._expire_locked()
        # Atomic start-or-return-existing: two threads that both saw "no pending"
        # outside the lock must still create exactly one device flow, and must not
        # consume the per-identity rate budget twice.
        existing = self._pending.get(wa_jid)
        if existing is not None:
            return existing
        self._check_signin_rate(wa_jid, self._clock())
        if len(self._pending) >= self.max_pending:
            raise AuthError("too many sign-in attempts in progress; try again later")
        code = self.flow.start()
        now = self._clock()
        pending = Pending(
            wa_jid=wa_jid,
            user_id=user_id,
            device_code=code.device_code,
            user_code=code.user_code,
            verification_url=code.verification_url,
            expires_at=now + min(code.expires_in, self.pending_ttl),
            interval=code.interval,
            # Google asks clients to wait one interval before the first poll.
            next_poll_at=now + code.interval,
        )
        self._pending[wa_jid] = pending
        return pending

    def poll(self, wa_jid: str) -> tuple[str, str]:
        with self._lock:
            return self._poll_locked(wa_jid)

    def _poll_locked(self, wa_jid: str) -> tuple[str, str]:
        """Poll once for a pending sign-in.

        Returns ``(status, message)`` where status is one of the module-level
        poll statuses. Polling is lazy: it happens when the guest sends another
        message, so the broker needs no background thread and no push channel.
        """
        pending = self._pending_for_locked(wa_jid)
        if pending is None:
            return EXPIRED, "No sign-in in progress. Send /signin to start."

        now = self._clock()
        if now < pending.next_poll_at:
            return PENDING, self._instruction(pending, "Waiting for you to approve the code.")

        status, body = self.flow.poll(pending.device_code)
        if status == PENDING:
            pending.next_poll_at = now + pending.interval
            return PENDING, self._instruction(pending, "Waiting for you to approve the code.")
        if status == SLOW_DOWN:
            pending.interval = min(pending.interval + 5, 60)
            pending.next_poll_at = now + pending.interval
            return SLOW_DOWN, self._instruction(pending, "Google asked us to slow down; still waiting.")
        if status == DENIED:
            self._pending.pop(wa_jid, None)
            return DENIED, "Sign-in was denied. Send /signin to try again."
        if status in (EXPIRED, ERROR):
            self._pending.pop(wa_jid, None)
            if status == EXPIRED:
                return EXPIRED, "That code expired. Send /signin to get a new one."
            reason = str(body.get("error_description") or body.get("error") or "unknown error")
            return ERROR, f"Sign-in failed ({reason}). Send /signin to try again."

        # AUTHORIZED
        access_token = str(body.get("access_token", ""))
        try:
            identity = self.flow.fetch_identity(access_token)
        except Exception as exc:  # network/userinfo failure must not bind
            return ERROR, f"Signed in with Google but the identity check failed: {type(exc).__name__}"
        sub = str(identity.get("sub", ""))
        email = str(identity.get("email", ""))
        try:
            binding = self.store.bind(wa_jid=wa_jid, user_id=pending.user_id, sub=sub, email=email)
        except AuthError as exc:
            self._pending.pop(wa_jid, None)
            return DENIED, str(exc)
        # Tokens are broker-only and are never echoed back to WhatsApp.
        self.store.store_tokens(wa_jid, body)
        self._pending.pop(wa_jid, None)
        return AUTHORIZED, (
            f"Signed in as {_mask_email(binding.email)}. "
            f"You are verified as {binding.user_id}. You can now message the assistant."
        )

    def _instruction(self, pending: Pending, head: str) -> str:
        minutes = max(1, int((pending.expires_at - self._clock()) // 60))
        return (
            f"{head}\n"
            f"1) Open {pending.verification_url} on any device\n"
            f"2) Enter code: {pending.user_code}\n"
            f"The code expires in about {minutes} minute(s)."
        )


def _write_private(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as handle:
        handle.write(text)
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)
    os.chmod(path, 0o600)


def _mask(value: str) -> str:
    if not value:
        return ""
    head = value.split("@")[0]
    return (head[:3] + "***@" + value.split("@")[1]) if "@" in value else head[:3] + "***"


def _mask_email(value: str) -> str:
    if "@" not in value:
        return "***"
    local, _, domain = value.partition("@")
    return f"{local[:2]}***@{domain}"


class FakeDeviceFlow:
    """Dev-only flow used by `guest-broker dev --fake-signin`.

    It performs **no** Google communication and must never be used for a real
    pilot: it exists so the /signin gate can be demonstrated end-to-end without
    credentials. Every reply it produces says so explicitly.
    """

    def __init__(self, *, sub: str = "dev-sub-pilot-001", email: str = "pilot@example.test") -> None:
        self.sub = sub
        self.email = email
        self.polls = 0

    def start(self) -> DeviceCode:
        return DeviceCode(
            device_code="dev-device-code",
            user_code="DEV-CODE",
            verification_url="https://example.test/device (FAKE - no Google involved)",
            expires_in=600,
            interval=0,
        )

    def poll(self, device_code: str) -> tuple[str, dict[str, Any]]:
        self.polls += 1
        return AUTHORIZED, {"access_token": "dev-access-token", "expires_in": 3600, "scope": DEVICE_SCOPE}

    def fetch_identity(self, access_token: str) -> dict[str, Any]:
        return {"sub": self.sub, "email": self.email, "email_verified": True}
