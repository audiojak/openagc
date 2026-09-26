#!/usr/bin/env bash
# Capture a snapshot of the Debug app against the demo mailbox in a
# throwaway data directory, never the user's accounts or Keychain items.
# Usage: scripts/snapshot.sh out.png [extra -OpenAGCSnapshot* args...]
# SNAPSHOT_LOG=file keeps the app's stderr (the -OpenAGCSnapshotDumpViews tree).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
shift
APP="$ROOT/build/DerivedData/Build/Products/Debug/OpenAGC.app/Contents/MacOS/OpenAGC"
DATA="$(mktemp -d -t openagc-snapshot)"
trap 'rm -rf "$DATA"' EXIT
"$APP" -OpenAGCDataDirectory "$DATA" -OpenAGCDemo YES -OpenAGCFakeAgents YES \
  -OpenAGCSnapshot "$OUT" "$@" >/dev/null 2>"${SNAPSHOT_LOG:-/dev/null}"
echo "snapshot: $OUT"
