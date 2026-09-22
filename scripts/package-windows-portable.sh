#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

ARCH="amd64"
NO_BUILD=0
for arg in "$@"; do
  case "$arg" in
    amd64|x64) ARCH="amd64" ;;
    arm64) ARCH="arm64" ;;
    --no-build) NO_BUILD=1 ;;
    *) echo "usage: $0 [amd64|x64|arm64] [--no-build]" >&2; exit 2 ;;
  esac
done

case "$ARCH" in
  arm64) TARGET="aarch64-pc-windows-msvc" ;;
  amd64) TARGET="x86_64-pc-windows-msvc" ;;
esac

# The workspace product version ([workspace.package]), shared by the
# hydra-gui, hydra-cli and hydra-host bin crates this bundle carries.
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

if [ "$NO_BUILD" = 0 ]; then
  echo "building hydra-gui + hydra-host + hydra (cli) for $TARGET..."
  # cargo-xwin puts its sysroot -L in CFLAGS, which clang flags as unused when
  # embed-resource runs it as the .rc preprocessor.
  PATH="/opt/homebrew/opt/llvm/bin:$PATH" \
    TARGET_CFLAGS="-Wno-unused-command-line-argument" \
    cargo xwin build --release --target "$TARGET" --cross-compiler clang \
      -p hya-gui -p hya-host -p hya-cli
fi

BUILD="target/$TARGET/release"
for bin in hydra-gui.exe hydra-host.exe hydra.exe; do
  [ -f "$BUILD/$bin" ] || {
    echo "missing $BUILD/$bin (build failed or --no-build without a prior build?)" >&2
    exit 1
  }
done

STAGE="target/dist/portable-$ARCH"
ROOT="$STAGE/HydraPortable"
APP="$ROOT/App/Hydra"
rm -rf "$STAGE"
mkdir -p "$APP/assets" "$ROOT/App/DefaultData/hydra"

# The launcher shell: HydraPortable.exe, App/AppInfo (icons, appinfo.ini) and
# App/AppInfo/Launcher/HydraPortable.ini, which is what points the launcher at
# App\Hydra\hydra-gui.exe. .gitkeep only exists to keep the empty seed
# directory in Git and has no business in a released bundle.
cp -R scripts/windows/portable/. "$ROOT/"
find "$ROOT" \( -name .gitkeep -o -name .DS_Store \) -delete

# PortableApps.com reads the bundle version from here, and a static template
# would go stale one release after it was written. PackageVersion must be four
# numeric components; a pre-release tag (0.6.0-rc) ships under its base version
# because the suffix has no place to go in that format.
NUM_VERSION="${VERSION%%-*}"
while [ "$(printf '%s' "$NUM_VERSION" | tr -cd . | wc -c)" -lt 3 ]; do
  NUM_VERSION="$NUM_VERSION.0"
done
cat >> "$ROOT/App/AppInfo/appinfo.ini" <<INI

[Version]
PackageVersion=$NUM_VERSION
DisplayVersion=$VERSION
INI

# The application itself, file for file as hydra-installer.nsi installs it:
# the GUI, the CLI under both of its names, the native-messaging host, the
# icon the shortcut uses and the licence.
cp "$BUILD/hydra-gui.exe" "$BUILD/hydra.exe" "$BUILD/hydra-host.exe" "$APP/"
cp "$BUILD/hydra.exe" "$APP/hya.exe"
cp scripts/windows/hydra.ico "$APP/"
cp LICENSE "$APP/"
cp docs/logo.png "$APP/assets/"

echo "packing browser extensions..."
scripts/build-extensions.sh --out "$APP/extensions" --quiet \
  --prefix 'HydraPortable\App\Hydra\extensions'

mkdir -p target/dist
OUT="hydra-$VERSION-windows-portable-$ARCH.zip"
rm -f "target/dist/$OUT"
# 7-Zip on the Windows runners, Info-ZIP everywhere else; the runners have no
# `zip` and macOS has no `7z`.
if command -v zip >/dev/null 2>&1; then
  (cd "$STAGE" && zip -qr "../$OUT" HydraPortable)
elif command -v 7z >/dev/null 2>&1; then
  (cd "$STAGE" && 7z a -bso0 -bsp0 -tzip "../$OUT" HydraPortable)
else
  echo "no zip tool found - install Info-ZIP (zip) or 7-Zip (7z)" >&2
  exit 1
fi
echo "Built: target/dist/$OUT"
