#!/usr/bin/env bash
# Smoke-test orchestrator — the "Jenkins shell" role from the integration design.
#
# Chains the full run against a phone-control control API:
#   acquire → [install] → capture/start → maestro test → capture/stop → report → release
#
# phone-control owns device scheduling + install + capture; Maestro drives the
# UI (decision #3). Exit code is Maestro's, so CI passes/fails on the flow.
#
# Usage:
#   smoke-run.sh --flow settings-smoke.yaml            # any idle device
#   smoke-run.sh --flow app-smoke.yaml --serial emulator-5556 --apk ./app.apk
#   smoke-run.sh --flow app-smoke.yaml --task build-1234 --output-dir ./artifacts
#
# Env: SMOKE_API (default http://127.0.0.1:9090), token from
#      ~/.phone_control/api_token (or $PHONE_CONTROL_TOKEN).
set -uo pipefail

API="${SMOKE_API:-http://127.0.0.1:9090}"
TOKEN="${PHONE_CONTROL_TOKEN:-$(cat "$HOME/.phone_control/api_token" 2>/dev/null || true)}"
TASK="smoke-$$-$(date +%s)"
FLOW="" ; APK="" ; SERIAL="" ; OUTPUT_DIR=""

die() { echo "ERROR: $*" >&2; exit 2; }
while [[ $# -gt 0 ]]; do
  case "$1" in
    --flow)       FLOW="$2"; shift 2;;
    --apk)        APK="$2"; shift 2;;
    --serial)     SERIAL="$2"; shift 2;;
    --task)       TASK="$2"; shift 2;;
    --output-dir) OUTPUT_DIR="$2"; shift 2;;
    --api)        API="$2"; shift 2;;
    -h|--help)    sed -n '2,20p' "$0"; exit 0;;
    *) die "unknown arg: $1";;
  esac
done
[[ -n "$FLOW" ]]  || die "--flow <maestro.yaml> is required"
[[ -n "$TOKEN" ]] || die "no API token (start phone-control, or set PHONE_CONTROL_TOKEN)"

AUTH=(-H "authorization: Bearer $TOKEN" -H 'content-type: application/json')
api()  { curl -sS "${AUTH[@]}" "$@"; }
# crude JSON string field extractor (avoids a jq dependency)
jstr() { sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p"; }

echo "▸ task=$TASK  api=$API  flow=$FLOW"

# 1) acquire a device (any idle, or the requested serial)
if [[ -n "$SERIAL" ]]; then body=$(printf '{"task_id":"%s","serial":"%s"}' "$TASK" "$SERIAL")
else                        body=$(printf '{"task_id":"%s"}' "$TASK"); fi
acq=$(api -X POST "$API/api/v1/devices/acquire" -d "$body")
SERIAL=$(printf '%s' "$acq" | jstr serial)
[[ -n "$SERIAL" ]] || die "acquire failed: $acq"
echo "▸ acquired $SERIAL"

# release on any exit (belt-and-braces; the TTL sweeper is the backstop)
cleanup() { api -X POST "$API/api/v1/devices/release" -d "$(printf '{"task_id":"%s"}' "$TASK")" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# 2) install the APK (optional)
if [[ -n "$APK" ]]; then
  echo "▸ installing $APK"
  api -X POST "$API/api/v1/install" -d "$(printf '{"task_id":"%s","apk_path":"%s"}' "$TASK" "$APK")"; echo
fi

# 3) start capture (screen recording + logcat)
if [[ -n "$OUTPUT_DIR" ]]; then start=$(printf '{"task_id":"%s","serial":"%s","output_dir":"%s"}' "$TASK" "$SERIAL" "$OUTPUT_DIR")
else                           start=$(printf '{"task_id":"%s","serial":"%s"}' "$TASK" "$SERIAL"); fi
api -X POST "$API/api/v1/capture/start" -d "$start" >/dev/null
echo "▸ capture started"

# 4) run the Maestro flow (phone-control does NOT drive the UI — decision #3)
rc=0
if command -v maestro >/dev/null 2>&1; then
  echo "▸ maestro test --device $SERIAL $FLOW"
  maestro test --device "$SERIAL" "$FLOW" || rc=$?
else
  echo "▸ WARN: maestro not installed — UI flow SKIPPED."
  echo "        install: curl -Ls https://get.maestro.mobile.dev | bash"
  rc=127
fi

# 5) stop capture + 6) fetch the artifact report (always, even on failure)
echo "▸ capture stop:"; api -X POST "$API/api/v1/capture/stop" -d "$(printf '{"serial":"%s"}' "$SERIAL")"; echo
echo "▸ report:";       api "$API/api/v1/smoke/report?task_id=$TASK"; echo

echo "▸ maestro exit=$rc"
exit "$rc"
