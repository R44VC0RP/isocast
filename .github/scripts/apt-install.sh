#!/bin/bash
# Download before installing: a mirror can trickle data indefinitely without
# hitting apt's HTTP timeout. Never interrupt dpkg to change mirrors.
set -euo pipefail

if [[ $# == 0 || $1 == -* ]]; then
    echo 'usage: apt-install.sh PKG...' >&2
    exit 2
fi

apt=(sudo apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30)
install=(install -y --no-install-recommends)
limit=${APT_DOWNLOAD_SECONDS:-240}
marker=${RUNNER_TEMP:-/tmp}/apt-install.updated

update() {
    timeout 180 "${apt[@]}" -o APT::Update::Error-Mode=any update
}

switch_mirror() {
    local f changed=
    for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.sources /etc/apt/sources.list.d/*.list; do
        if [ -f "$f" ] && grep -q 'azure\.archive\.ubuntu\.com' "$f"; then
            sudo sed -i 's#azure\.archive\.ubuntu\.com#archive.ubuntu.com#g' "$f"
            changed=1
        fi
    done
    if [[ -n $changed ]]; then
        echo 'apt-install: switched to archive.ubuntu.com' >&2
    else
        echo 'apt-install: no alternate mirror for this image; retrying the same mirror' >&2
        [[ ! -f $marker ]] || return 0
    fi
    rm -f "$marker"
    update
    touch "$marker"
}

if [[ ! -f $marker ]]; then
    if update; then
        touch "$marker"
    else
        echo "::warning::apt-install: mirror update failed after ${SECONDS}s (limit 180s)"
        switch_mirror
    fi
fi

start=$SECONDS
if ! timeout "$limit" "${apt[@]}" "${install[@]}" --download-only "$@"; then
    echo "::warning::apt-install: slow or failed mirror download after $((SECONDS - start))s (limit ${limit}s)"
    switch_mirror
    timeout "$limit" "${apt[@]}" "${install[@]}" --download-only "$@"
fi
echo "apt-install: packages downloaded in $((SECONDS - start))s"
# Use the normal install path: --no-download breaks apt's local .deb handling.
"${apt[@]}" "${install[@]}" "$@"
