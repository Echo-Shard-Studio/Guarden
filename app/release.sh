#!/usr/bin/env bash
# Build the Windows installer and its auto-update files, from Linux.
# Needs cargo-xwin, tauri-cli, makensis and the updater key at ~/.tauri/guarden.key (never commit it).
# Output in dist/release/: the setup to upload to a GitHub release tagged v<version>, its .sig, and
# latest.json, which the installed apps poll (see plugins.updater.endpoints in tauri.conf.json).
set -euo pipefail
cd "$(dirname "$0")/.."
T=x86_64-pc-windows-msvc
REPO=Echo-Shard-Studio/Guarden
VERSION=$(grep -m1 '^version' app/src-tauri/Cargo.toml | cut -d'"' -f2)

# The CLI and shim ship next to the app (bundle.externalBin wants the target triple in the name).
cargo xwin build --release --target $T --bin guarden --bin guarden-shim
mkdir -p app/src-tauri/binaries
for b in guarden guarden-shim; do cp "target/$T/release/$b.exe" "app/src-tauri/binaries/$b-$T.exe"; done

export TAURI_SIGNING_PRIVATE_KEY="$HOME/.tauri/guarden.key" TAURI_SIGNING_PRIVATE_KEY_PASSWORD=""
(cd app/src-tauri && cargo tauri build --runner cargo-xwin --target $T)

SETUP="Guarden_${VERSION}_x64-setup.exe"
mkdir -p dist/release
cp "target/$T/release/bundle/nsis/$SETUP" "target/$T/release/bundle/nsis/$SETUP.sig" dist/release/
python3 -I - "$VERSION" "$REPO" "$SETUP" <<'PY'
import json, sys, datetime
version, repo, setup = sys.argv[1:]
json.dump({
    "version": version,
    "pub_date": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
    "platforms": {"windows-x86_64": {
        "signature": open(f"dist/release/{setup}.sig").read(),
        "url": f"https://github.com/{repo}/releases/download/v{version}/{setup}",
    }},
}, open("dist/release/latest.json", "w"), indent=2)
PY
ls -la dist/release
