# Deployment validation (2026-11-08)

Production code tested: `b1bcb81726fa3a37d66af1ca9ed69f2131cca6da`.
Ownership drill: `a9049bc` (adds persistent control-connection sampling).
All deployment drills used fresh `/tmp/ku-deploy-*` homes and random loopback
Telegram API ports, with fake credentials. They did not deploy the live instance.

Rootfs Rust toolchain, shared cargo cache, rootfs library path and Zig linker:

```
cargo test -p kurisu
test result: ok. 169 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 33.30s
test result: ok. 169 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 29.55s
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.45s

cargo build --release -p kurisu --bin ku
Finished `release` profile [optimized] target(s) in 5m 19s
```

Release acceptance command:

```
KURISU_DEPLOY_REQUIRE_CONTINUOUS=1 python3 scripts/test-deploy.py target/release/ku
```

- (a) Real ACP prompt held in flight: finished across handover, retained history,
  and accepted another turn afterwards. Shadow: 129 ms; handover: 818 ms;
  post-swap socket probe: 1 ms; drained: true.
- (b) Candidate that reports a valid version but exits with code 71 on startup:
  rejected in shadow. Running build identity, process PID and installed binary
  bytes were unchanged.
- (c) Injected post-swap fault: old socket and binary restored after 11 ms;
  previous PID remained live and another session turn succeeded.
- A Telegram update submitted during the handover was answered; the fixture
  uses real HTTP polling against a loopback API, not Telegram production.
- (d) **FAIL for literally exactly one continuously.** 21 persistent-control
  snapshots: primary server count 0–1, Telegram poller count 0–1. Never duplicate
  owners, but a zero-owner interval exists between old shutdown and promotion.

Exact failure at `scripts/test-deploy.py:118`:

```
AssertionError: acceptance (d) failed: an ownership sample was 0, expected exactly 1 continuously
```

The old poller stops at `crates/kurisu/src/server.rs:3068`; the installer waits
for old shutdown before invoking promotion at `crates/kurisu/src/deploy.rs:179`.
This meets no-overlap ownership, but not the literal continuous-exactly-one
criterion. The PR remains a draft; it has not been merged or deployed live.

Machine-local evidence: `/tmp/kurisu-zd-test-verified.log`,
`/tmp/kurisu-zd-release.log`, `/tmp/kurisu-zd-release-drill.log`, and
`/tmp/ku-deploy-0zidrite/samples.json`.
