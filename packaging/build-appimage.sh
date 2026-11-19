#!/bin/sh
# Builds target/TidyTwin-x86_64.AppImage. Needs curl and libfuse2 (or set APPIMAGE_EXTRACT_AND_RUN=1).
set -eu
cd "$(dirname "$0")/.."
cargo build --release
tool=target/appimagetool-1.9.1
[ -x "$tool" ] || { curl -fL -o "$tool" https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage; chmod +x "$tool"; }
app=target/AppDir
rm -rf "$app" && mkdir -p "$app/usr/bin"
cp target/release/tidytwin "$app/usr/bin/"
cp packaging/tidytwin.desktop packaging/tidytwin.svg "$app/"
ln -s tidytwin.svg "$app/.DirIcon"
ln -s usr/bin/tidytwin "$app/AppRun"
ARCH=x86_64 "$tool" "$app" target/TidyTwin-x86_64.AppImage
