# Deploying Kurisu

`ku source install --from /path/to/candidate --target /path/to/installed/ku --yes`
probes a candidate against a consistent SQLite backup in a temporary home. The
shadow has no Telegram, web listener, browser, scheduled wakes or resumed turns.
It must answer version, session count, history and schema reads before the
installer changes the installed binary or public socket.

The candidate then starts on a separate socket in standby. The installer checks
it on the real home, atomically repoints `kurisu.sock`, checks it again, and drains
the old server for up to 60 seconds. New turns wait for promotion. The old server
releases its Telegram poller before the candidate starts polling; Telegram keeps
updates queued during this interval. CLI and web clients retry connections across
the switch. Shadow and standby servers are additional processes, but must never
start a second Telegram poller or execute a second turn for a busy session.

A failed post-swap check repoints the socket to the still-running old process and
restores the previous binary. `KURISU_DEPLOY_FAULT=post_swap` exercises this path.
A deployment lock prevents simultaneous installers from using the same slots.

Session CLI processes may restart. Their history remains in SQLite; turns cut off
at the drain deadline are recorded for recovery on promotion. This does not claim
that an arbitrary CLI process or every open streaming connection survives.

`scripts/deploy-live.sh --prod` builds and verifies the candidate, then uses this
same install path. It does not stop the instance with `ctl.sh`.

Run `python3 scripts/test-deploy.py target/debug/ku` for an isolated acceptance
drill. It uses actual server and ACP processes, a deliberately broken startup
candidate, post-swap fault injection, a held mid-turn prompt, and a loopback
Telegram API. Artifacts under the printed `/tmp/ku-deploy-*` directory include
deploy output and samples of each server's role and Telegram state. No real
Telegram credentials are used by this drill.
