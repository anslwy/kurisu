#!/bin/sh
# Deploy a fresh Kurisu build to the live test instance, with a rollback.
#
#   scripts/deploy-live.sh [--dry-run]
#
# Builds the repo, backs up the installed binary, stops the instance, swaps the
# binary in, starts it again and checks that `ku --version` reports the commit
# that is checked out. If any step after the swap fails, the backup is put back
# and the instance is started on it.
#
# Env (defaults are this machine's live test setup):
#   KURISU_SRC        source checkout to build  (default: this repo)
#   KURISU_LIVE_ROOT  parent of the live pieces (default: .../omni/dev)
#   KURISU_LIVE_DIR   instance dir with ctl.sh  (default: $KURISU_LIVE_ROOT/kurisu-live-test)
#   KURISU_LIVE_BIN   installed binary          (default: $KURISU_LIVE_ROOT/kurisu-bridge-test/target/release/ku)
#
# The source tree must be clean; the script pulls --ff-only and refuses to run
# on any local change. --dry-run prints the plan and touches nothing.
set -eu

usage() {
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
}

dry_run=0
case "${1:-}" in
    "") ;;
    --dry-run) dry_run=1 ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'unknown argument: %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
esac
[ "$#" -le 1 ] || { printf 'too many arguments\n\n' >&2; usage >&2; exit 2; }

here=$(cd "$(dirname "$0")/.." && pwd)
SRC=${KURISU_SRC:-$here}
LIVE_ROOT=${KURISU_LIVE_ROOT:-/run/media/deck/SDCARD/distrobox/omni/dev}  # lint-paths: allow (this machine's live test setup)
LIVE_DIR=${KURISU_LIVE_DIR:-$LIVE_ROOT/kurisu-live-test}
LIVE_BIN=${KURISU_LIVE_BIN:-$LIVE_ROOT/kurisu-bridge-test/target/release/ku}
CTL=$LIVE_DIR/ctl.sh
NEW_BIN=$SRC/target/release/ku
HOME_DIR=$LIVE_DIR/kurisu-home

say() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
run() {
    say "$*"
    "$@"
}

swapped=0
backup=
restore() {
    status=$?
    if [ "$status" -ne 0 ] && [ "$swapped" -eq 1 ] && [ -n "$backup" ]; then
        printf '\n==> deploy failed: restoring %s\n' "$backup" >&2
        "$CTL" stop >/dev/null 2>&1 || true
        if install -m 755 "$backup" "$LIVE_BIN"; then
            "$CTL" start >/dev/null 2>&1 || printf 'warning: ctl.sh start failed after the restore\n' >&2
            printf '==> restored the previous binary and started it again\n' >&2
        else
            printf 'RESTORE FAILED: put %s back at %s by hand\n' "$backup" "$LIVE_BIN" >&2
        fi
    fi
    exit "$status"
}
trap restore EXIT

# ---------------------------------------------------------------- checks
git -C "$SRC" rev-parse --git-dir >/dev/null 2>&1 || die "$SRC is not a git checkout"
[ -x "$CTL" ] || die "no executable ctl.sh at $CTL"
[ -d "$HOME_DIR" ] || die "no Kurisu home at $HOME_DIR"
[ -d "$(dirname "$LIVE_BIN")" ] || die "no directory for the live binary: $(dirname "$LIVE_BIN")"
[ -f "$LIVE_BIN" ] || die "no installed binary at $LIVE_BIN to back up"

dirty=$(git -C "$SRC" status --porcelain)
if [ -n "$dirty" ]; then
    if [ "$dry_run" -eq 1 ]; then
        printf 'warning: the source tree is dirty; a real run would stop here:\n%s\n\n' "$dirty" >&2
    else
        printf 'error: the source tree has local changes; commit or stash them first:\n%s\n' "$dirty" >&2
        exit 1
    fi
fi

if [ -f "$SRC/.toolchain/rust-env.sh" ]; then
    . "$SRC/.toolchain/rust-env.sh"
fi
command -v cargo >/dev/null 2>&1 || die "cargo not found (looked in \$PATH and $SRC/.toolchain)"
command -v git >/dev/null 2>&1 || die "git not found"
command -v install >/dev/null 2>&1 || die "install(1) not found"

head_commit=$(git -C "$SRC" rev-parse HEAD)
branch=$(git -C "$SRC" rev-parse --abbrev-ref HEAD)

say "source:   $SRC ($branch @ $(git -C "$SRC" rev-parse --short HEAD))"
say "live:     $LIVE_DIR"
say "binary:   $LIVE_BIN"

if [ "$dry_run" -eq 1 ]; then
    printf '\nplan (dry run, nothing changed):\n'
    printf '  1. git -C %s pull --ff-only\n' "$SRC"
    printf '  2. (cd %s && cargo build --release -p kurisu)\n' "$SRC"
    printf '  3. back up %s -> %s.bak-<timestamp>\n' "$LIVE_BIN" "$LIVE_BIN"
    printf '  4. %s stop\n' "$CTL"
    printf '  5. install -m 755 %s %s\n' "$NEW_BIN" "$LIVE_BIN"
    printf '  6. %s start\n' "$CTL"
    printf '  7. %s status, expect "kurisu: up"\n' "$CTL"
    printf '  8. %s --version, expect commit %s\n' "$LIVE_BIN" "$head_commit"
    printf '\n(dry run) done\n'
    exit 0
fi

# ---------------------------------------------------------------- deploy
run git -C "$SRC" pull --ff-only
head_commit=$(git -C "$SRC" rev-parse HEAD)

say "building (cargo build --release -p kurisu)"
( cd "$SRC" && cargo build --release -p kurisu )
[ -x "$NEW_BIN" ] || die "the build produced no $NEW_BIN"

backup=$LIVE_BIN.bak-$(date +%Y%m%d-%H%M%S)
say "backing up $LIVE_BIN -> $backup"
cp -p "$LIVE_BIN" "$backup"

run "$CTL" stop

say "installing $NEW_BIN -> $LIVE_BIN"
swapped=1
install -m 755 "$NEW_BIN" "$LIVE_BIN"

run "$CTL" start

status=$("$CTL" status)
printf '%s\n' "$status"
case "$status" in
    *"kurisu: up"*) ;;
    *) die "the instance is not up after the deploy" ;;
esac

say "checking the version"
KURISU_HOME=$HOME_DIR "$LIVE_BIN" --version
got=$(KURISU_HOME=$HOME_DIR "$LIVE_BIN" --version | sed -n 's/^source: .* commit //p')
[ "$got" = "$head_commit" ] || die "the running binary reports commit ${got:-none}, expected $head_commit"

say "deployed $(git -C "$SRC" rev-parse --short HEAD) (backup: $backup)"
