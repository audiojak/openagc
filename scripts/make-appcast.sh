#!/usr/bin/env bash
# Add a release to appcast.xml (spec §16). Run by the release maintainer (or
# release.yml) after the DMG is notarized and stapled:
#
#   scripts/make-appcast.sh build/release/OpenAGC-0.2.0.dmg 0.2.0 [--beta]
#
# The EdDSA private key comes from the maintainer's Keychain (created once
# with generate_keys), or from $SPARKLE_ED_PRIVATE_KEY in CI. The DMG is
# expected to be uploaded to the GitHub release tagged v<version>.
set -euo pipefail

cd "$(dirname "$0")/.."
dmg=${1:?usage: make-appcast.sh <dmg> <version> [--beta]}
version=${2:?usage: make-appcast.sh <dmg> <version> [--beta]}
channel=()
if [[ "${3:-}" == "--beta" ]]; then
  channel=(--channel beta)
fi

tools=$(find build/DerivedData/SourcePackages/artifacts -type d -path '*Sparkle/bin' 2>/dev/null | head -1)
if [[ -z "$tools" ]]; then
  echo "Sparkle tools not found; build the app once (scripts/test-macos.sh build)" >&2
  exit 1
fi

work=build/appcast
rm -rf "$work"
mkdir -p "$work"
cp "$dmg" "$work/"
# Keep earlier entries: generate_appcast updates an existing appcast.
[[ -f appcast.xml ]] && cp appcast.xml "$work/appcast.xml"
notes="${dmg%.dmg}.md"
[[ -f "$notes" ]] && cp "$notes" "$work/"

key_args=()
if [[ -n "${SPARKLE_ED_PRIVATE_KEY:-}" ]]; then
  key_args=(--ed-key-file -)
fi

printf '%s' "${SPARKLE_ED_PRIVATE_KEY:-}" | "$tools/generate_appcast" \
  ${key_args[@]+"${key_args[@]}"} \
  ${channel[@]+"${channel[@]}"} \
  --download-url-prefix "https://github.com/audiojak/openagc/releases/download/v${version}/" \
  --link "https://github.com/audiojak/openagc" \
  --maximum-versions 10 \
  "$work"

cp "$work/appcast.xml" appcast.xml
echo "appcast.xml updated for ${version}${channel:+ (beta)}; commit it to main."
