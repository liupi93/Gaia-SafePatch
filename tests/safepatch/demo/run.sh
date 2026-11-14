#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ "$(uname -s)" != Linux || "$(uname -m)" != x86_64 ]]; then
    echo "本演示仅适用于 Linux x86_64。"
    exit 1
fi
out="${1:-/tmp/gaia-document-demo}"
mkdir -p "$out"
echo "【准备】编译纯 C 文档服务（普通版本与协作版本）"
cc -O2 -g -pthread -fno-pie -no-pie -fno-inline demo/target.c -o "$out/plain"
cc -O2 -g -pthread -fno-pie -no-pie -fno-inline -DCOOPERATIVE demo/target.c -o "$out/cooperative"
echo "【准备】使用仓库配置的 Rust 工具链构建演示驱动"
if ! cargo build --offline --bin safepatch-video-demo >"$out/build.log" 2>&1; then
    cat "$out/build.log"
    exit 1
fi
objdump -d "$out/plain" > "$out/disassembly.txt"
for mode in control base compatible cooperative; do
    bash demo/show.sh "$mode" "$out" | tee "$out/$mode.log"
done
