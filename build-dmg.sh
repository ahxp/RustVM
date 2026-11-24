#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT_DIR"

VERSION="${1:-$(awk -F'"' '/^version *=/{print $2; exit}' Cargo.toml)}"
ARCH="$(uname -m)"
APP_NAME="RustVM"
VOL_NAME="${APP_NAME} ${VERSION}"
APP_BUNDLE="$ROOT_DIR/dist/${APP_NAME}.app"
DMG_NAME="${APP_NAME}-${VERSION}-${ARCH}.dmg"
DMG_PATH="$ROOT_DIR/dist/${DMG_NAME}"
SHA_PATH="${DMG_PATH}.sha256"

echo "==> Building ${APP_NAME} v${VERSION} (${ARCH})..."
./setup.sh

if [[ ! -d "$APP_BUNDLE" ]]; then
    echo "Error: App bundle not found at $APP_BUNDLE" >&2
    exit 1
fi

STAGE_DIR="$(mktemp -d -t rustvm_dmg_stage)"
RW_DMG="$(mktemp -u -t rustvm_rw).dmg"
MOUNT_DIR=""

cleanup() {
    if [[ -n "$MOUNT_DIR" && -d "$MOUNT_DIR" ]]; then
        hdiutil detach "$MOUNT_DIR" -quiet || hdiutil detach "$MOUNT_DIR" -force -quiet || true
    fi
    rm -rf "$STAGE_DIR" "$RW_DMG"
}
trap cleanup EXIT

echo "==> Staging DMG contents..."
cp -R "$APP_BUNDLE" "$STAGE_DIR/${APP_NAME}.app"
ln -s /Applications "$STAGE_DIR/Applications"

ICON_SRC="$APP_BUNDLE/Contents/Resources/${APP_NAME}.icns"
if [[ -f "$ICON_SRC" ]]; then
    cp "$ICON_SRC" "$STAGE_DIR/.VolumeIcon.icns"
fi

# Calculate image size with generous headroom (minimum 32 MB)
APP_SIZE_MB=$(du -sm "$STAGE_DIR" | awk '{print $1}')
DMG_SIZE_MB=$(( APP_SIZE_MB + 24 ))
if (( DMG_SIZE_MB < 32 )); then
    DMG_SIZE_MB=32
fi

echo "==> Creating temporary read-write disk image (${DMG_SIZE_MB} MB)..."
hdiutil create \
    -volname "$VOL_NAME" \
    -srcfolder "$STAGE_DIR" \
    -fs HFS+ \
    -fsargs "-c c=64,a=16,e=16" \
    -format UDRW \
    -size "${DMG_SIZE_MB}m" \
    "$RW_DMG" >/dev/null

echo "==> Configuring volume icon and Finder layout..."
ATTACH_OUT="$(hdiutil attach -readwrite -noverify -noautoopen "$RW_DMG")"
MOUNT_DIR="$(echo "$ATTACH_OUT" | awk -F'\t' '/\/Volumes\//{print $NF; exit}')"

if [[ -n "$MOUNT_DIR" && -d "$MOUNT_DIR" ]]; then
    if [[ -f "$MOUNT_DIR/.VolumeIcon.icns" ]] && xcrun -f SetFile >/dev/null 2>&1; then
        xcrun SetFile -c icnC "$MOUNT_DIR/.VolumeIcon.icns" || true
        xcrun SetFile -a C "$MOUNT_DIR" || true
    fi
    # Clean macOS indexing metadata so DMG stays tidy
    rm -rf "$MOUNT_DIR/.Trashes" "$MOUNT_DIR/.fseventsd" 2>/dev/null || true
    sync
    hdiutil detach "$MOUNT_DIR" -quiet
    MOUNT_DIR=""
fi

echo "==> Compressing final DMG: $DMG_PATH"
rm -f "$DMG_PATH" "$SHA_PATH"
hdiutil convert "$RW_DMG" \
    -format UDZO \
    -imagekey zlib-level=9 \
    -o "$DMG_PATH" >/dev/null

echo "==> Verifying DMG integrity..."
hdiutil verify "$DMG_PATH" >/dev/null

(cd "$ROOT_DIR/dist" && shasum -a 256 "$DMG_NAME" > "$(basename "$SHA_PATH")")

DMG_HUMAN_SIZE="$(du -h "$DMG_PATH" | awk '{print $1}')"
echo ""
echo "================================================================"
echo " DMG ready!"
echo "================================================================"
echo "  File     : $DMG_PATH ($DMG_HUMAN_SIZE)"
echo "  SHA-256  : $(cat "$SHA_PATH")"
echo "  Install  : Open the DMG and drag RustVM.app into Applications."
