"""Encrypted backups (plan only in Phase 0).

A backup is a tar of the Kurisu data dir (state, AGENTS.md, roster, logs),
encrypted to the user's own ``age`` public key. Kurisu never holds the private
key and never backs up Claude Code's own credential files.
"""

from __future__ import annotations

import time
from pathlib import Path

#: Never included, even if they live under the data dir.
EXCLUDE = (".claude.json", ".credentials.json", "*.token", "secrets/*")


def backup_plan(data_dir: Path, out_dir: Path, recipient: str, now: float | None = None) -> list[list[str]]:
    """argv list: tar the data dir and pipe through ``age -r <recipient>``."""
    if not recipient.startswith("age1"):
        raise ValueError("an age public key (age1...) is required; backups are never written unencrypted")
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime(now))
    target = Path(out_dir) / f"kurisu-{stamp}.tar.age"
    tar = ["tar", "-C", str(data_dir), *[f"--exclude={p}" for p in EXCLUDE], "-cf", "-", "."]
    age = ["age", "-r", recipient, "-o", str(target)]
    return [tar, age]
