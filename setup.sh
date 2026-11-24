#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$ROOT_DIR"

echo "==> Building RustVM (release)..."
cargo build --release

BIN_PATH="$ROOT_DIR/target/release/rustvm"
APP_DIR="$ROOT_DIR/dist/RustVM.app"

echo "==> Assembling macOS app bundle: $APP_DIR"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$BIN_PATH" "$APP_DIR/Contents/MacOS/rustvm"
cp "$ROOT_DIR/Info.plist" "$APP_DIR/Contents/Info.plist"

if command -v python3 >/dev/null 2>&1 && command -v iconutil >/dev/null 2>&1; then
    python3 "$ROOT_DIR/assets/generate_icon.py" "$APP_DIR/Contents/Resources/RustVM.icns" || true
fi

echo "==> Code-signing with Apple Virtualization entitlement..."
codesign --entitlements "$ROOT_DIR/entitlements.plist" --force -s - "$BIN_PATH"
codesign --entitlements "$ROOT_DIR/entitlements.plist" --force -s - "$APP_DIR/Contents/MacOS/rustvm"
codesign --entitlements "$ROOT_DIR/entitlements.plist" --force -s - "$APP_DIR"

# Install CLI symlink in ~/.local/bin
mkdir -p "$HOME/.local/bin"
ln -sf "$APP_DIR/Contents/MacOS/rustvm" "$HOME/.local/bin/rustvm"

# Optional: copy RustVM.app into ~/Applications if requested
if [[ "${1:-}" == "--install-app" ]]; then
    mkdir -p "$HOME/Applications"
    rm -rf "$HOME/Applications/RustVM.app"
    cp -R "$APP_DIR" "$HOME/Applications/RustVM.app"
    codesign --entitlements "$ROOT_DIR/entitlements.plist" --force -s - "$HOME/Applications/RustVM.app"
    echo "==> Installed app to $HOME/Applications/RustVM.app"
fi

echo ""
echo "================================================================"
echo " RustVM is ready!"
echo "================================================================"
echo "  GUI App : open \"$APP_DIR\""
echo "  CLI     : $HOME/.local/bin/rustvm  (or ./run.sh)"
echo ""
echo "Quick ways to boot Linux right now:"
echo "  1. Open GUI Manager  : open \"$APP_DIR\""
echo "  2. 1-command Alpine  : ./run.sh quickstart alpine"
echo "  3. 1-command Ubuntu  : ./run.sh quickstart ubuntu-desktop"
echo "  4. Terminal Linux    : ./run.sh quickstart alpine --headless"
echo ""
if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    echo "Tip: Add ~/.local/bin to your PATH to run 'rustvm' from anywhere:"
    echo "  echo 'export PATH=\"\$HOME/.local/bin:\$PATH\"' >> ~/.zshrc && source ~/.zshrc"
fi
