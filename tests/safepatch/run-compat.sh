#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
output="${1:-/tmp/gaia-safepatch-compatible}"
mkdir -p "$output"
cc -O0 -g -pthread -fno-pie -no-pie compatible.c -o "$output/compatible"
cc -O2 -g -pthread -fno-pie -no-pie -fno-inline demo/target.c -o "$output/document"
cargo build --offline --bin safepatch-compat-validation
target/debug/safepatch-compat-validation "$output/compatible" >"$output/summary" 2>"$output/runtime"
GAIA_COMPAT_TARGET="$output/compatible" GAIA_DOCUMENT_TARGET="$output/document" cargo test --offline --bin safepatch-compat-validation -- --nocapture >"$output/fault-tests.log" 2>&1
cat "$output/summary"
tail -3 "$output/fault-tests.log"
