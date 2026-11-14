#!/usr/bin/env bash
set -euo pipefail
root=/tmp/gaia-3x2-interactive
if [[ $# -gt 0 && "$1" != --* ]]; then root="$1"; shift; fi
exec python3 "$(dirname "$0")/../demo/monitor.py" base "$root" --current "$@"
