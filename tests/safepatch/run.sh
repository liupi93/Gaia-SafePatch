#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
output="${1:-/tmp/gaia-safepatch-results}"
mkdir -p "$output"
for test in high_frequency long_running; do
    cc -O0 -g -pthread -fno-pie -no-pie -fno-inline "$test.c" -o "$output/$test"
done
cc -O0 -g -pthread -fno-pie -no-pie -fno-inline -DGAIA_BASELINE high_frequency.c -o "$output/high_frequency_base"
cargo build --manifest-path Cargo.toml
for test in high_frequency long_running; do
    target/debug/safepatch-validation "$output/$test" "$output/$test.calls" >"$output/$test.summary" 2>"$output/$test.runtime"
done
target/debug/safepatch-validation "$output/long_running" "$output/base-risk.calls" risk >"$output/base-risk.summary" 2>"$output/base-risk.runtime"
target/debug/safepatch-validation "$output/long_running" "$output/compatible-risk.calls" compat-risk >"$output/compatible-risk.summary" 2>"$output/compatible-risk.runtime"
for sample in 1 2 3; do
    for mode in base safe; do
        executable="$output/high_frequency"
        if [[ "$mode" == base ]]; then executable="$output/high_frequency_base"; fi
        target/debug/safepatch-validation "$executable" "$output/perf-$mode-$sample.calls" "$mode" >"$output/perf-$mode-$sample.summary" 2>"$output/perf-$mode-$sample.runtime"
    done
done
git rev-parse base >"$output/base-commit"
bash run-compat.sh "$output/compatible" >"$output/compatible-validation.summary" 2>"$output/compatible-validation.runtime"
python3 analyze.py "$output"
