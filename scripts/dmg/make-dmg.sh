#!/bin/bash
# Build the installer DMG with the pinned dmgbuild and scripts/dmg/settings.py.
# Usage: make-dmg.sh --check | make-dmg.sh APP VOLUME_NAME OUTPUT.dmg
# Uses python3 when it already has the exact pinned dmgbuild (CI installs it
# with pip --require-hashes). Otherwise installs requirements.txt with
# --require-hashes into a private venv keyed by that file's SHA-256 (under
# ISOCAST_DMGBUILD_CACHE, default ~/Library/Caches/isocast/dmgbuild), with uv
# or Python 3.10+'s venv/pip; never into the system Python.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
self="$here/$(basename "$0")"
requirements="$here/requirements.txt"
pin="$(sed -n 's/^dmgbuild==\([0-9.]*\).*/\1/p' "$requirements")"
die() { echo "make-dmg: $*" >&2; exit 1; }

# A usable venv runs, has every exact pin from requirements.txt, and can
# actually start dmgbuild (metadata alone survives a deleted package).
valid_venv() {
    "$1/bin/python" - "$requirements" <<'PY' 2>/dev/null &&
import importlib.metadata as m, re, sys
pins = re.findall(r"^([A-Za-z0-9_.-]+)==([^ \\]+)", open(sys.argv[1]).read(), re.M)
sys.exit(not pins or any(m.version(n) != v for n, v in pins))
PY
        "$1/bin/python" -c 'import ds_store, mac_alias' 2>/dev/null &&
        "$1/bin/python" -m dmgbuild --help >/dev/null 2>&1
}

# Runs under lockf, one installer per key: reuse a valid entry (perhaps
# just published by another installer), else install with hashes into
# staging and atomically replace whatever invalid entry is at the key.
install_venv() {
    local env="$1" cache
    cache="$(dirname "$env")"
    if valid_venv "$env"; then return 0; fi
    # Invocation-owned staging; removed on every exit, including failures.
    staging="$(mktemp -d "$cache/.install.XXXXXX")"
    trap 'if [ -n "${staging:-}" ]; then rm -rf "${staging:?}"; fi' EXIT
    if command -v uv >/dev/null; then
        uv venv --quiet --python '>=3.10' "$staging/venv" &&
            uv pip install --quiet --python "$staging/venv/bin/python" \
                --require-hashes --no-deps -r "$requirements"
    elif python3 -c 'import sys; sys.exit(sys.version_info < (3, 10))' 2>/dev/null; then
        python3 -m venv "$staging/venv" &&
            "$staging/venv/bin/python" -m pip install --quiet --disable-pip-version-check \
                --require-hashes --no-deps -r "$requirements"
    else
        die "need dmgbuild==$pin: pip install --require-hashes -r scripts/dmg/requirements.txt, or install uv or Python 3.10+"
    fi || die "hash-verified install of $requirements failed"
    valid_venv "$staging/venv" || die "installed venv is not usable: $staging/venv"
    if [ -e "$env" ] || [ -L "$env" ]; then
        # Never move into an existing entry (that nests); set it aside first.
        mv "$env" "$staging/stale" || die "could not replace invalid cache entry $env"
    fi
    mv "$staging/venv" "$env" || die "could not publish $env"
}

hashed_venv() {
    local key cache env
    key="$(shasum -a 256 "$requirements" | cut -c1-16)"
    cache="${ISOCAST_DMGBUILD_CACHE:-$HOME/Library/Caches/isocast/dmgbuild}"
    env="$cache/$key"
    if ! valid_venv "$env"; then
        mkdir -p "$cache"
        /usr/bin/lockf -k -t 900 "$cache/.$key.lock" "$self" --install-venv "$env" ||
            exit 1
        valid_venv "$env" || die "cached venv is not usable: $env"
    fi
    dmgbuild=("$env/bin/python" -m dmgbuild)
}

if [ "${1:-}" = --install-venv ]; then
    install_venv "$2"
    exit 0
fi
if python3 -c 'import importlib.metadata as m,sys; sys.exit(m.version("dmgbuild")!=sys.argv[1])' "$pin" 2>/dev/null; then
    dmgbuild=(python3 -m dmgbuild)
else
    hashed_venv
fi
[ -x /usr/bin/tiffutil ] || die 'missing /usr/bin/tiffutil (HiDPI background)'

if [ "${1:-}" = --check ]; then
    "${dmgbuild[@]}" --help >/dev/null || die "dmgbuild==$pin is not runnable"
    echo "make-dmg: dmgbuild $pin via ${dmgbuild[0]}"
    exit 0
fi
[ $# = 3 ] || die 'usage: make-dmg.sh APP VOLUME_NAME OUTPUT.dmg'
app="$1"
[ -d "$app/Contents" ] || die "not an app bundle: $app"
icon="$app/Contents/Resources/Isocast.icns"
[ -f "$icon" ] || die "missing volume icon: $icon"
"${dmgbuild[@]}" -s "$here/settings.py" -D "app=$app" -D "icon=$icon" \
    -D "background=$here/background.png" "$2" "$3"
