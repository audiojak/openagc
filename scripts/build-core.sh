#!/usr/bin/env bash
# Build openagc-core and package it for the app (spec §4.1):
#   1. cargo build the static library
#   2. generate Swift bindings, C header and modulemap with uniffi-bindgen-swift
#   3. wrap library + headers in an XCFramework
#
# Output (gitignored): build/core/
#   OpenAGCCoreFFI.xcframework   static library + openagc_coreFFI module
#   swift/openagc_core.swift     generated bindings, compiled into OpenAGCCore
#
# Usage: scripts/build-core.sh [debug|release]
# Under Xcode, the profile follows $CONFIGURATION (Release → release).
set -euo pipefail

export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

PROFILE="${1:-}"
if [[ -z "$PROFILE" ]]; then
  if [[ "${CONFIGURATION:-Debug}" == "Release" ]]; then PROFILE=release; else PROFILE=debug; fi
fi

TARGET=aarch64-apple-darwin
OUT="$ROOT/build/core"
LIB="$ROOT/target/$TARGET/$PROFILE/libopenagc_core.a"
MODULE=openagc_coreFFI

CARGO_FLAGS=(--package openagc-core --target "$TARGET" --locked)
[[ "$PROFILE" == "release" ]] && CARGO_FLAGS+=(--release)

# Xcode exports SDK and deployment variables meant for Swift/Clang; keep
# them away from cargo so Rust builds identically inside and outside Xcode.
env -u SDKROOT -u MACOSX_DEPLOYMENT_TARGET -u IPHONEOS_DEPLOYMENT_TARGET \
  MACOSX_DEPLOYMENT_TARGET=26.0 cargo build "${CARGO_FLAGS[@]}"

STAGE="$OUT/stage"
rm -rf "$STAGE" "$OUT/swift" "$OUT/$MODULE.xcframework"
mkdir -p "$STAGE/headers" "$OUT/swift"

BINDGEN=(cargo run --quiet --locked --package uniffi-bindgen-swift --)
"${BINDGEN[@]}" --swift-sources "$LIB" "$OUT/swift"
"${BINDGEN[@]}" --headers "$LIB" "$STAGE/headers"
"${BINDGEN[@]}" --modulemap --module-name "$MODULE" \
  --modulemap-filename module.modulemap "$LIB" "$STAGE/headers"

xcodebuild -create-xcframework \
  -library "$LIB" -headers "$STAGE/headers" \
  -output "$OUT/$MODULE.xcframework" >/dev/null

rm -rf "$STAGE"
echo "build-core: $PROFILE → $OUT"
