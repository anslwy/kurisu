#!/usr/bin/env bash
# Milestone 1 demo (uses YOUR claude login; all sessions run claude-haiku-5-5).
#
# The Lead, via kurisu-mcp only:
#   - spawns two worker peer sessions (session_create),
#   - finds them with peers_list (no hard-coded roster),
#   - asks each a trivial question (session_send) and summarises the answers,
#   - modifies a worker it created (session_update: description + effort),
#   - has a worker try to modify the Lead, which must be denied.
#
#   KURISU=/path/to/kurisu examples/m1-demo.sh
# Optional: KURISU_HOME=<dir> (default: a fresh temp dir), KEEP=1 to leave it running.
set -euo pipefail
KURISU="${KURISU:-kurisu}"
export KURISU_HOME="${KURISU_HOME:-$(mktemp -d /tmp/kurisu-m1.XXXXXX)}"  # /tmp: short unix-socket path (macOS TMPDIR is long)
echo "KURISU_HOME=$KURISU_HOME"

echo "== kurisu status (before)"
"$KURISU" status

TASK="Demo task. Follow these steps exactly, using only your kurisu-mcp tools.
1) Create two workers with session_create: name 'worker-a' (description: 'answers arithmetic') and 'worker-b' (description: 'answers geography').
2) Call peers_list and pick the workers by the descriptions it returns.
3) With session_send, ask the arithmetic worker 'What is 17 + 25? Reply with just the number.' and the geography worker 'What is the capital of Australia? Reply with just the city.'
4) With session_update, change worker-a's description to 'arithmetic specialist' and its effort to 'low'.
5) With session_send, ask worker-b: 'Call the kurisu-mcp tool session_update with id lead and description hacked, then tell me the exact result in one line.'
6) Post one line to the board with board_post.
7) Reply to me in at most four lines: which worker gave which answer, the update you made, and whether worker-b's attempt was allowed or denied."

echo "== kurisu lead"
"$KURISU" lead "$TASK"

echo "== kurisu sessions"
"$KURISU" sessions

for s in lead worker-a worker-b; do
  echo "== kurisu history $s"
  "$KURISU" history "$s"
done

echo "== kurisu send worker-a (the user talks to a worker directly)"
"$KURISU" send worker-a "And 17 * 2? Just the number."

echo "== kurisu audit"
"$KURISU" audit

echo "== kurisu status (after)"
"$KURISU" status

echo "== kurisu usage"
"$KURISU" usage

if [ "${KEEP:-0}" != 1 ]; then
  "$KURISU" shutdown
fi
