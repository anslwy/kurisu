#!/usr/bin/env python3
"""Fail if tracked files contain hard-coded, user-specific absolute paths.

Allowed: placeholders such as /run/media/<user>/..., ${VAR} expansion, and
generic container/test homes (/home/guest, /home/operator, /home/user).
A line may opt out with the marker `lint-paths: allow` (for tests that check
that such a path is *redacted*).

Usage: scripts/lint-paths.py [paths...]   (default: all git-tracked files)
"""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

RULES = [
    ("user media mount", re.compile(r"/run/media/(?!<)[A-Za-z0-9_.-]+/")),
    ("user home", re.compile(r"/home/(?!(guest|operator|user|runner|<)\b)[a-z_][a-z0-9_-]*")),
    ("macOS home", re.compile(r"/Users/(?!(Shared|<)\b)[A-Za-z0-9_.-]+/")),
    ("agent scratch dir", re.compile(r"/tmp/claude-\d+")),
    ("host-specific name", re.compile(r"\bsteamdeck\b|\bSDCARD\b")),
]
ALLOW_MARK = "lint-paths: allow"
SKIP_SUFFIXES = {".png", ".jpg", ".gif", ".ico", ".sum", ".lock"}
SELF = "scripts/lint-paths.py"


def tracked() -> list[str]:
    out = subprocess.run(["git", "ls-files", "-z"], capture_output=True, check=True)
    return [p for p in out.stdout.decode().split("\0") if p]


def scan(paths: list[str]) -> list[str]:
    problems = []
    for rel in paths:
        if rel == SELF or Path(rel).suffix in SKIP_SUFFIXES:
            continue
        try:
            text = Path(rel).read_text(encoding="utf-8")
        except (UnicodeDecodeError, FileNotFoundError, IsADirectoryError):
            continue
        for n, line in enumerate(text.splitlines(), 1):
            if ALLOW_MARK in line:
                continue
            for name, rx in RULES:
                m = rx.search(line)
                if m:
                    problems.append(f"{rel}:{n}: {name}: {m.group(0)}")
    return problems


def main(argv: list[str]) -> int:
    problems = scan(argv or tracked())
    for p in problems:
        print(p)
    if problems:
        print(f"\n{len(problems)} hard-coded path(s). Use placeholders, ${{VAR}} or config instead.", file=sys.stderr)
        return 1
    print("lint-paths: clean")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
