#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
output="${1:-/tmp/gaia-safepatch-compatible}"
mkdir -p "$output"
cc -O0 -g -pthread -fno-pie -no-pie compatible.c -o "$output/compatible"
cc -O0 -g -fno-omit-frame-pointer -fno-optimize-sibling-calls -fno-inline -fno-pie -no-pie -fno-stack-protector -falign-functions=1 -nostdlib -Wl,--build-id=none restricted.c -o "$output/restricted"
cc -O0 -g -fomit-frame-pointer -fno-inline -fno-pie -no-pie -fno-stack-protector -falign-functions=1 -nostdlib -Wl,--build-id=none restricted.c -o "$output/no-frame"
cc -O0 -g -fno-omit-frame-pointer -fno-inline -fno-pie -no-pie -fno-stack-protector -falign-functions=1 -nostdlib -Wl,--build-id=none -DBAD_SYSCALL restricted.c -o "$output/bad-syscall"
cargo build --offline --manifest-path Cargo.toml --bin safepatch-compat-validation
target/debug/safepatch-compat-validation "$output/compatible" >"$output/summary" 2>"$output/runtime"
python3 - "$output/runtime" <<'PY'
import sys
lines=[s for s in open(sys.argv[1]) if 'compatible result=' in s]
assert len(lines)==72, len(lines)
assert all('result=error' in s and 'checks=0' in s and
           'code_installed=false' in s and 'consistency_complete=false' in s for s in lines)
print('72 refusals; no installation or consistency success reported')
PY
cat "$output/summary"
target/debug/safepatch-compat-validation "$output/restricted" restricted >"$output/restricted.summary" 2>"$output/restricted.runtime"
cat "$output/restricted.summary"
for target in no-frame bad-syscall; do
    target/debug/safepatch-compat-validation "$output/$target" reject-static >"$output/$target.summary" 2>"$output/$target.runtime"
    cat "$output/$target.summary"
done
python3 - "$output/restricted.runtime" <<'PYTEST'
import sys,re
log=open(sys.argv[1]).read()
successes=[s for s in log.splitlines() if 'result=success' in s]
assert len(successes)==8, len(successes)
assert all(int(re.search(r'checks=(\d+)',s)[1])>0 for s in successes)
assert 'result=busy active=target_value depth=1' in log
assert 'result=busy active=main' in log
assert 'root=verified' in log and 'full_functions=true' in log
assert 'business_consistency=unclaimed' in log
print('8 safe writes with checked root-closed stacks; active old and related calls refused/retried')
PYTEST

GAIA_COMPAT_TARGET="$output/compatible" GAIA_RESTRICTED_TARGET="$output/restricted" cargo test --offline --manifest-path Cargo.toml --bin safepatch-compat-validation -- --nocapture 2>&1 | tee "$output/fault-tests.log"
