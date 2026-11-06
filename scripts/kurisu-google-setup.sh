#!/usr/bin/env bash
# Kurisu Google setup helper - prints (and optionally executes) ONLY the
# scriptable steps. It never reads credentials, never lists projects, never
# creates OAuth clients (no public API exists for that) and never changes
# console-only configuration.
#
#   ./scripts/kurisu-google-setup.sh --project kurisu-abc123            # dry run
#   ./scripts/kurisu-google-setup.sh --project kurisu-abc123 --execute  # for real
#
# The console-only steps are printed at the end for you to do by hand:
#   * OAuth consent screen (External, Testing, test users)
#   * OAuth client of type "TVs and Limited Input devices"
set -euo pipefail

PROJECT=""
EXECUTE=0
FIREBASE_VERSION="15"

usage() {
  sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --project) PROJECT="${2:-}"; shift 2 ;;
    --execute) EXECUTE=1; shift ;;
    -h|--help) usage ;;
    *) echo "unknown argument: $1" >&2; usage ;;
  esac
done

if [ -z "$PROJECT" ]; then
  echo "error: --project <globally-unique-id> is required" >&2
  exit 2
fi
if ! printf '%s' "$PROJECT" | grep -Eq '^[a-z][a-z0-9-]{4,28}[a-z0-9]$'; then
  echo "error: '$PROJECT' is not a valid GCP project id (lowercase, 6-30 chars, may contain '-')" >&2
  exit 2
fi

run() {
  echo "+ $*"
  if [ "$EXECUTE" -eq 1 ]; then
    "$@"
  fi
}

echo "== Kurisu Google setup =="
echo "project: $PROJECT"
if [ "$EXECUTE" -eq 0 ]; then
  echo "mode: DRY RUN (add --execute to actually run the commands)"
fi
echo

echo "-- 1. project (scriptable, Firebase Management API) --"
run npx --yes "firebase-tools@${FIREBASE_VERSION}" projects:create "$PROJECT" --display-name Kurisu
echo

echo "-- 2. APIs (scriptable, needs gcloud) --"
if command -v gcloud >/dev/null 2>&1; then
  run gcloud config set project "$PROJECT"
  run gcloud services enable oauth2.googleapis.com openidconnect.googleapis.com
else
  echo "gcloud not found on PATH - run these on a machine that has it:"
  echo "  gcloud config set project $PROJECT"
  echo "  gcloud services enable oauth2.googleapis.com openidconnect.googleapis.com"
fi
echo

cat <<'CONSOLE'
-- 3. CONSOLE ONLY (no public API exists for either) --

  a) APIs & Services -> OAuth consent screen
       User type: External · App name: Kurisu · Publishing status: Testing
       Scopes: openid, .../auth/userinfo.email, .../auth/userinfo.profile
       Test users: add the friend's Google address

  b) APIs & Services -> Credentials -> Create credentials -> OAuth client ID
       Application type: "TVs and Limited Input devices"   <-- required
       Name: Kurisu broker
       No redirect URI is needed or requested.

    Do NOT use an IAP client (gcloud alpha iap oauth-clients) or a Firebase web
    client: neither can run the device authorization grant.

-- 4. Verify the client type (safe, read-only for config) --

  curl -sS https://oauth2.googleapis.com/device/code \
    -d "client_id=<client id>" -d "scope=openid email profile"

  Expect device_code/user_code/verification_url. invalid_client means the wrong
  client type.

-- 5. Broker environment (never in git, never in the guest) --

  export KURISU_GOOGLE_CLIENT_SECRET='<client secret>'
  then set auth.google_client_id in config/broker.local.yaml and run
  `guest-broker preflight --config config/broker.local.yaml`
CONSOLE

if [ "$EXECUTE" -eq 0 ]; then
  echo
  echo "Nothing was executed. Re-run with --execute to perform steps 1-2."
fi
