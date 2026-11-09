#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
business="${1:?async|aligned}"
mode="${2:?base|compatible|cooperative}"
case "$business" in async|aligned) ;; *) exit 2;; esac
case "$mode" in base|compatible|cooperative) ;; *) exit 2;; esac
layout="${3:-3000}";print_ms="${4:-2000}"
for number in "$layout" "$print_ms";do [[ "$number" =~ ^[1-9][0-9]*$ && ${#number} -le 6 ]] || exit 2;done
root="${GAIA_CASE_ROOT:-/tmp/gaia-3x2-interactive}"
mkdir -p "$root"
root="$(cd "$root" && pwd)"
out="${GAIA_CASE_OUTPUT_DIR:-$(mktemp -d "$root/$business-$mode-XXXXXXXX")}"
mkdir -p "$out"
flags=();if [[ "$business" == aligned ]];then flags+=(-DALIGNED_BOUNDARY);fi
if [[ "$mode" == cooperative ]];then flags+=(-DCOOPERATIVE);fi
cc -O2 -g -pthread -fno-pie -no-pie -fno-inline -fno-optimize-sibling-calls "${flags[@]}" "-DLAYOUT_MS=$layout" "-DPRINT_MS=$print_ms" concurrent-export/target.c -o "$out/service"
cargo build --offline --bin safepatch-concurrent-export >"$out/build.log" 2>&1 || exit 1
# Current-session pointers support both the matrix monitor and the legacy mode monitor.
for name in current "current-$mode" "current-$business-$mode"; do
 ln -s "$out/files" "$out/.link-$name"
 mv -Tf "$out/.link-$name" "$root/$name"
done
saved_tty="";if [[ -t 0 ]];then saved_tty="$(stty -g)";fi
cleanup(){ if [[ -n "$saved_tty" ]];then stty "$saved_tty" 2>/dev/null || true;fi; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export GAIA_CASE_TIMEOUT_MS=$((layout+print_ms+10000))
target/debug/safepatch-concurrent-export "$out/service" "$mode" "$out/files" "$business" interactive 2>"$out/runtime.log"
