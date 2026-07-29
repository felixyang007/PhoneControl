#!/usr/bin/env bash
# Phase 3 orchestration — AI root-cause triage on a failed smoke run.
#
# Reads a <task>-manifest.json (the bridge phone-control writes), pulls crash /
# network signals out of the logcat, and asks Claude to classify the failure
# (A crash / B env / C flaky-UI) and — with --create — file a Linear bug via the
# Linear MCP with the analysis + artifact paths attached.
#
# This is the CI/orchestration layer: it lives OUTSIDE the phone-control app on
# purpose (credentials + integrations don't belong in a GUI). smoke-run.sh calls
# it on a non-zero exit; you can also run it standalone against any manifest.
#
# Usage:
#   smoke-triage.sh --manifest ~/.phone_control/recordings/BUILD-1-manifest.json
#   smoke-triage.sh --manifest <path> --create        # also file a Linear bug
#
# Prereqs: jq; claude CLI (for the AI step); for --create, the Linear MCP must be
# configured on this machine (see docs/smoke-test-integration.md §0/§1).
set -uo pipefail

MANIFEST="" ; MODE="dry-run" ; TAIL=200
die() { echo "ERROR: $*" >&2; exit 2; }
while [[ $# -gt 0 ]]; do
  case "$1" in
    --manifest) MANIFEST="$2"; shift 2;;
    --create)   MODE="create"; shift;;
    --dry-run)  MODE="dry-run"; shift;;
    --tail)     TAIL="$2"; shift 2;;
    -h|--help)  sed -n '2,20p' "$0"; exit 0;;
    *) die "unknown arg: $1";;
  esac
done
[[ -n "$MANIFEST" && -f "$MANIFEST" ]] || die "--manifest <path> required (file not found)"
command -v jq >/dev/null || die "jq required"

exit_code=$(jq -r '.exit_code // "null"' "$MANIFEST")
task=$(jq -r '.task_id // "?"' "$MANIFEST")
logcat=$(jq -r '.artifacts[0].logcat_path // empty' "$MANIFEST")
mp4=$(jq -r '.artifacts[0].mp4_path // empty' "$MANIFEST")
model=$(jq -r '.artifacts[0].device_info.model // "?"' "$MANIFEST")
os=$(jq -r '.artifacts[0].device_info.os_version // "?"' "$MANIFEST")

if [[ "$exit_code" == "0" ]]; then
  echo "▸ task=$task passed (exit_code=0) — nothing to triage."
  exit 0
fi
echo "▸ triaging task=$task (exit_code=$exit_code, device=$model/Android $os)"

# ── Heuristic signal extraction (fast path + fallback if Claude is absent) ──────
crash="" ; net=""
if [[ -n "$logcat" && -f "$logcat" ]]; then
  crash=$(grep -aE "FATAL EXCEPTION|E +AndroidRuntime|NullPointerException|signal 11 \(SIGSEGV\)|ANR in|Tombstone" "$logcat" | head -40)
  net=$(grep -aiE "Unable to resolve host|ECONNREFUSED|Connection refused|SocketTimeout|timeout|Failed to connect|UnknownHost" "$logcat" | head -20)
fi
if   [[ -n "$crash" ]]; then hclass="[A] 代码崩溃"
elif [[ -n "$net"   ]]; then hclass="[B] 环境/网络"
else                        hclass="[C] 用例失效/UI 改版"; fi
echo "▸ heuristic class: $hclass"

context=$(cat <<EOF
task_id: $task
exit_code: $exit_code
device: $model (Android $os)
heuristic_class: $hclass
--- crash signals ---
${crash:-(none)}
--- network/env signals ---
${net:-(none)}
--- logcat tail (last $TAIL lines) ---
$( [[ -n "$logcat" && -f "$logcat" ]] && tail -n "$TAIL" "$logcat" || echo "(no logcat)")
--- artifacts ---
mp4:    ${mp4:-(none)}
logcat: ${logcat:-(none)}
EOF
)

# ── AI triage via headless Claude Code ─────────────────────────────────────────
# Skip the LLM when it's unavailable or explicitly disabled (SMOKE_TRIAGE_NO_LLM=1),
# emitting the heuristic classification + extracted signals so CI still gets a result.
if [[ -n "${SMOKE_TRIAGE_NO_LLM:-}" ]] || ! command -v claude >/dev/null 2>&1; then
  echo "▸ LLM step skipped — heuristic result:"
  printf '%s\n' "$context"
  exit 0
fi

prompt="$(cat "$(dirname "$0")/triage-prompt.md")"
if [[ "$MODE" == "create" ]]; then
  action="ACTION: after the analysis, create a Linear bug via the Linear MCP.
Title: \"[Smoke] \$classification — \$task ($model)\". Put the full analysis in the
description, attach the mp4 and logcat paths, and apply the Android side label.
Only file for class [A]; for [B]/[C] print the analysis and say why no ticket."
else
  action="ACTION: dry-run — print the analysis only. Do NOT create anything."
fi

printf '%s\n\n%s\n\nCONTEXT:\n%s\n' "$prompt" "$action" "$context" | claude -p
