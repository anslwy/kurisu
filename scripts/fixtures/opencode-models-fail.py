#!/usr/bin/env python3
"""A failing `opencode models`: proves Kurisu's graceful fallback."""
import sys

if '--version' in sys.argv:
    print('opencode fixture')
    sys.exit()

print('fixture: the OpenCode CLI cannot list models right now', file=sys.stderr)
sys.exit(3)
