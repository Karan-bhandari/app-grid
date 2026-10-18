#!/usr/bin/env bash
set -euo pipefail

[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"
[ -s "$HOME/.nvm/nvm.sh" ] && source "$HOME/.nvm/nvm.sh" && nvm use 24 >/dev/null 2>&1 || true

APP_BUNDLE="src-tauri/target/release/bundle/macos/App Grid.app"

echo "Building release bundle..."
cargo tauri build --bundles app

SIGN_IDENTITY="-"
if security find-identity -v -p codesigning 2>/dev/null | grep -q "SpotlightExtLocalDev"; then
  SIGN_IDENTITY="SpotlightExtLocalDev"
fi

echo "Signing with identity '$SIGN_IDENTITY' (identifier: com.personal.spotlight-launchpad)..."
codesign --force --deep -s "$SIGN_IDENTITY" --identifier "com.personal.spotlight-launchpad" "$APP_BUNDLE"
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP_BUNDLE"

echo "Cleaning intermediate Cargo build artifacts to keep disk usage minimal..."
rm -rf \
  src-tauri/target/debug \
  src-tauri/target/release/deps \
  src-tauri/target/release/build \
  src-tauri/target/release/incremental \
  src-tauri/target/release/.fingerprint \
  src-tauri/target/release/examples \
  src-tauri/target/release/app-grid \
  src-tauri/target/release/app-grid.d \
  src-tauri/target/CACHEDIR.TAG \
  src-tauri/target/.rustc_info.json

echo "Done! Launch with: open \"$APP_BUNDLE\""
