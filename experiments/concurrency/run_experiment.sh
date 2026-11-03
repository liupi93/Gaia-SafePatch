#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
BUILD_DIR="${SCRIPT_DIR}/build"
TARGET="${BUILD_DIR}/gaia-consistency-demo"
LOG_FILE="${BUILD_DIR}/experiment.log"
GAIA_API="${GAIA_API:-http://127.0.0.1:17890}"
THREADS="${THREADS:-8}"
READY_FILE="/tmp/gaia-consistency.ready"
CONTINUE_FILE="/tmp/gaia-consistency.continue"
for command in gcc curl jq nm; do command -v "${command}" >/dev/null || { echo "missing: ${command}" >&2; exit 1; }; done
curl -fsS "${GAIA_API}/api/v1/state" >/dev/null || { echo "Gaia API unavailable: ${GAIA_API}" >&2; exit 1; }
mkdir -p "${BUILD_DIR}"
rm -f "${READY_FILE}" "${CONTINUE_FILE}" "${LOG_FILE}"
gcc -std=c11 -O0 -g -fno-omit-frame-pointer -fno-pie -no-pie -pthread -Wall -Wextra -Werror \
  "${SCRIPT_DIR}/gaia_consistency_demo.c" -o "${TARGET}"
nm -C "${TARGET}" | grep -q ' T pricing_rule$' || { echo "pricing_rule symbol missing" >&2; exit 1; }
"${TARGET}" "${THREADS}" >"${LOG_FILE}" 2>&1 & TARGET_PID=$!
PATCH_INDEX=""
cleanup() {
  touch "${CONTINUE_FILE}" 2>/dev/null || true
  wait "${TARGET_PID}" 2>/dev/null || true
  if [[ -n "${PATCH_INDEX}" ]]; then
    curl -fsS -X DELETE "${GAIA_API}/api/v1/config/hotpatch/${PATCH_INDEX}" >/dev/null || true
  fi
}
trap cleanup EXIT
for _ in $(seq 1 300); do
  [[ -f "${READY_FILE}" ]] && break
  kill -0 "${TARGET_PID}" 2>/dev/null || { cat "${LOG_FILE}" >&2; exit 1; }
  sleep 0.02
done
[[ -f "${READY_FILE}" ]] || { echo "target did not reach barrier" >&2; exit 1; }
echo "Target PID ${TARGET_PID}: ${THREADS} requests in flight; applying Gaia patch"
PATCH_RESPONSE="$({
  printf '{"binary":"%s","symbol":"pricing_rule","pid":%s,' "${TARGET}" "${TARGET_PID}"
  printf '"enabled":true,"patch_action":"override_return","override_return_value":200}'
} | curl -fsS -H 'Content-Type: application/json' --data-binary @- "${GAIA_API}/api/v1/config/hotpatch")"
PATCH_INDEX="$(jq 'length - 1' <<<"${PATCH_RESPONSE}")"
[[ "$(jq -r ".[${PATCH_INDEX}].pid" <<<"${PATCH_RESPONSE}")" == "${TARGET_PID}" ]] || { echo "unexpected API response" >&2; exit 1; }
touch "${CONTINUE_FILE}"; wait "${TARGET_PID}"
MIXED_COUNT="$(grep -c 'classification=MIXED' "${LOG_FILE}" || true)"
RESULT_COUNT="$(grep -c '^RESULT ' "${LOG_FILE}" || true)"
cat "${LOG_FILE}"; printf 'SUMMARY total=%s mixed=%s\n' "${RESULT_COUNT}" "${MIXED_COUNT}"
[[ "${RESULT_COUNT}" -eq "${THREADS}" && "${MIXED_COUNT}" -gt 0 ]] || { echo "mixed-version request not reproduced" >&2; exit 1; }
curl -fsS -X DELETE "${GAIA_API}/api/v1/config/hotpatch/${PATCH_INDEX}" >/dev/null || \
  echo "WARNING: failed to remove persisted Gaia target at index ${PATCH_INDEX}" >&2
PATCH_INDEX=""
trap - EXIT
echo "PASS: an in-flight request observed old and new function semantics."
