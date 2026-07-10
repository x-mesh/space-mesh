#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${VERSION:-$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$ROOT/core/Cargo.toml" | head -n1)}"
ARCH="${ARCH:-$(uname -m)}"
HOST_ARCH="$(uname -m)"
APP_NAME="space-mesh.app"
APP_DIR="$ROOT/dist/$APP_NAME"
ARCHIVE="$ROOT/dist/space-mesh-macos-$VERSION-$ARCH.zip"
ICON_SOURCE="$ROOT/packaging/AppIcon-1024.png"
ICONSET="$ROOT/dist/AppIcon.iconset"

case "$ARCH" in
  arm64|x86_64) ;;
  *)
    echo "ERROR: unsupported architecture: $ARCH" >&2
    exit 2
    ;;
esac

if [[ "$ARCH" != "$HOST_ARCH" ]]; then
  echo "ERROR: native package architecture mismatch: requested=$ARCH host=$HOST_ARCH" >&2
  exit 2
fi

if [[ -z "$VERSION" ]]; then
  echo "ERROR: could not determine version from core/Cargo.toml" >&2
  exit 2
fi

echo "==> Building space-mesh $VERSION for $ARCH"
make -C "$ROOT" app-release cli

SWIFT_BIN_DIR="$(cd "$ROOT/app" && swift build -c release --show-bin-path)"
APP_BINARY="$SWIFT_BIN_DIR/SpaceMeshApp"
RUST_DYLIB="$ROOT/core/target/release/libspace_ffi.dylib"
CLI_BINARY="$ROOT/core/target/release/space-mesh"

for path in "$APP_BINARY" "$RUST_DYLIB" "$CLI_BINARY" "$ICON_SOURCE"; do
  if [[ ! -f "$path" ]]; then
    echo "ERROR: required build output is missing: $path" >&2
    exit 1
  fi
done

rm -rf "$APP_DIR" "$ARCHIVE" "$ICONSET"
install -d "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Frameworks" "$APP_DIR/Contents/Resources"
install -m 755 "$APP_BINARY" "$APP_DIR/Contents/MacOS/SpaceMeshApp"
install -m 755 "$RUST_DYLIB" "$APP_DIR/Contents/Frameworks/libspace_ffi.dylib"
install -m 755 "$CLI_BINARY" "$APP_DIR/Contents/Resources/space-mesh"
install -m 644 "$ROOT/packaging/Info.plist" "$APP_DIR/Contents/Info.plist"

install -d "$ICONSET"
for spec in \
  "16 icon_16x16.png" \
  "32 icon_16x16@2x.png" \
  "32 icon_32x32.png" \
  "64 icon_32x32@2x.png" \
  "128 icon_128x128.png" \
  "256 icon_128x128@2x.png" \
  "256 icon_256x256.png" \
  "512 icon_256x256@2x.png" \
  "512 icon_512x512.png" \
  "1024 icon_512x512@2x.png"; do
  size="${spec%% *}"
  name="${spec#* }"
  sips -z "$size" "$size" "$ICON_SOURCE" --out "$ICONSET/$name" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP_DIR/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET"

/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$APP_DIR/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$APP_DIR/Contents/Info.plist"

BUNDLED_APP="$APP_DIR/Contents/MacOS/SpaceMeshApp"
BUNDLED_DYLIB="$APP_DIR/Contents/Frameworks/libspace_ffi.dylib"
LINKED_DYLIB="$(otool -L "$BUNDLED_APP" | awk '/libspace_ffi\.dylib/{print $1; exit}')"
if [[ -z "$LINKED_DYLIB" ]]; then
  echo "ERROR: SpaceMeshApp does not link libspace_ffi.dylib" >&2
  exit 1
fi

install_name_tool -id "@rpath/libspace_ffi.dylib" "$BUNDLED_DYLIB"
install_name_tool -change "$LINKED_DYLIB" \
  "@executable_path/../Frameworks/libspace_ffi.dylib" "$BUNDLED_APP"

# Ad-hoc signing keeps the unsigned GitHub artifact internally consistent.
codesign --force --sign - "$BUNDLED_DYLIB"
codesign --force --deep --sign - "$APP_DIR"
codesign --verify --deep --strict --verbose=2 "$APP_DIR"

if otool -L "$BUNDLED_APP" | tail -n +2 | grep -Fq "$ROOT"; then
  echo "ERROR: packaged app still references the build workspace" >&2
  otool -L "$BUNDLED_APP" >&2
  exit 1
fi

CLI_VERSION="$("$APP_DIR/Contents/Resources/space-mesh" --version)"
if [[ "$CLI_VERSION" != "space-mesh $VERSION" ]]; then
  echo "ERROR: bundled CLI version mismatch: $CLI_VERSION" >&2
  exit 1
fi

ditto -c -k --sequesterRsrc --keepParent "$APP_DIR" "$ARCHIVE"

echo "==> Package ready"
echo "    $ARCHIVE"
shasum -a 256 "$ARCHIVE"
