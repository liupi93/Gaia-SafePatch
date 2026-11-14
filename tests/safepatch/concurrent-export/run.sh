#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
out="${1:-$(mktemp -d /tmp/gaia-concurrent-export-XXXXXXXX)}"
scenario="${2:-all}"
mode="${3:-all}"
case "$scenario" in all) scenarios=(async aligned);; async|aligned) scenarios=("$scenario");; *) echo 'scenario: all|async|aligned';exit 2;; esac
case "$mode" in all) modes=(base compatible cooperative);; base|compatible|cooperative) modes=("$mode");; *) echo 'mode: all|base|compatible|cooperative';exit 2;; esac
mkdir -p "$out"
cargo build --offline --bin safepatch-concurrent-export >"$out/build.log" 2>&1 || { cat "$out/build.log";exit 1; }
for business in "${scenarios[@]}"; do
 dir="$out/$business";mkdir -p "$dir"
 flags=();if [[ "$business" == aligned ]];then flags+=(-DALIGNED_BOUNDARY);fi
 cc -O2 -g -pthread -fno-pie -no-pie -fno-inline -fno-optimize-sibling-calls "${flags[@]}" concurrent-export/target.c -o "$dir/plain"
 cc -O2 -g -pthread -fno-pie -no-pie -fno-inline -fno-optimize-sibling-calls "${flags[@]}" -DCOOPERATIVE concurrent-export/target.c -o "$dir/cooperative-bin"
 for path in "${modes[@]}"; do
  exe="$dir/plain";if [[ "$path" == cooperative ]];then exe="$dir/cooperative-bin";fi
  target/debug/safepatch-concurrent-export "$exe" "$path" "$dir/$path" "$business" 2>"$dir/$path.runtime"
 done
done
printf '证据目录：%s\n' "$out"
