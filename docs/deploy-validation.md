# Deployment validation (2026-11-08)

Production code tested: `6a733ba7dc700c2aa5c8d61504db1d4f4fe7b460`.
Drill: `81ba656cb1186ffaad89fe5ef387c58e0c11a0ca`.
All drills used throwaway `/tmp/ku-deploy-*` homes, OS-assigned loopback ports,
a mock Telegram endpoint and fake credentials. No acceptance test deployed live.

## Tests and release

Rootfs Rust toolchain and library path, shared cargo cache and Zig linker:

```
cargo test -p kurisu -- --test-threads=2
test result: ok. 172 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 31.93s
test result: ok. 172 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 32.04s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.50s

cargo build --release -p kurisu --bin ku
Finished `release` profile [optimized] target(s) in 1m 31s

python3 scripts/test-deploy.py target/release/ku
```

The first parallel test attempt hit the pre-existing five-second Codex fixture
startup deadline. Its targeted retry passed; the complete suite above passed
with bounded concurrency. No test assertion or timeout was weakened.

## Release acceptance

- A real ACP prompt held in flight finished across handover; history remained
  and the next turn succeeded. Mid-turn handover: 733 ms; post-swap probe: 1 ms.
- A candidate reporting the expected version but exiting with code 71 on startup
  was rejected in shadow. Live version, PID and installed bytes stayed unchanged.
- Injected post-swap failure restored the previous socket and binary; its original
  process remained live and answered another turn.
- A Telegram update submitted during handover was answered afterwards.
- HARD 1: 77 persistent-control snapshots; maximum primary owners
  1, maximum Telegram pollers 1. Never two.
- HARD 2: 636 HTTP requests, 0 failures, every response 200.
  Independent clients request each route every 30 ms, without client retries:
  `/web` and `/api/v1/sessions` on the standalone frontend, plus the actual paired
  phone UI `/_web/` and its authenticated `/_web/api/sessions` API. Each route:
  159 requests. Maximum HTTP duration: 205.8081029681489 ms.
- Five consecutive poller-stop-to-promotion-request windows, in ms:
  `13.18156, 6.788555, 6.752325, 6.108654, 6.443026`.
  Maximum: **13.18156 ms**, below the soft 50 ms target.
- Poller-stop-to-running windows, in ms:
  `13.916481, 7.351645, 7.505156, 6.704545, 7.251446`.

Timestamps come from the old server immediately after stopping its poller, the
installer immediately before requesting promotion, and the new server after its
poller starts. These are measured handover windows, not failed acceptance checks.
The phone's listening FD stays open across handover; accepted HTTP requests drain,
and approved web pairing cookies transfer before the candidate starts accepting.

Machine-local evidence: `/tmp/kurisu-zd-phone-tests2.log`,
`/tmp/kurisu-zd-phone-build.log`, `/tmp/kurisu-zd-phone-drill2.log`,
`/tmp/ku-deploy-aldacmsx/acceptance.json`, `http-samples.json`, `samples.json`,
and `deploy-events.jsonl` in the same drill directory.
