#!/bin/sh
# Deploy a fresh Kurisu build to an instance, with a heads-up and a rollback.
#
#   scripts/deploy-live.sh [--dry-run] [--prod]   (never prompts)
#
# Order, so an instance is down for a few seconds at most:
#   pull -> build -> check the NEW binary's --version against HEAD -> back up ->
#   heads-up -> stop -> install -> start -> check it is up on
#   that commit. Nothing is stopped until the new binary exists and reports the
#   right commit; a failed swap puts the backup back and starts it again.
#
# Sessions survive a restart: they resume on their next message, so the downtime
# only affects turns that are in flight.
#
# Env:
#   KURISU_SRC            source checkout to build   (default: this repo)
#   KURISU_LIVE_ROOT      parent of the instance     (default: this machine's .../omni/dev)
#   KURISU_LIVE_DIR       dir with ctl.sh            (default: $KURISU_LIVE_ROOT/kurisu-live-test)
#   KURISU_LIVE_BIN       installed binary           (default: $KURISU_LIVE_ROOT/kurisu-bridge-test/target/release/ku)
#   KURISU_SWAP_SECONDS   announced downtime         (default: 5)
#   KURISU_NOTICE_LEAD    pause after the Telegram heads-up (default: 2 s)
#
# Safety: a home that has telegram.json is a Telegram-connected instance (the
# prod bridge), so it is refused unless --prod is given. Never test against
# prod: give a test its own KURISU_HOME under /tmp (e.g. /tmp/kurisu-stage/...),
# its own port (e.g. 8799) and no Telegram credentials.
#
# Nothing is interactive: before the swap the script prints
#   deploy: taking the bridge down now (~Ns), sessions resume on their next message
# and, when the instance has a Telegram config, sends the same line to the
# owner's chat through the instance's own channel (KURISU_NOTICE_LEAD seconds of
# lead time, default 2). A failed heads-up is a note, never a failure.
# --yes/-y and KURISU_DEPLOY_YES are accepted and ignored: nothing ever waits
# for a human. --dry-run prints the plan and changes nothing. The source tree
# must be clean.
set -eu

usage() {
    sed -n '2,/^set -eu$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

dry_run=0
prod=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=1 ;;
        --yes|-y) ;;  # no-op: kept so older invocations keep working
        --prod) prod=1 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'unknown argument: %s\n\n' "$arg" >&2; usage >&2; exit 2 ;;
    esac
done

here=$(cd "$(dirname "$0")/.." && pwd)
SRC=${KURISU_SRC:-$here}
LIVE_ROOT=${KURISU_LIVE_ROOT:-/run/media/deck/SDCARD/distrobox/omni/dev}  # lint-paths: allow (this machine's live test setup)
LIVE_DIR=${KURISU_LIVE_DIR:-$LIVE_ROOT/kurisu-live-test}
LIVE_BIN=${KURISU_LIVE_BIN:-$LIVE_ROOT/kurisu-bridge-test/target/release/ku}
SWAP_SECONDS=${KURISU_SWAP_SECONDS:-5}
CTL=$LIVE_DIR/ctl.sh
NEW_BIN=$SRC/target/release/ku
HOME_DIR=$LIVE_DIR/kurisu-home

say() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
run() {
    say "$*"
    "$@"
}
# The commit a `ku` binary reports, or nothing. `--version` is static, so this
# never opens the instance's home or database.
commit_of() { "$1" --version 2>/dev/null | sed -n 's/^source: .* commit //p'; }

# Best-effort heads-up to the owner's chat through the instance's own Telegram
# channel (`ku telegram notify`), so the token never passes through this script.
# The instance is still up at this point; any failure is a note, not an error.
notify_telegram() {
    [ -f "$HOME_DIR/telegram.json" ] || return 0
    if command -v timeout >/dev/null 2>&1; then
        timeout 10 env KURISU_HOME="$HOME_DIR" "$LIVE_BIN" telegram notify "$1" >/dev/null 2>&1 || {
            printf 'note: could not send the Telegram heads-up; deploying anyway\n' >&2
            return 0
        }
    else
        env KURISU_HOME="$HOME_DIR" "$LIVE_BIN" telegram notify "$1" >/dev/null 2>&1 || {
            printf 'note: could not send the Telegram heads-up; deploying anyway\n' >&2
            return 0
        }
    fi
    # Let the message leave the instance before it goes down.
    sleep "${KURISU_NOTICE_LEAD:-2}" || true
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

if [ -f "$HOME_DIR/telegram.json" ] && [ "$prod" -eq 0 ]; then
    if [ "$dry_run" -eq 1 ]; then
        printf 'warning: %s has telegram.json (a Telegram-connected instance); a real run would refuse without --prod\n\n' "$HOME_DIR" >&2
    else
        printf 'error: %s has telegram.json: this is a Telegram-connected instance (the prod bridge).\n' "$HOME_DIR" >&2
        printf '       Test elsewhere (own KURISU_HOME under /tmp, own port, no Telegram), or pass --prod.\n' >&2
        exit 1
    fi
fi

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
say "installed: commit $(commit_of "$LIVE_BIN")"
say "live:     $LIVE_DIR"
say "binary:   $LIVE_BIN"
if [ "$prod" -eq 1 ]; then say "prod:     yes (--prod given)"; fi

if [ "$dry_run" -eq 1 ]; then
    printf '\nplan (dry run, nothing changed):\n'
    printf '  1. git -C %s pull --ff-only (skipped in a dry run)\n' "$SRC"
    printf '  2. (cd %s && cargo build --release -p kurisu)\n' "$SRC"
    printf '  3. %s --version, expect commit %s (after the pull)\n' "$NEW_BIN" "$head_commit"
    printf '  4. back up %s -> %s.bak-<timestamp>\n' "$LIVE_BIN" "$LIVE_BIN"
    printf '  5. print the notice (+ Telegram heads-up to the owner chat, ~%ss lead)\n' "${KURISU_NOTICE_LEAD:-2}"
    printf '  6. %s stop; install -m 755 %s %s; %s start   (<%s s down)\n' "$CTL" "$NEW_BIN" "$LIVE_BIN" "$CTL" "$SWAP_SECONDS"
    printf '  7. %s status, expect "kurisu: up"\n' "$CTL"
    printf '  8. %s --version, expect commit %s; otherwise restore %s.bak-<timestamp>\n' "$LIVE_BIN" "$head_commit" "$LIVE_BIN"
    printf '\n(dry run) done\n'
    exit 0
fi

# ---------------------------------------------------------------- build
run git -C "$SRC" pull --ff-only
head_commit=$(git -C "$SRC" rev-parse HEAD)

say "building (cargo build --release -p kurisu)"
( cd "$SRC" && cargo build --release -p kurisu )
[ -x "$NEW_BIN" ] || die "the build produced no $NEW_BIN"

# Prove the artifact before touching the running instance.
say "checking the new binary (outside the instance, with no home set)"
"$NEW_BIN" --version
new_commit=$(commit_of "$NEW_BIN")
[ "$new_commit" = "$head_commit" ] || die "the built binary reports commit ${new_commit:-none}, expected $head_commit; nothing was stopped"

# ---------------------------------------------------------------- swap
backup=$LIVE_BIN.bak-$(date +%Y%m%d-%H%M%S)
say "backing up $LIVE_BIN -> $backup"
cp -p "$LIVE_BIN" "$backup"
cmp -s "$LIVE_BIN" "$backup" || die "the backup at $backup does not match $LIVE_BIN; nothing was stopped"

# Heads-up, then straight into the swap: no prompt, nothing may block.
notice="deploy: taking the bridge down now (~${SWAP_SECONDS}s), sessions resume on their next message"
printf '%s\n' "$notice"
printf '%s %s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$notice" >>"$LIVE_DIR/kurisu.log" 2>/dev/null || true
notify_telegram "$notice"

started=$(date +%s)
swapped=1
run "$CTL" stop
say "installing $NEW_BIN -> $LIVE_BIN"
install -m 755 "$NEW_BIN" "$LIVE_BIN"
run "$CTL" start
took=$(( $(date +%s) - started ))

say "checking the instance"
status=$("$CTL" status)
printf '%s\n' "$status"
case "$status" in
    *"kurisu: up"*) ;;
    *) die "the instance is not up after the deploy (restoring)" ;;
esac
got=$(commit_of "$LIVE_BIN")
[ "$got" = "$head_commit" ] || die "the installed binary reports commit ${got:-none}, expected $head_commit (restoring)"

if [ "$took" -gt "$SWAP_SECONDS" ]; then
    printf 'warning: the swap took %ss, longer than the %ss announced\n' "$took" "$SWAP_SECONDS" >&2
fi
say "deployed $(git -C "$SRC" rev-parse --short HEAD); down for ~${took}s (backup: $backup)"
