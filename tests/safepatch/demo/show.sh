#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mode="${1:?请选择：control（正常对照）、base（原版）、compatible（兼容）或 cooperative（协作）}"
out="${2:-/tmp/gaia-document-demo}"
exe="$out/plain"
if [[ "$mode" == cooperative ]]; then
    exe="$out/cooperative"
fi
# 每次演示保留独立的真实文件，避免与旧结果混淆。
result="$(mktemp -d "$out/$mode-XXXXXX")"
result="$result/files"
target/debug/safepatch-video-demo "$exe" "$mode" "$result" 2>&1 | python3 -u demo/display.py
