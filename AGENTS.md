# Local build environment

Use the supplied SD-card toolchain. Do not install dependencies without approval.

```sh
export SD=/run/media/deck/SDCARD/distrobox/omni/dev # lint-paths: allow (this workspace's supplied SD-card build root)
export CARGO_HOME=$SD/cargo-home
export TMPDIR=$SD/tmp
export KURISU_TEST_TMPDIR=$SD/tmp
export ZIG_GLOBAL_CACHE_DIR=$SD/zigcache
export ZIG_LOCAL_CACHE_DIR=$SD/zigcache
export CARGO_BUILD_JOBS=2
cargo test -p kurisu
```

Do not edit `../kurisu-deploy-src`; another worker owns it. Before running
`sh "$SD/run-deploy.sh"`, require its `git status --porcelain` output to be empty.
Post to the team board before and after deployment. If dirty, report and stop
short of deployment.
