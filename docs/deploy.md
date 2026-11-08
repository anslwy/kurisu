# Deploying Kurisu

`ku source install --from /path/to/candidate --target /path/to/installed/ku --yes`
probes a candidate against a consistent SQLite backup in a temporary home. The
shadow has no Telegram, web listener, browser, scheduled wakes or resumed turns.
It must answer version, session count, history and schema reads before the
installer changes the installed binary or public socket.

The candidate then starts on a separate socket in standby. The old server passes
its phone web listening FD to the candidate, keeping the TCP accept backlog open. The installer checks
it on the real home, atomically repoints `kurisu.sock`, checks it again, and drains
the old server for up to 60 seconds. Accepted phone HTTP requests finish before
the old listener closes. Approved web pairings transfer before promotion.
New turns wait for promotion. The old server
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

The acceptance drill performs five consecutive handovers under 30 ms HTTP load
on both frontends. Every request must return 200 and ownership samples must never
show two primaries or pollers. It reports exact counts and the measured handover
window for each switch; the soft target is below 50 ms. See `deploy-validation.md`.

The first upgrade from a legacy daemon without descriptor export uses the
compatibility drain path. Listening FD and pairing preservation require an old
daemon that supports the handover protocol.

## Standalone frontend

After promotion, an existing `ku web --serve` process is recycled with the new
binary, its bound address, original home and exact environment token. No process
means a reported skip; multiple PIDs or a missing token abort preflight.
The served hashed asset set and frontend build endpoint must match the candidate
before the installed binary path is published. Failure restores the old frontend
and leaves the installed binary path unchanged. Identical assets are valid for
a same-commit redeploy, but the process must still be replaced and verified.

`ku doctor` reports `/WEB BUILD` against the daemon's embedded asset/build identity.
An old frontend without verifiable build metadata warns or fails rather than
reporting a false build OK. See `web-recycle-validation.md` for measured UI gaps
and native EventSource reconnection results.
