#!/usr/bin/env bash
# Build a release DMG (spec §16): Release configuration, Developer ID
# signing with the hardened runtime on every Mach-O, notarization, stapling.
#
#   scripts/release.sh 0.2.0 [--beta]
#
# Needs, for a signed and notarized build (docs/releasing.md):
#   DEVELOPER_ID_APPLICATION  e.g. "Developer ID Application: Actual AI (TEAMID)"
#   NOTARY_PROFILE            a `xcrun notarytool store-credentials` profile
# Without them it still builds, signs ad hoc and makes the DMG, and says so:
# useful to check the pipeline, not to ship.
set -euo pipefail

cd "$(dirname "$0")/.."
version=${1:?usage: release.sh <version> [--beta]}
beta=${2:-}
out=build/release
app_build=build/ReleaseDerivedData
rm -rf "$out" "$app_build"
mkdir -p "$out"

identity=${DEVELOPER_ID_APPLICATION:-}
if [[ -z "$identity" ]]; then
  echo "release: DEVELOPER_ID_APPLICATION not set — signing ad hoc; this DMG is NOT distributable" >&2
fi

(cd macos && xcodegen generate --quiet)
xcodebuild -project macos/OpenAGC.xcodeproj -scheme OpenAGC -configuration Release \
  -derivedDataPath "$app_build" -destination 'platform=macOS,arch=arm64' \
  MARKETING_VERSION="$version" \
  CODE_SIGN_IDENTITY="${identity:--}" \
  OTHER_CODE_SIGN_FLAGS="--timestamp --options runtime" \
  build > "$out/xcodebuild.log" 2>&1 || { grep -E "error:" "$out/xcodebuild.log" | head -20; exit 1; }

app="$app_build/Build/Products/Release/OpenAGC.app"
[[ -d "$app" ]] || { echo "release: build produced no app" >&2; exit 1; }

# Re-sign inside out so every nested Mach-O carries the hardened runtime and
# a secure timestamp (Xcode skips --options runtime for ad-hoc builds).
sign() {
  local flags=(--force --options runtime --sign "${identity:--}")
  [[ -n "$identity" ]] && flags+=(--timestamp)
  codesign "${flags[@]}" "$@"
}
# Innermost first: Sparkle's XPC services and helper app as bundles, its
# Autoupdate tool, then the frameworks, our shim, and the app last.
while IFS= read -r -d '' b; do sign "$b"; done < <(find "$app/Contents/Frameworks" -depth \( -name "*.xpc" -o -name "*.app" \) -print0)
while IFS= read -r -d '' f; do
  if file "$f" | grep -q "Mach-O" && [[ "$f" != *.xpc/* && "$f" != *.app/Contents/MacOS/* ]]; then sign "$f"; fi
done < <(find "$app/Contents/Frameworks" -type f -perm -u+x -print0)
for fw in "$app"/Contents/Frameworks/*.framework; do sign "$fw"; done
sign "$app/Contents/MacOS/openagc-mcp"
entitlements=macos/OpenAGC/Resources/OpenAGC.entitlements
if [[ -z "$identity" ]]; then
  # Ad-hoc signatures never share a team, so library validation would stop
  # the app loading its own frameworks. Dry runs only: a Developer ID build
  # keeps library validation on.
  entitlements=$(mktemp -t openagc-adhoc).plist
  /usr/libexec/PlistBuddy -c "Add :com.apple.security.cs.disable-library-validation bool true" "$entitlements" >/dev/null
fi
sign --entitlements "$entitlements" "$app"

# Verify: hardened runtime on the app and every nested Mach-O.
fail=0
while IFS= read -r -d '' f; do
  if file "$f" | grep -q "Mach-O"; then
    # Capture first: under pipefail, grep -q's early exit would SIGPIPE codesign.
    details=$(codesign -dv "$f" 2>&1 || true)
    if ! grep -Eq "flags=0x[0-9a-f]+\([^)]*runtime" <<<"$details"; then
      echo "release: no hardened runtime on ${f#$app/}" >&2
      fail=1
    fi
  fi
done < <(find "$app" -type f -perm -u+x -print0)
codesign --verify --deep --strict "$app"
[[ $fail -eq 0 ]] || exit 1
echo "release: hardened runtime on every Mach-O"

name="OpenAGC-$version"
dmg="$out/$name.dmg"
stage=$(mktemp -d)
cp -R "$app" "$stage/"
ln -s /Applications "$stage/Applications"
hdiutil create -volname "OpenAGC $version" -srcfolder "$stage" -ov -format UDZO "$dmg" >/dev/null
rm -rf "$stage"
[[ -n "$identity" ]] && codesign --sign "$identity" --timestamp "$dmg"

if [[ -n "$identity" && -n "${NOTARY_PROFILE:-}" ]]; then
  xcrun notarytool submit "$dmg" --keychain-profile "$NOTARY_PROFILE" --wait
  xcrun stapler staple "$dmg"
  spctl -a -t open --context context:primary-signature -v "$dmg"
  echo "release: notarized and stapled $dmg"
  scripts/make-appcast.sh "$dmg" "$version" $beta
else
  echo "release: not notarized (set DEVELOPER_ID_APPLICATION and NOTARY_PROFILE); built $dmg"
fi
