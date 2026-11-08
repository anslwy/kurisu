# Standalone web recycling validation

All tests used SD-card temp storage and random ports; no live process was touched.

```
cargo test -p kurisu -- --test-threads=2
test result: ok. 183 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 25.82s
test result: ok. 183 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 31.10s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.56s
```

Two fresh-instance drills used an older UI binary and the new release. Each preserved
the exact token, bound address and home, completed a session turn afterwards, and
verified that the promoted daemon PID stayed unchanged during frontend recycling.

## Run 1

- Port: `37087`; frontend PID `3069108` to `3069138`.
- Observed old-process exit to first new HTTP 200: **15.298293 ms**.
- Served asset-hash swap: **yes**.
- Native Node EventSource (no custom retry loop) reconnected and received a new hello: **3006 ms**.
- Doctor: `/WEB BUILD         OK   {"git_sha":"ba207b4172aada1619d8d3b13270bd78b835131c","built":"1794174427"} / {"git_sha":"ba207b4172aada1619d8d3b13270bd78b835131c","built":"1794174427"} pid=3069138 assets=match`
- Old assets: `assets/Splitter--8KvJ2nY.css`, `assets/Splitter-DJiLTrZq.js`, `assets/main-CND8Jnv9.js`, `assets/main-DGkzarqb.css`
- New assets: `assets/Splitter-Cj6Q1Uaa.css`, `assets/Splitter-DkNafShd.js`, `assets/main-DGkzarqb.css`, `assets/main-DPGeyYMW.js`

## Run 2

- Port: `43485`; frontend PID `3069164` to `3069191`.
- Observed old-process exit to first new HTTP 200: **11.541047 ms**.
- Served asset-hash swap: **yes**.
- Native Node EventSource (no custom retry loop) reconnected and received a new hello: **3005 ms**.
- Doctor: `/WEB BUILD         OK   {"git_sha":"ba207b4172aada1619d8d3b13270bd78b835131c","built":"1794174427"} / {"git_sha":"ba207b4172aada1619d8d3b13270bd78b835131c","built":"1794174427"} pid=3069191 assets=match`
- Old assets: `assets/Splitter--8KvJ2nY.css`, `assets/Splitter-DJiLTrZq.js`, `assets/main-CND8Jnv9.js`, `assets/main-DGkzarqb.css`
- New assets: `assets/Splitter-Cj6Q1Uaa.css`, `assets/Splitter-DkNafShd.js`, `assets/main-DGkzarqb.css`, `assets/main-DPGeyYMW.js`

## Refusal and recovery cases

- Multiple frontend PIDs were printed and refused before changing daemon PID or installed bytes.
- Missing/invalid tokens are refused by unit tests; the exact existing token is retained.
- No frontend running: skip printed, no frontend spawned.
- A candidate that launched the old UI failed served-asset verification and returned nonzero.
  Installed binary bytes stayed unchanged; the old frontend/token were restored.
- Doctor returned `/WEB BUILD FAIL` for that stale UI beside the promoted current daemon.

Identical candidate assets on a same-commit redeploy are allowed only after recycling
the process and checking its new PID, build identity, served assets and preserved-token API.
A stale unchanged asset set that differs from the candidate always fails.

The measured UI gap is not zero. Existing SSE clients lose their connection and use
their standard EventSource reconnect timer; the daemon/session processes are not
signalled by the frontend recycle. New frontends transfer the listening FD and finish
accepted HTTP requests before closing SSE, keeping queued connections across restarts.

Commands:
```
KURISU_TEST_TMPDIR=$SD/tmp TMPDIR=$SD/tmp python3 scripts/test-web-recycle.py target/release/ku OLD_UI_KU
```
Machine-local detailed records: `$SD/tmp/web-recycle-acceptance.json` and
`$SD/tmp/ku-web-recycle-drill.log`.
