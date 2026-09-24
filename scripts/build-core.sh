#!/usr/bin/env bash
# Build openagc-core and lay it out for Xcode (spec §4.1):
#   1. cargo build the static library
#   2. generate Swift bindings, C header and modulemap with uniffi-bindgen-swift
#   3. install them under build/core/, rewriting only files whose content changed
#
# Output (gitignored):
#   build/core/include/   openagc_coreFFI.h + module.modulemap  (SWIFT_INCLUDE_PATHS)
#   build/core/lib/       libopenagc_core.a                     (LIBRARY_SEARCH_PATHS)
#   build/core/swift/     openagc_core.swift                    (compiled into OpenAGCCore)
#
# Xcode reads these paths directly at compile/link time. An XCFramework is
# deliberately not used for development builds: Xcode copies XCFramework
# headers in a step planned before this script runs, so regenerated headers
# would be missed until the next build.
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

# Pin the SDK and deployment target so C dependencies (bundled SQLite)
# compile identically inside Xcode, whose PATH puts the real clang first
# and so needs SDKROOT, and in a terminal, where cc is the xcrun shim.
SDKROOT="$(xcrun --sdk macosx --show-sdk-path)" \
MACOSX_DEPLOYMENT_TARGET=26.0 \
  env -u IPHONEOS_DEPLOYMENT_TARGET cargo build "${CARGO_FLAGS[@]}"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/include" "$STAGE/swift" "$STAGE/lib"

BINDGEN=(cargo run --quiet --locked --package uniffi-bindgen-swift --)
"${BINDGEN[@]}" --swift-sources "$LIB" "$STAGE/swift"
"${BINDGEN[@]}" --headers "$LIB" "$STAGE/include"
# A plain `module` (not `--xcframework`, which emits `framework module`).
"${BINDGEN[@]}" --modulemap --module-name "$MODULE" \
  --modulemap-filename module.modulemap "$LIB" "$STAGE/include"
cp "$LIB" "$STAGE/lib/"

# Install only what changed, so unchanged builds stay incremental.
changed=0
while IFS= read -r -d '' src; do
  rel="${src#"$STAGE"/}"
  dst="$OUT/$rel"
  if ! cmp -s "$src" "$dst"; then
    mkdir -p "$(dirname "$dst")"
    cp "$src" "$dst"
    changed=$((changed + 1))
  fi
done < <(find "$STAGE" -type f -print0)

echo "build-core: $PROFILE → $OUT ($changed file(s) updated)"
