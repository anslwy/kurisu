#!/usr/bin/env python3
"""Apply rlimits, then exec the guest command (runs inside the sandbox).

Why this exists: a transient systemd *scope* can enforce cgroup properties
(memory, CPU, tasks, runtime, IO weight) but it cannot set POSIX rlimits on a
process it did not spawn. Disk/file-size and descriptor ceilings therefore have
to be applied from inside the namespace, immediately before the guest starts.

Usage:
    guest-exec.py --fsize 67108864 --nofile 4096 --nproc 512 -- <command> [args]
"""

from __future__ import annotations

import argparse
import os
import resource
import sys


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    if "--" not in argv:
        print("guest-exec: missing '-- <command>'", file=sys.stderr)
        return 2
    split = argv.index("--")
    opts, command = argv[:split], argv[split + 1:]
    if not command:
        print("guest-exec: empty command", file=sys.stderr)
        return 2

    parser = argparse.ArgumentParser(prog="guest-exec", add_help=False)
    parser.add_argument("--fsize", type=int, default=0)
    parser.add_argument("--nofile", type=int, default=0)
    parser.add_argument("--nproc", type=int, default=0)
    parser.add_argument("--core", type=int, default=0)
    parser.add_argument("--as", dest="as_bytes", type=int, default=0)
    args = parser.parse_args(opts)

    def _apply(limit: int, value: int, label: str) -> None:
        if value <= 0:
            return
        try:
            soft, hard = resource.getrlimit(limit)
            new_hard = value if hard == resource.RLIM_INFINITY else min(hard, value)
            resource.setrlimit(limit, (min(value, new_hard), new_hard))
        except (ValueError, OSError) as exc:
            print(f"guest-exec: cannot set {label}: {exc}", file=sys.stderr)
            raise

    _apply(resource.RLIMIT_FSIZE, args.fsize, "RLIMIT_FSIZE")
    _apply(resource.RLIMIT_NOFILE, args.nofile, "RLIMIT_NOFILE")
    _apply(resource.RLIMIT_NPROC, args.nproc, "RLIMIT_NPROC")
    _apply(resource.RLIMIT_AS, args.as_bytes, "RLIMIT_AS")
    try:
        resource.setrlimit(resource.RLIMIT_CORE, (args.core, args.core))
    except (ValueError, OSError):
        pass

    try:
        os.execvpe(command[0], command, os.environ)
    except OSError as exc:
        print(f"guest-exec: cannot exec {command[0]!r}: {exc}", file=sys.stderr)
        return 127
    return 1  # unreachable


if __name__ == "__main__":
    raise SystemExit(main())
