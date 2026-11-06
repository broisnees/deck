#!/bin/sh
# Cross-build Deck for Windows on Linux: dist/Deck-<version>-setup.exe (the
# installer) and dist/Deck-<version>-windows-x64.zip (portable).
#
#     scripts/build-windows.sh
#
# Needs cargo-xwin (cargo install cargo-xwin), llvm (llvm-rc for the icon
# and version resource, lld), makensis (NSIS 3) and zip. No Zig: the
# Ghostty engine is Unix only, so deck.exe runs the Alacritty engine.
#
# DECK_TOOLCHAIN picks the rustup toolchain (default 1.96.1).
# DECK_PROFILE=dev packages a debug build instead of a release one.
#
# fxc.exe only runs on Windows, so a deck.exe built here carries gpui's
# HLSL source and compiles its Direct3D shaders at launch with the
# d3dcompiler_47.dll every Windows 10 and 11 has (vendor/gpui, "Deck patch").
set -eu

cd "$(dirname "$0")/.."
TOOLCHAIN="${DECK_TOOLCHAIN:-1.96.1}"
TARGET=x86_64-pc-windows-msvc
PROFILE="${DECK_PROFILE:-release}"
OUT=release
[ "$PROFILE" = dev ] && OUT=debug

VERSION="$(cargo metadata --no-deps --format-version 1 |
    python3 -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "app"))')"
DIST="$PWD/dist"
STAGE="$DIST/windows/Deck"
SETUP="$DIST/Deck-$VERSION-setup.exe"
ZIP="$DIST/Deck-$VERSION-windows-x64.zip"

say() { printf '\033[1m==> %s\033[0m\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

for tool in cargo-xwin makensis zip llvm-rc; do
    command -v "$tool" >/dev/null || die "$tool not found (see the top of this script)"
done

say "building deck.exe $VERSION ($TARGET, $PROFILE)"
cargo "+$TOOLCHAIN" xwin build --profile "$PROFILE" --target "$TARGET" -p app

say "staging $STAGE"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "target/$TARGET/$OUT/deck.exe" "$STAGE/"

say "installer $SETUP"
rm -f "$SETUP"
makensis -V2 -DVERSION="$VERSION" -DSRC="$STAGE" -DOUTFILE="$SETUP" packaging/windows/deck.nsi

say "portable zip $ZIP"
rm -f "$ZIP"
(cd "$DIST/windows" && zip -qr9 "$ZIP" Deck)

ls -l "$SETUP" "$ZIP"
