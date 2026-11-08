#!/usr/bin/env bash
# 从仓库中任意目录运行：bash .../show.sh entry|orders base|compatible|cooperative [产物目录]
set -euo pipefail
cd "$(dirname "$0")/.."
scene="${1:?请指定场景：entry（入口冲突）或 orders（订单结算）}"
mode="${2:?请指定模式：base（原版）、compatible（兼容）或 cooperative（协作）}"
out="${3:-/tmp/gaia-video-demo}"
exe="$out/plain"
if [[ "$mode" == cooperative ]]; then exe="$out/cooperative"; fi
# 保留驱动的退出码：中文显示不会把测试失败变成成功。
target/debug/safepatch-video-demo "$exe" "$scene" "$mode" 2>&1 | python3 -u demo/display.py
