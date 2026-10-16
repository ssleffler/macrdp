#!/bin/bash
# Builds macrdp-<version>.pkg: /Applications/macrdp.app (a universal Apple
# Silicon + Intel macrdp-server in a background app bundle, so macOS keys its
# permissions by bundle ID and can prompt for them) plus scripts that set it up
# as a login agent for the user who installs it.
# Usage: packaging/macos/build-pkg.sh   (output: packaging/macos/dist/)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="$(cd "$HERE/../.." && pwd)"
VERSION="0.2.1"
OUT="$HERE/dist/macrdp-$VERSION.pkg"
BUILD="$HERE/build"
ROOT="$BUILD/root"
APP="$ROOT/Applications/macrdp.app"
BIN="$APP/Contents/MacOS/macrdp-server"

. "$HOME/.cargo/env"
export MACOSX_DEPLOYMENT_TARGET=14.0
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --manifest-path "$SRC/Cargo.toml" -p macrdp-server --release --target "$target" 2>&1 \
        | grep -v -e '^objc\[' -e '^\s*Compiling' | grep -E '^error|Finished' || true
    test -x "$SRC/target/$target/release/macrdp-server"
done

# Payload: the static files, plus the app bundle and a command-line link to it
rm -rf "$BUILD" && mkdir -p "$ROOT" "$HERE/dist"
ditto --noextattr --noqtn "$HERE/payload" "$ROOT"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
sed "s/__VERSION__/$VERSION/g" "$HERE/Info.plist" > "$APP/Contents/Info.plist"
cp "$SRC/icon.icns" "$APP/Contents/Resources/macrdp.icns"
lipo -create \
    "$SRC/target/aarch64-apple-darwin/release/macrdp-server" \
    "$SRC/target/x86_64-apple-darwin/release/macrdp-server" \
    -output "$BIN"
# Load Swift concurrency from macOS itself, not from the Command Line Tools
# (avoids duplicate-class warnings and works on Macs without developer tools).
install_name_tool -delete_rpath /Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx \
    -add_rpath /usr/lib/swift "$BIN"
codesign --force --sign - --identifier com.macrdp.server "$APP"
ln -sf /Applications/macrdp.app/Contents/MacOS/macrdp-server "$ROOT/usr/local/bin/macrdp-server"

chmod 755 "$HERE"/scripts/* "$ROOT/usr/local/bin/macrdp-uninstall" "$ROOT/usr/local/share/macrdp/setup-user.sh"
chmod 644 "$ROOT/usr/local/share/macrdp/config.template.toml"

mkdir -p "$BUILD/component"
export COPYFILE_DISABLE=1
# Always install to /Applications: never "upgrade" some other macrdp.app copy
# that macOS happens to know about (e.g. a build folder)
pkgbuild --analyze --root "$ROOT" "$BUILD/components.plist" >/dev/null
plutil -replace 0.BundleIsRelocatable -bool NO "$BUILD/components.plist"
plutil -replace 0.BundleOverwriteAction -string upgrade "$BUILD/components.plist"
pkgbuild --root "$ROOT" --component-plist "$BUILD/components.plist" --scripts "$HERE/scripts" \
    --identifier com.macrdp.server --version "$VERSION" \
    --ownership recommended --install-location / \
    "$BUILD/component/macrdp-component.pkg"
productbuild --distribution "$HERE/distribution.xml" \
    --package-path "$BUILD/component" "$OUT"
echo "Built $OUT"
