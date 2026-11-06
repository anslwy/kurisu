"""WhatsApp identity allowlist.

Default-deny by construction:

* an empty or missing allowlist denies every inbound message;
* a JID is allowed only if it matches a listed identity exactly after
  normalisation (device suffix stripped, ASCII-lowercased);
* group, broadcast, newsletter and status chats are never allowed, whatever
  the file says, because the pilot is a one-to-one channel;
* one WhatsApp identity maps to at most one ``user_id`` and vice versa, so a
  message can never be attributed to two guests.
"""

from __future__ import annotations

import json
import re
import stat
from dataclasses import dataclass
from pathlib import Path

from .errors import UnauthorizedError

#: Server = "user" part of a WhatsApp JID, e.g. ``15551230001``.
_PHONE_JID = re.compile(r"^\d{6,20}$")
_LID = re.compile(r"^[A-Za-z0-9._-]{4,64}$")
_ALLOWED_SERVERS = ("s.whatsapp.net", "lid")
_FORBIDDEN_SERVERS = ("g.us", "broadcast", "newsletter", "status")


def normalize_jid(raw: str) -> str:
    """Normalise a WhatsApp JID to ``user@server`` form.

    Raises :class:`UnauthorizedError` for anything that cannot safely be
    matched (device suffixes are dropped, never guessed).
    """
    if not isinstance(raw, str) or not raw.strip():
        raise UnauthorizedError("empty WhatsApp identity")
    text = raw.strip().lower()
    if "/" in text:
        # whatsmeow sometimes surfaces `user@server/resource`; resource is not
        # part of the identity.
        text = text.split("/", 1)[0]
    if "@" not in text:
        raise UnauthorizedError(f"identity {raw!r} is not a JID (no '@')")
    user, server = text.rsplit("@", 1)
    if server in _FORBIDDEN_SERVERS:
        raise UnauthorizedError(f"chat type {server!r} is not permitted for the pilot")
    if server not in _ALLOWED_SERVERS:
        raise UnauthorizedError(f"unsupported JID server {server!r}")
    # Strip the device suffix: "15551230001:12@s.whatsapp.net" -> "15551230001"
    if ":" in user:
        user = user.split(":", 1)[0]
    # Canonicalize the optional human "+" prefix away: whatsmeow emits plain
    # digits, and an operator typing "+<number>" in the allowlist must still
    # match. Without this, a leading '+' makes the intended friend unverified.
    if server == "s.whatsapp.net":
        user = user.lstrip("+")
    if server == "s.whatsapp.net" and not _PHONE_JID.fullmatch(user):
        raise UnauthorizedError(f"malformed phone-number JID {raw!r}")
    if server == "lid" and not _LID.fullmatch(user):
        raise UnauthorizedError(f"malformed LID JID {raw!r}")
    return f"{user}@{server}"


@dataclass(frozen=True)
class Identity:
    wa_jid: str
    user_id: str
    enabled: bool = True
    note: str = ""


class Allowlist:
    """Immutable, exact-match identity allowlist."""

    def __init__(self, identities: tuple[Identity, ...], source: str = "<memory>") -> None:
        self._by_jid: dict[str, Identity] = {}
        self._users: dict[str, Identity] = {}
        for ident in identities:
            jid = normalize_jid(ident.wa_jid)
            if not ident.user_id or not re.fullmatch(r"[a-z0-9][a-z0-9_-]{1,31}", ident.user_id):
                raise UnauthorizedError(f"allowlist: invalid user_id {ident.user_id!r}")
            if jid in self._by_jid:
                raise UnauthorizedError(f"allowlist: duplicate identity {jid}")
            if ident.user_id in self._users:
                raise UnauthorizedError(
                    f"allowlist: user_id {ident.user_id!r} is claimed by two WhatsApp identities"
                )
            normalised = Identity(wa_jid=jid, user_id=ident.user_id, enabled=ident.enabled, note=ident.note)
            self._by_jid[jid] = normalised
            self._users[ident.user_id] = normalised
        self.source = source

    # -- queries ---------------------------------------------------------
    def __len__(self) -> int:
        return len(self._by_jid)

    @property
    def user_ids(self) -> tuple[str, ...]:
        return tuple(sorted(self._users))

    def resolve(self, raw_jid: str) -> str:
        """Map a sender JID to a ``user_id`` or raise :class:`UnauthorizedError`."""
        jid = normalize_jid(raw_jid)
        ident = self._by_jid.get(jid)
        if ident is None:
            raise UnauthorizedError(f"identity {jid} is not on the allowlist")
        if not ident.enabled:
            raise UnauthorizedError(f"identity {jid} is disabled")
        return ident.user_id

    def jid_for_user(self, user_id: str) -> str | None:
        ident = self._users.get(user_id)
        return ident.wa_jid if ident else None

    def contains(self, raw_jid: str) -> bool:
        try:
            self.resolve(raw_jid)
        except UnauthorizedError:
            return False
        return True

    def public_view(self) -> list[dict[str, object]]:
        """Redacted view safe to log / show in a status command."""
        def mask(jid: str) -> str:
            user, _, server = jid.partition("@")
            return f"{user[:3]}***@{server}"

        return [
            {"wa_jid": mask(i.wa_jid), "user_id": i.user_id, "enabled": i.enabled}
            for i in sorted(self._by_jid.values(), key=lambda i: i.user_id)
        ]


def load_allowlist(path: str | Path) -> Allowlist:
    path = Path(path)
    if not path.is_file():
        raise UnauthorizedError(f"allowlist file not found: {path}")
    mode = stat.S_IMODE(path.stat().st_mode)
    if mode & 0o077:
        raise UnauthorizedError(
            f"allowlist {path} is group/world accessible (mode {mode:04o}); chmod 600 it"
        )
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise UnauthorizedError(f"allowlist {path} is not valid JSON: {exc}") from exc
    if not isinstance(data, dict) or data.get("version") != 1:
        raise UnauthorizedError("allowlist: expected an object with version: 1")
    entries = data.get("identities")
    if not isinstance(entries, list):
        raise UnauthorizedError("allowlist: 'identities' must be a list")
    identities: list[Identity] = []
    for index, entry in enumerate(entries):
        if not isinstance(entry, dict):
            raise UnauthorizedError(f"allowlist.identities[{index}]: expected an object")
        unknown = set(entry) - {"wa_jid", "user_id", "enabled", "note"}
        if unknown:
            raise UnauthorizedError(f"allowlist.identities[{index}]: unknown key(s) {sorted(unknown)}")
        if not isinstance(entry.get("wa_jid"), str) or not isinstance(entry.get("user_id"), str):
            raise UnauthorizedError(f"allowlist.identities[{index}]: wa_jid and user_id are required strings")
        identities.append(
            Identity(
                wa_jid=entry["wa_jid"],
                user_id=entry["user_id"],
                enabled=bool(entry.get("enabled", True)),
                note=str(entry.get("note", "")),
            )
        )
    return Allowlist(tuple(identities), source=str(path))
