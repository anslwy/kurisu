"""Front-end channels. Telegram first (ported from the bridge), web second,
WhatsApp optional (whatsmeow; off by default, see docs/terms-check.md).

Every channel only answers the configured owner.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Protocol


@dataclass(frozen=True)
class Inbound:
    channel: str
    sender_id: str
    text: str
    attachments: tuple[str, ...] = ()


@dataclass(frozen=True)
class Card:
    """A question/approval card rendered natively by each channel."""

    card_id: str
    title: str
    options: tuple[str, ...] = field(default_factory=tuple)


class Channel(Protocol):
    kind: str

    def send_text(self, text: str) -> None: ...
    def send_card(self, card: Card) -> None: ...


def is_owner(owner_id: str, sender_id: str) -> bool:
    """Fail closed: an unset owner matches nobody."""
    return bool(owner_id) and str(sender_id) == str(owner_id)
