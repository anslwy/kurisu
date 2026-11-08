#!/usr/bin/env python3
"""Deterministic `opencode models` fixture; no provider, auth or network access.

The model ids come from $KURISU_FIXTURE_HOME/fixture-models.txt (one per line),
so a test can add a provider/model while the server stays up.
"""
import os
import sys
from pathlib import Path

if '--version' in sys.argv:
    print('opencode fixture')
    sys.exit()

if len(sys.argv) < 2 or sys.argv[1] != 'models':
    print('fixture only implements: opencode models', file=sys.stderr)
    sys.exit(2)

root = Path(os.environ.get('KURISU_FIXTURE_HOME') or os.environ['KURISU_HOME'])
with (root / 'fixture-models-calls').open('a') as f:
    f.write('call\n')
listing = root / 'fixture-models.txt'
if listing.exists():
    print(listing.read_text())
else:
    print('\n'.join(['fixture/known-model', 'fixture/novel-cli-only-7']))
