#!/bin/sh
# Deploy a fresh Kurisu build to an instance, with a heads-up and a rollback.
#
#   scripts/deploy-live.sh [--dry-run] [--prod]   (never prompts)
#
# The candidate is tested against a copied store before the public socket moves.
# Then the old server drains, the candidate promotes, and clients reconnect.
# CLI processes may restart; history is retained and interrupted turns resume.
# A failed post-swap probe restores the previous socket and binary.
#
# Env:
#   KURISU_SRC            source checkout to build   (default: this repo)
#   KURISU_LIVE_ROOT      parent of the instance     (default: this machine's .../omni/dev)
#   KURISU_LIVE_DIR       dir with ctl.sh            (default: $KURISU_LIVE_ROOT/kurisu-live-test)
#   KURISU_LIVE_BIN       installed binary           (default: $KURISU_LIVE_ROOT/kurisu-bridge-test/target/release/ku)
#   KURISU_NOTICE_LEAD    pause after the Telegram heads-up (default: 2 s)
#
# Safety: a home that has telegram.json is a Telegram-connected instance (the
# prod bridge), so it is refused unless --prod is given. Never test against
# prod: give a test its own KURISU_HOME under /tmp (e.g. /tmp/kurisu-stage/...),
# its own port (e.g. 8799) and no Telegram credentials.
#
# Noninteractive. --dry-run changes nothing. The source tree must be clean.
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
    printf '  4. shadow probe: copied store, version, sessions, schema reads\n'
    printf '  5. %s source install --from %s --target %s --yes\n' "$NEW_BIN" "$NEW_BIN" "$LIVE_BIN"
    printf '  6. atomic socket switch, post-swap probe, drain, promote; rollback on failure\n'
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

# ---------------------------------------------------------------- handover
notice="deploy: probing the candidate, then handing over; active turns drain for up to 60s"
printf '%s\n' "$notice"
notify_telegram "$notice"
run env KURISU_HOME="$HOME_DIR" "$NEW_BIN" source install --from "$NEW_BIN" --target "$LIVE_BIN" --yes
got=$(env KURISU_HOME="$HOME_DIR" "$NEW_BIN" version --json)
printf '%s\n' "$got"
# Probe the running process, not just the file installed on disk.
printf '%s\n' "$got" | python3 -c 'import json,sys; v=json.load(sys.stdin); assert v["git_sha"] == sys.argv[1], v' "$head_commit"
. "$SRC/scripts/doctor-after-deploy.sh"
doctor_after_deploy
say "deployed $(git -C "$SRC" rev-parse --short HEAD) through atomic handover"
