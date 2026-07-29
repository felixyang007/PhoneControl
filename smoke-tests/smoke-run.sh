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

# State the finalize trap reads. MAESTRO_RC starts at 1 so an interrupt before
# the flow finishes counts as failure (blocks CI) rather than a false pass.
CAPTURE_STARTED=0
MAESTRO_RC=1

# Exception safety: whatever happens after acquire — assertion failure, a
# CI timeout SIGTERM, Ctrl-C — capture/stop, report, and release MUST run so
# artifacts are finalised and the device is returned (not left for the TTL
# sweeper). This trap is the single place steps 5/6/7 happen.
finalize() {
  trap - EXIT INT TERM
  if [[ "$CAPTURE_STARTED" == "1" ]]; then
    echo "▸ capture stop:"
    api -X POST "$API/api/v1/capture/stop" -d "$(printf '{"serial":"%s"}' "$SERIAL")" || true; echo
    echo "▸ report (exit_code=$MAESTRO_RC):"
    api "$API/api/v1/smoke/report?task_id=$TASK&exit_code=$MAESTRO_RC" || true; echo
  fi
  api -X POST "$API/api/v1/devices/release" -d "$(printf '{"task_id":"%s"}' "$TASK")" >/dev/null 2>&1 || true
  echo "▸ done · exit=$MAESTRO_RC"
  exit "$MAESTRO_RC"
}

# 1) acquire a device (any idle, or the requested serial)
if [[ -n "$SERIAL" ]]; then body=$(printf '{"task_id":"%s","serial":"%s"}' "$TASK" "$SERIAL")
else                        body=$(printf '{"task_id":"%s"}' "$TASK"); fi
acq=$(api -X POST "$API/api/v1/devices/acquire" -d "$body")
SERIAL=$(printf '%s' "$acq" | jstr serial)
[[ -n "$SERIAL" ]] || die "acquire failed: $acq"
echo "▸ acquired $SERIAL"

# Arm finalize only AFTER a successful acquire (nothing to clean up before it).
trap finalize EXIT INT TERM

# 2) install the APK (optional)
if [[ -n "$APK" ]]; then
  echo "▸ installing $APK"
  api -X POST "$API/api/v1/install" -d "$(printf '{"task_id":"%s","apk_path":"%s"}' "$TASK" "$APK")"; echo
fi

# 3) start capture (screen recording + logcat)
if [[ -n "$OUTPUT_DIR" ]]; then start=$(printf '{"task_id":"%s","serial":"%s","output_dir":"%s"}' "$TASK" "$SERIAL" "$OUTPUT_DIR")
else                           start=$(printf '{"task_id":"%s","serial":"%s"}' "$TASK" "$SERIAL"); fi
api -X POST "$API/api/v1/capture/start" -d "$start" >/dev/null
CAPTURE_STARTED=1
echo "▸ capture started"

# 4) run the Maestro flow (phone-control does NOT drive the UI — decision #3).
#    finalize (trap) runs on the way out — steps 5/6/7 happen there.
if command -v maestro >/dev/null 2>&1; then
  echo "▸ maestro test --device $SERIAL $FLOW"
  maestro test --device "$SERIAL" "$FLOW" && MAESTRO_RC=0 || MAESTRO_RC=$?
else
  echo "▸ WARN: maestro not installed — UI flow SKIPPED."
  echo "        install: curl -Ls https://get.maestro.mobile.dev | bash"
  MAESTRO_RC=127
fi
# fall through → EXIT trap → finalize()
