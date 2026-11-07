#!/usr/bin/env bash
set -euo pipefail
umask 077
tool_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
repo="$(git -C "$tool_dir" rev-parse --show-toplevel)"
base=eec1660a4d6679fb3ce687b757a25578a6abd326
main=be3dd66ecc1aa4479a490df8cfb8caefa9c2c836
export RUSTUP_AUTO_INSTALL=0 CARGO_NET_OFFLINE=true CI=true
unset AYA_BUILD_SKIP RUSTUP_TOOLCHAIN
test "$(id -u)" -eq 0
test "$(uname -m)" = x86_64
git -C "$repo" cat-file -e "$base^{commit}"
git -C "$repo" cat-file -e "$main^{commit}"
work=$(mktemp -d "${GAIA_PERF_WORK_ROOT:-/root}/gaia-formal.XXXXXX")
printf 'GAIA_FORMAL_DIR=%s\n' "$work"
git -C "$repo" worktree add --detach "$work/base" "$base"
git -C "$repo" worktree add --detach "$work/main" "$main"
cmp "$work/base/Cargo.lock" "$work/main/Cargo.lock"
cmp "$work/base/gaia-webui/pnpm-lock.yaml" "$work/main/gaia-webui/pnpm-lock.yaml"
for variant in base main; do
    (
        cd "$work/$variant"
        python3 "$tool_dir/formal/instrument.py" "$PWD"
        git diff -- gaia-xdp/src/main.rs gaia-xdp/src/web.rs gaia-xdp/src/safepatch.rs > "$work/$variant-instrumentation.diff"
        rustc -Vv
        cargo -V
        CARGO_TARGET_DIR="$PWD/target" cargo build --release --locked -p gaia-xdp
        git diff --exit-code -- Cargo.lock gaia-webui/pnpm-lock.yaml
        test -x target/release/gaia-xdp
    ) 2>&1 | tee "$work/$variant-release.log"
done
mkdir "$work/collector" "$work/collector/templates"
for name in runner.py adapters.py analyze.py common.py resources.py environment.py config.json; do
    cp -- "$tool_dir/$name" "$work/collector/$name"
done
cp -- "$tool_dir/templates/monitor.toml" "$work/collector/templates/monitor.toml"
cp -- "$tool_dir/formal/workload.c" "$work/collector/workload.c"
cp -- "$tool_dir/formal/collect.sh" "$work/collector/collect.sh"
export GAIA_FORMAL_DIR="$work"
bash "$work/collector/collect.sh"
