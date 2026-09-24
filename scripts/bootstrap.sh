#!/usr/bin/env bash
# Install the developer toolchain for OpenAGC. Idempotent.
# Requires Homebrew and Xcode (App Store) selected with xcode-select.
set -euo pipefail

if ! xcodebuild -version >/dev/null 2>&1; then
  echo "Xcode is required: install it from the App Store, then run" >&2
  echo "  sudo xcode-select -s /Applications/Xcode.app && sudo xcodebuild -license accept" >&2
  exit 1
fi

brew install rustup xcodegen

export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"
# rust-toolchain.toml pins the version; this installs it plus components.
rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install

echo
echo "Toolchain ready:"
rustc --version
xcodebuild -version | head -1
xcodegen --version
echo
echo "Add to your shell profile if cargo is not found:"
echo '  export PATH="/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$PATH"'
