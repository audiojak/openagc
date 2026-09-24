#!/usr/bin/env bash
# Run scripts/check.sh, print its tail, and exit with ITS status (a pipe to
# tail would hide failures). Use: scripts/gate.sh && git commit ...
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG="$ROOT/build/check.log"
mkdir -p "$ROOT/build"
"$ROOT/scripts/check.sh" >"$LOG" 2>&1
status=$?
tail -6 "$LOG"
[[ $status -eq 0 ]] || echo "check failed ($status); full log: build/check.log"
exit $status
