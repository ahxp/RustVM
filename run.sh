#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
RELEASE_BIN="$ROOT_DIR/target/release/rustvm"
DEBUG_BIN="$ROOT_DIR/target/debug/rustvm"

if [[ -x "$RELEASE_BIN" ]]; then
    BIN="$RELEASE_BIN"
else
    cargo build --manifest-path "$ROOT_DIR/Cargo.toml"
    BIN="$DEBUG_BIN"
fi

# Ensure virtualization entitlement is always attached
if ! codesign -d --entitlements - "$BIN" 2>&1 | grep -q "com.apple.security.virtualization"; then
    codesign --entitlements "$ROOT_DIR/entitlements.plist" --force -s - "$BIN" >/dev/null 2>&1
fi

exec "$BIN" "$@"
