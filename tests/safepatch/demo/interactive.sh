#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
mode="${1:-}"
case "$mode" in base|compatible|cooperative) ;; *) echo '用法：bash tests/safepatch/demo/interactive.sh base|compatible|cooperative [任务毫秒=30000] [产物根目录] [等待上限毫秒]'; exit 2;; esac
task_ms="${2:-30000}"
[[ "$task_ms" =~ ^[1-9][0-9]*$ && ${#task_ms} -le 7 ]] || { echo '任务时长须为 1..3600000 毫秒'; exit 2; }
(( task_ms <= 3600000 )) || { echo '任务时长过大'; exit 2; }
out="${3:-/tmp/gaia-document-interactive}"
timeout_ms="${4:-$((task_ms + 10000))}"
[[ "$timeout_ms" =~ ^[1-9][0-9]*$ && ${#timeout_ms} -le 8 ]] || { echo '等待上限须为正整数毫秒'; exit 2; }
[[ "$(uname -s)" == Linux && "$(uname -m)" == x86_64 ]] || { echo '需要 Linux x86_64'; exit 2; }
mkdir -p "$out"
out="$(cd "$out" && pwd)"
session="$(mktemp -d "$out/$mode-XXXXXXXX")"
# Same business source and duration for each mode; only its existing optional ABI differs.
flags=()
if [[ "$mode" == cooperative ]]; then flags+=(-DCOOPERATIVE); fi
cc -O2 -g -pthread -fno-pie -no-pie -fno-inline "-DDEMO_TASK_MS=$task_ms" "${flags[@]}" demo/target.c -o "$session/service"
if ! cargo build --offline --bin safepatch-interactive-demo >"$session/build.log" 2>&1; then echo "构建失败"; exit 1; fi
# Publish an atomic pointer for the independent monitor, without printing paths.
link_tmp="$session/current-link"
ln -s "$session/files" "$link_tmp"
mv -Tf "$link_tmp" "$out/current-$mode"
# Runtime's existing raw stderr is retained separately; UI shows timestamped results
# and actual observer/memory samples. stdin remains the user's terminal.
# Restore terminal echo even when Ctrl-C terminates the Rust UI before Drop.
saved_tty=""
if [[ -t 0 ]]; then saved_tty="$(stty -g)"; fi
restore_terminal() { if [[ -n "$saved_tty" ]]; then stty "$saved_tty" 2>/dev/null || true; fi; }
trap restore_terminal EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
target/debug/safepatch-interactive-demo "$session/service" "$mode" "$session" "$task_ms" "$timeout_ms" 2>"$session/runtime.log"
