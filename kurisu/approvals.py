"""Approval cards: single-use, expiring, audited.

Channel-independent model: Telegram renders it as inline buttons (ported
from the bridge, docs/bridge-reuse.md), the web UI as a dialog.
"""

from __future__ import annotations

import secrets
import time
from dataclasses import dataclass, field
from typing import Callable

#: Actions that always need the user's explicit approval.
ALWAYS_ASK = ("push", "deploy", "spend", "outbound_message", "delete_data", "credential_access")


@dataclass
class Approval:
    id: str
    role: str
    action: str
    summary: str
    created: float
    expires: float
    outcome: str = "pending"  # pending | approved | denied | expired


@dataclass
class ApprovalBook:
    ttl_seconds: int = 3600
    clock: Callable[[], float] = time.time
    audit: list[dict] = field(default_factory=list)
    _items: dict[str, Approval] = field(default_factory=dict)

    def request(self, role: str, action: str, summary: str) -> Approval:
        now = self.clock()
        item = Approval(id=secrets.token_urlsafe(9), role=role, action=action, summary=summary,
                        created=now, expires=now + self.ttl_seconds)
        self._items[item.id] = item
        self._log(item, "requested")
        return item

    def resolve(self, approval_id: str, approve: bool) -> Approval:
        item = self._items.get(approval_id)
        if item is None:
            raise KeyError("unknown approval")
        if item.outcome != "pending":
            raise ValueError(f"approval already {item.outcome}; approvals are single-use")
        if self.clock() >= item.expires:
            item.outcome = "expired"
            self._log(item, "expired")
            raise ValueError("approval expired")
        item.outcome = "approved" if approve else "denied"
        self._log(item, item.outcome)
        return item

    def is_approved(self, approval_id: str) -> bool:
        item = self._items.get(approval_id)
        return bool(item and item.outcome == "approved")

    def _log(self, item: Approval, event: str) -> None:
        self.audit.append({"t": self.clock(), "id": item.id, "role": item.role,
                           "action": item.action, "event": event})


def needs_approval(action: str) -> bool:
    return action in ALWAYS_ASK
