#!/usr/bin/env bash
# Build the Windows installer locally, from Linux, to test it in a VM. Releases are published by the
# Release workflow (.github/workflows/release.yml), not from here.
# Needs cargo-xwin, tauri-cli, makensis and the updater key at ~/.tauri/guarden.key (never commit it).
set -euo pipefail
cd "$(dirname "$0")/.."
T=x86_64-pc-windows-msvc
VERSION=$(grep -m1 '^version' app/src-tauri/Cargo.toml | cut -d'"' -f2)

# The CLI and shim ship next to the app (bundle.externalBin wants the target triple in the name).
cargo xwin build --release --target $T --bin guarden --bin guarden-shim
mkdir -p app/src-tauri/binaries
for b in guarden guarden-shim; do cp "target/$T/release/$b.exe" "app/src-tauri/binaries/$b-$T.exe"; done

export TAURI_SIGNING_PRIVATE_KEY="$HOME/.tauri/guarden.key" TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
(cd app/src-tauri && cargo tauri build --runner cargo-xwin --target $T)

mkdir -p dist/release
cp "target/$T/release/bundle/nsis/Guarden_${VERSION}_x64-setup.exe" dist/release/
ls -la dist/release
