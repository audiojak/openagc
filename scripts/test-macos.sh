#!/usr/bin/env bash
# Regenerate the Xcode project and run the app's tests.
# Output is filtered to errors, warnings from our sources and test results.
# Usage: scripts/test-macos.sh [build|test] [extra xcodebuild args...]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ACTION="${1:-test}"
shift || true

mkdir -p "$ROOT/build"
cd "$ROOT/macos"
xcodegen generate --spec project.yml --quiet

set +e
xcodebuild -project OpenAGC.xcodeproj -scheme OpenAGC \
  -destination 'platform=macOS,arch=arm64' \
  -derivedDataPath "$ROOT/build/DerivedData" \
  "$ACTION" "$@" >"$ROOT/build/xcodebuild.log" 2>&1
status=$?
set -e

grep -E "error:|$ROOT/macos/.*warning:|\*\* (BUILD|TEST) |✔ Test |✘" "$ROOT/build/xcodebuild.log" \
  | grep -v -e 'appintentsmetadataprocessor' -e 'com.apple.linkd' || true
[[ $status -eq 0 ]] || echo "xcodebuild failed ($status); full log: build/xcodebuild.log"
exit $status
