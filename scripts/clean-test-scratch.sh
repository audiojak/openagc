#!/usr/bin/env bash
# Remove scratch directories that earlier test runs left in $TMPDIR:
# `openagc-*` (Rust tests) and UUID-named OpenAGC data directories (Swift
# tests), when older than an hour so a concurrent run is never disturbed.
# Called by gate.sh and test-macos.sh; safe to run by hand.
set -euo pipefail
TMP="${TMPDIR:-/tmp}"
find "$TMP" -maxdepth 1 -name 'openagc-*' -mmin +60 -exec rm -rf {} + 2>/dev/null || true
find "$TMP" -maxdepth 1 -type d -mmin +60 \
  -regex '.*/[0-9A-F]\{8\}-[0-9A-F]\{4\}-[0-9A-F]\{4\}-[0-9A-F]\{4\}-[0-9A-F]\{12\}' 2>/dev/null |
  while IFS= read -r dir; do
    if [[ -d "$dir/accounts" || -f "$dir/mail.sqlite" || -d "$dir/agents" ]]; then rm -rf "$dir"; fi
  done
