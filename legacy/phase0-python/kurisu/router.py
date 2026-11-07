"""Model routing by difficulty (policy only; no model is called here).

* Each *task* gets a tier: mechanical / normal / hard.
* A worker that struggles (failed checks, repeated retries, self-reported
  stuck) is escalated for its **next** task, one step at a time, capped.
* The model never changes mid-task: switching only between tasks keeps the
  prompt cache warm. ``Router.decide`` is therefore called once per task.
* Every decision is returned as a record so it can be logged with its cost.
"""

from __future__ import annotations

from dataclasses import dataclass, field

TIERS = ("mechanical", "normal", "hard")
EFFORTS = ("low", "medium", "high", "xhigh", "max")

#: Defaults are placeholders the user edits in kurisu.yaml later; they name
#: model families, not account- or price-specific ids.
DEFAULT_TABLE: dict[str, tuple[str, str]] = {
    "mechanical": ("haiku", "low"),
    "normal": ("sonnet", "medium"),
    "hard": ("opus", "high"),
}


@dataclass(frozen=True)
class Decision:
    role: str
    task_id: str
    tier: str
    model: str
    effort: str
    reason: str


@dataclass
class Router:
    table: dict[str, tuple[str, str]] = field(default_factory=lambda: dict(DEFAULT_TABLE))
    max_escalations: int = 2
    _struggle: dict[str, int] = field(default_factory=dict)
    _current: dict[str, str] = field(default_factory=dict)  # role -> task in progress

    def report_struggle(self, role: str) -> None:
        self._struggle[role] = min(self._struggle.get(role, 0) + 1, self.max_escalations)

    def report_success(self, role: str) -> None:
        self._struggle.pop(role, None)

    def decide(self, role: str, task_id: str, base_tier: str) -> Decision:
        if base_tier not in TIERS:
            raise ValueError(f"unknown tier {base_tier!r}")
        if self._current.get(role) not in (None, task_id):
            raise RuntimeError(
                f"role {role!r} is still on task {self._current[role]!r}; finish it before routing a new "
                "task (models switch only between tasks)"
            )
        bump = self._struggle.get(role, 0)
        idx = TIERS.index(base_tier)
        tier_idx = min(idx + bump, len(TIERS) - 1)
        tier = TIERS[tier_idx]
        model, effort = self.table[tier]
        # Already at the top tier: escalate effort instead.
        overflow = idx + bump - tier_idx
        if overflow > 0:
            effort = EFFORTS[min(EFFORTS.index(effort) + overflow, len(EFFORTS) - 1)]
        reason = f"base={base_tier}" + (f", struggle+{bump}" if bump else "")
        self._current[role] = task_id
        return Decision(role=role, task_id=task_id, tier=tier, model=model, effort=effort, reason=reason)

    def finish(self, role: str, task_id: str) -> None:
        if self._current.get(role) == task_id:
            del self._current[role]
