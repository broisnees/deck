#!/bin/sh
# Build the release binary and install it as a macOS app.
#
#     scripts/install.sh            # /Applications/Deck.app + ~/.local/bin/deck
#     DECK_APP=~/Applications/Deck.app DECK_BIN=/usr/local/bin scripts/install.sh
#
# The bundle is assembled in a temporary folder and swapped in at the end, so
# a failed build never leaves a half-installed app behind.
set -eu

cd "$(dirname "$0")/.."
APP="${DECK_APP:-/Applications/Deck.app}"
BIN="${DECK_BIN:-$HOME/.local/bin}"

cargo build --release -p app

# Sign with a certificate that stays the same from build to build. macOS
# remembers what you allowed an app (files and folders, notifications,
# automation…) by its signature; an ad-hoc one changes with every build, so
# each install was a stranger and asked for everything again. The
# certificate is made once, on this Mac, in a keychain of its own that only
# holds it. Its password guards nothing else, so it lives here in the clear.
KEYCHAIN="$HOME/Library/Application Support/deck/signing.keychain-db"
KEYPASS="deck-local-signing"
IDENTITY="Deck Local Signing"

make_identity() {
    work="$(mktemp -d)"
    openssl req -x509 -newkey rsa:2048 -nodes -days 7300 -subj "/CN=$IDENTITY" \
        -addext "basicConstraints=critical,CA:false" \
        -addext "keyUsage=critical,digitalSignature" \
        -addext "extendedKeyUsage=critical,codeSigning" \
        -keyout "$work/key.pem" -out "$work/cert.pem" 2>/dev/null || return 1
    # OpenSSL 3 needs -legacy for a PKCS#12 that `security` can read;
    # LibreSSL has no such flag and writes the old format anyway.
    openssl pkcs12 -export -legacy -inkey "$work/key.pem" -in "$work/cert.pem" \
        -name "$IDENTITY" -out "$work/id.p12" -passout "pass:$KEYPASS" 2>/dev/null ||
        openssl pkcs12 -export -inkey "$work/key.pem" -in "$work/cert.pem" \
            -name "$IDENTITY" -out "$work/id.p12" -passout "pass:$KEYPASS" || return 1
    mkdir -p "$(dirname "$KEYCHAIN")"
    security create-keychain -p "$KEYPASS" "$KEYCHAIN" || return 1
    security set-keychain-settings "$KEYCHAIN"
    security unlock-keychain -p "$KEYPASS" "$KEYCHAIN"
    security import "$work/id.p12" -k "$KEYCHAIN" -P "$KEYPASS" -T /usr/bin/codesign >/dev/null || return 1
    security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KEYPASS" "$KEYCHAIN" >/dev/null
    rm -rf "$work"
}

sign_bundle() {
    if { [ -f "$KEYCHAIN" ] || make_identity; } &&
        security unlock-keychain -p "$KEYPASS" "$KEYCHAIN" 2>/dev/null; then
        # codesign only finds identities in the search list: add ours for
        # the signing and put the list back as it was.
        old="$(security list-keychains -d user | tr -d '"' | xargs)"
        # shellcheck disable=SC2086
        security list-keychains -d user -s $old "$KEYCHAIN"
        hash="$(security find-certificate -c "$IDENTITY" -Z "$KEYCHAIN" | awk '/SHA-1/ {print $3}')"
        ok=0
        codesign --force --entitlements packaging/Deck.entitlements --sign "$hash" "$1" >/dev/null 2>&1 && ok=1
        # shellcheck disable=SC2086
        security list-keychains -d user -s $old
        [ "$ok" = 1 ] && return 0
    fi
    echo "warning: signing with the local certificate failed; macOS will ask for permissions again" >&2
    codesign --force --entitlements packaging/Deck.entitlements --sign - "$1" >/dev/null
}

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
BUNDLE="$STAGE/Deck.app"
mkdir -p "$BUNDLE/Contents/MacOS" "$BUNDLE/Contents/Resources"
cp target/release/deck "$BUNDLE/Contents/MacOS/deck"
cp packaging/Info.plist "$BUNDLE/Contents/Info.plist"
cp packaging/deck.icns "$BUNDLE/Contents/Resources/deck.icns"
sign_bundle "$BUNDLE"

# Quit a running copy first; replacing an app under a live process confuses
# Launch Services.
# Not an AppleScript "quit": that goes to Deck's bundle id, which the
# background service shares, and could stop it. Deck saves its layout
# within a second of any change, so a signal to the app alone is enough.
# Only the app: `deck daemon` (the background service, same binary) keeps
# running, and with it every job, agent and terminal session it holds.
pkill -f "$APP/Contents/MacOS/deck\$" 2>/dev/null || true

rm -rf "$APP"
mv "$BUNDLE" "$APP"
# Let Finder and Spotlight see the new bundle (and its icon) right away.
/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "$APP" >/dev/null 2>&1 || true

# The CLI, so `deck open URL` works from any terminal, not just deck's own.
mkdir -p "$BIN"
ln -sf "$APP/Contents/MacOS/deck" "$BIN/deck"

echo "Installed $APP"
echo "CLI: $BIN/deck"
