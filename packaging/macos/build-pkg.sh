#!/bin/bash
# Builds macrdp-<version>.pkg: a universal (Apple Silicon + Intel) macrdp-server
# plus scripts that set it up as a login agent for the user who installs it.
# Usage: packaging/macos/build-pkg.sh   (output: packaging/macos/dist/)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="$(cd "$HERE/../.." && pwd)"
VERSION="0.1.1"
OUT="$HERE/dist/macrdp-$VERSION.pkg"
BUILD="$HERE/build"
BIN="$HERE/payload/usr/local/bin/macrdp-server"

. "$HOME/.cargo/env"
export MACOSX_DEPLOYMENT_TARGET=14.0
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    cargo build --manifest-path "$SRC/Cargo.toml" -p macrdp-server --release --target "$target" 2>&1 \
        | grep -v -e '^objc\[' -e '^\s*Compiling' | grep -E '^error|Finished' || true
    test -x "$SRC/target/$target/release/macrdp-server"
done

lipo -create \
    "$SRC/target/aarch64-apple-darwin/release/macrdp-server" \
    "$SRC/target/x86_64-apple-darwin/release/macrdp-server" \
    -output "$BIN"
# Load Swift concurrency from macOS itself, not from the Command Line Tools
# (avoids duplicate-class warnings and works on Macs without developer tools).
install_name_tool -delete_rpath /Library/Developer/CommandLineTools/usr/lib/swift-5.5/macosx \
    -add_rpath /usr/lib/swift "$BIN"
codesign --force --sign - --identifier com.macrdp.server "$BIN"

chmod 755 "$HERE"/scripts/* "$HERE/payload/usr/local/bin/"* "$HERE/payload/usr/local/share/macrdp/setup-user.sh"
chmod 644 "$HERE/payload/usr/local/share/macrdp/config.template.toml"

rm -rf "$BUILD/component" && mkdir -p "$BUILD/component" "$HERE/dist"
export COPYFILE_DISABLE=1
pkgbuild --root "$HERE/payload" --scripts "$HERE/scripts" \
    --identifier com.macrdp.server --version "$VERSION" \
    --ownership recommended --install-location / \
    "$BUILD/component/macrdp-component.pkg"
productbuild --distribution "$HERE/distribution.xml" \
    --package-path "$BUILD/component" "$OUT"
echo "Built $OUT"
