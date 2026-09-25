#!/usr/bin/env bash
# Render macos/Icon/openagc-icon.svg into the app's AppIcon asset catalog.
# Needs rsvg-convert (brew install librsvg). Run after editing the SVG.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SVG="$ROOT/macos/Icon/openagc-icon.svg"
SET="$ROOT/macos/OpenAGC/Resources/Assets.xcassets/AppIcon.appiconset"
mkdir -p "$SET"
images=()
for size in 16 32 128 256 512; do
  for scale in 1 2; do
    px=$((size * scale))
    name="icon_${size}x${size}@${scale}x.png"
    rsvg-convert -w "$px" -h "$px" "$SVG" -o "$SET/$name"
    images+=("{\"filename\":\"$name\",\"idiom\":\"mac\",\"scale\":\"${scale}x\",\"size\":\"${size}x${size}\"}")
  done
done
printf '{"images":[%s],"info":{"author":"xcode","version":1}}\n' "$(IFS=,; echo "${images[*]}")" \
  | python3 -m json.tool --indent 2 > "$SET/Contents.json"
echo '{"info":{"author":"xcode","version":1}}' > "$SET/../Contents.json"
echo "make-icon: wrote $SET"
