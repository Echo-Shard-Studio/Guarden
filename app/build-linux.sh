#!/usr/bin/env bash
# Build the Linux packages (AppImage, .deb) locally. Releases are published by the Release workflow
# (.github/workflows/release.yml), not from here.
# Needs tauri-cli, webkit2gtk 4.1 dev files and the updater key at ~/.tauri/guarden.key (never commit it).
set -euo pipefail
cd "$(dirname "$0")/.."
T=x86_64-unknown-linux-gnu

# The CLI and shim ship next to the app (bundle.externalBin wants the target triple in the name).
cargo build --release --bin guarden --bin guarden-shim
mkdir -p app/src-tauri/binaries
for b in guarden guarden-shim; do cp "target/release/$b" "app/src-tauri/binaries/$b-$T"; done

export TAURI_SIGNING_PRIVATE_KEY="$HOME/.tauri/guarden.key" TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
(cd app/src-tauri && cargo tauri build --bundles appimage,deb)
ls -la target/release/bundle/appimage target/release/bundle/deb
