"""`kurisu` CLI (Phase 0: `check` only; `init`/`start`/`update` arrive in Phase 1)."""

from __future__ import annotations

import argparse
import sys

from .config import load_config
from .errors import ConfigError
from .roster import load_roster


def cmd_check(args: argparse.Namespace) -> int:
    try:
        cfg = load_config(args.config)
        roster = load_roster(args.roster or cfg.roster_path)
    except (ConfigError, OSError) as exc:
        print(f"[FAIL] {exc}", file=sys.stderr)
        return 2
    print(f"[ OK ] config: channels={[c.kind for c in cfg.channels if c.enabled]} data_dir={cfg.data_dir}")
    print(f"[ OK ] roster: lead={roster.lead.name} staff={[r.name for r in roster.staff]}")
    for event in ("merge", "push", "deploy"):
        if roster.gates_for(event):
            print(f"[ OK ] gate {event}: {', '.join(roster.gates_for(event))}")
    return 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="kurisu")
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("check", help="validate kurisu.yaml and roster.yaml")
    p.add_argument("--config", required=True)
    p.add_argument("--roster")
    p.set_defaults(func=cmd_check)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
