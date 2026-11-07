#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
output="${1:-/tmp/gaia-safepatch-compatible}"
mkdir -p "$output"
cc -O0 -g -pthread -fno-pie -no-pie compatible.c -o "$output/compatible"
cargo build --offline --manifest-path Cargo.toml --bin safepatch-compat-validation
target/debug/safepatch-compat-validation "$output/compatible" >"$output/summary" 2>"$output/runtime"
python3 - "$output/runtime" <<'PY'
import sys
lines=[s for s in open(sys.argv[1]) if 'compatible write verified' in s]
assert len(lines)==40, len(lines)
assert all('threads=9 rip_clear=true' in s for s in lines)
print('40 verified writes, all 9 threads stopped and RIP clear')
PY
cat "$output/summary"

GAIA_COMPAT_TARGET="$output/compatible" cargo test --offline --manifest-path Cargo.toml --bin safepatch-compat-validation -- --nocapture
