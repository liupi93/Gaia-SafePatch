#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
out="${1:-/tmp/gaia-video-demo}"
mkdir -p "$out"
echo "【准备】编译演示程序（普通版本）"
cc -O0 -g -pthread -fno-pie -no-pie demo/target.c -o "$out/plain"
echo "【准备】编译演示程序（协作版本）"
cc -O0 -g -pthread -fno-pie -no-pie -DCOOPERATIVE demo/target.c -o "$out/cooperative"
echo "【准备】使用仓库配置的 Rust 工具链构建驱动"
if ! cargo build --offline --bin safepatch-video-demo >"$out/build.log" 2>&1; then
 echo "【构建失败】以下为编译器原始诊断："
 cat "$out/build.log"
 exit 1
fi
objdump -d "$out/plain" > "$out/disassembly.txt"
for mode in base compatible; do
 bash demo/show.sh entry "$mode" "$out" | tee "$out/entry-$mode.log"
done
for mode in base compatible cooperative; do
 bash demo/show.sh orders "$mode" "$out" | tee "$out/orders-$mode.log"
done
