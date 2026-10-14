#!/bin/bash
# Sparkle 2 for the released app's self-update (#40). Nothing is vendored in
# git: the official release archive is downloaded at a pinned version and
# SHA-256, extracted once into a cache and reused.
#
#   sparkle.sh path        print the verified, extracted distribution
#                          (Sparkle.framework, bin/sign_update, bin/generate_appcast)
#   sparkle.sh embed APP   copy Sparkle.framework into APP/Contents/Frameworks,
#                          arm64 only and without the XPC services, which only
#                          sandboxed apps use (Sparkle's sandboxing guide), and
#                          its licence to Contents/Resources/LICENSE-Sparkle
#   sparkle.sh public-key  print the app's SUPublicEDKey from public-ed-key.txt;
#                          fails until Ryan has replaced the placeholder
#
# The cache is ISOCAST_SPARKLE_CACHE (default ~/Library/Caches/isocast/sparkle),
# keyed by version and hash, filled under a lock by one process at a time.
set -euo pipefail

version=2.10.0
sha256=c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c
url="https://github.com/sparkle-project/Sparkle/releases/download/$version/Sparkle-$version.tar.xz"
here="$(cd "$(dirname "$0")" && pwd)"
self="$here/$(basename "$0")"
cache="${ISOCAST_SPARKLE_CACHE:-$HOME/Library/Caches/isocast/sparkle}"
entry="$cache/$version-${sha256:0:16}"
die() { echo "sparkle: $*" >&2; exit 1; }

valid() {
    [ -f "$1/.verified" ] && [ "$(cat "$1/.verified")" = "$sha256" ] &&
        [ -x "$1/bin/sign_update" ] && [ -x "$1/bin/generate_appcast" ] &&
        [ -f "$1/Sparkle.framework/Versions/B/Sparkle" ]
}

install_entry() {
    if valid "$entry"; then return 0; fi
    staging="$(mktemp -d "$cache/.install.XXXXXX")"
    trap 'if [ -n "${staging:-}" ]; then rm -rf "${staging:?}"; fi' EXIT
    curl --fail --silent --show-error --location --retry 3 --max-time 300 \
        -o "$staging/sparkle.tar.xz" "$url" || die "download failed: $url"
    actual="$(shasum -a 256 "$staging/sparkle.tar.xz" | cut -d' ' -f1)"
    [ "$actual" = "$sha256" ] || die "SHA-256 mismatch for $url: got $actual, pinned $sha256"
    mkdir "$staging/dist"
    tar -xf "$staging/sparkle.tar.xz" -C "$staging/dist"
    printf '%s' "$sha256" > "$staging/dist/.verified"
    valid "$staging/dist" || die "the archive lacks Sparkle.framework or its tools"
    if [ -e "$entry" ]; then mv "$entry" "$staging/stale"; fi
    mv "$staging/dist" "$entry"
}

dist() {
    if ! valid "$entry"; then
        mkdir -p "$cache"
        /usr/bin/lockf -k -t 600 "$cache/.lock" "$self" --install || exit 1
        valid "$entry" || die "cached distribution is not usable: $entry"
    fi
    echo "$entry"
}

case "${1:-}" in
    --install) install_entry ;;
    path) dist ;;
    embed)
        app="${2:?usage: sparkle.sh embed APP}"
        source="$(dist)/Sparkle.framework"
        frameworks="$app/Contents/Frameworks"
        [ ! -e "$frameworks/Sparkle.framework" ] || die "$frameworks/Sparkle.framework exists"
        mkdir -p "$frameworks"
        ditto "$source" "$frameworks/Sparkle.framework"
        framework="$frameworks/Sparkle.framework"
        rm -rf "${framework:?}/Versions/B/XPCServices" "${framework:?}/XPCServices"
        for binary in Versions/B/Sparkle Versions/B/Autoupdate Versions/B/Updater.app/Contents/MacOS/Updater; do
            lipo "$framework/$binary" -thin arm64 -output "$framework/$binary.arm64"
            mv "$framework/$binary.arm64" "$framework/$binary"
        done
        # Its licence (MIT, with bsdiff/sais/ed25519 notices) beside ours.
        cp "$(dist)/LICENSE" "$app/Contents/Resources/LICENSE-Sparkle"
        echo "embedded Sparkle $version (arm64, no XPC services): $framework"
        ;;
    public-key)
        key="$(grep -v '^#' "$here/public-ed-key.txt" | tr -d '[:space:]')"
        python3 -c 'import base64,binascii,sys
try: ok=len(base64.b64decode(sys.argv[1],validate=True))==32
except (binascii.Error,ValueError): ok=False
sys.exit(not ok)' "$key" 2>/dev/null ||
            die "scripts/sparkle/public-ed-key.txt has no EdDSA public key yet. Ryan: run Sparkle's generate_keys and paste its public key there (docs/releasing.md, 'Sparkle update key')"
        echo "$key"
        ;;
    *) die "usage: sparkle.sh path | embed APP | public-key" ;;
esac
