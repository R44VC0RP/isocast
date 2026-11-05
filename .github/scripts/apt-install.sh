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
    # Noble amd64 points its deb822 URIs at a priority list, not at Azure
    # directly. Match whole Ubuntu URI tokens in deb822 and legacy sources.
    sudo python3 - "$1" <<'PY'
from pathlib import Path
import re
import sys

mirror = sys.argv[1]
uri = re.compile(r'(?<=\s)(?:mirror\+file:/+etc/apt/apt-mirrors\.txt|https?://(?:'
                 r'(?:[\w-]+\.)*archive\.ubuntu\.com/ubuntu|security\.ubuntu\.com/ubuntu|'
                 r'ports\.ubuntu\.com/ubuntu-ports|mirrors\.edge\.kernel\.org/ubuntu|'
                 r'mirrors\.mit\.edu/ubuntu-ports))/?(?=\s|$)')

def replace(match):
    old = match[0]
    if old.partition(':')[2].rstrip('/') == mirror.partition(':')[2]:
        return old  # changing only the scheme would retry the same host
    return mirror

apt = Path('/etc/apt')
for path in [apt / 'sources.list', *apt.glob('sources.list.d/*.list'), *apt.glob('sources.list.d/*.sources')]:
    if path.is_file():
        old = path.read_text()
        new = ''.join(uri.sub(replace, line) if re.match(r'^\s*(?:URIs:|deb(?:-src)?)\s', line)
                      else line for line in old.splitlines(keepends=True))
        if new != old:
            path.write_text(new)
            print(path)
PY
}

case $(dpkg --print-architecture) in
    # The hosted arm64 image starts on ports.ubuntu.com already.
    arm64) mirrors=(https://mirrors.mit.edu/ubuntu-ports https://ports.ubuntu.com/ubuntu-ports) ;;
    *) mirrors=(https://archive.ubuntu.com/ubuntu https://mirrors.edge.kernel.org/ubuntu) ;;
esac

start=$SECONDS
for mirror in '' "${mirrors[@]}"; do
    if [[ -n $mirror ]]; then
        changed=$(switch_mirror "$mirror")
        [[ -n $changed ]] || continue
        echo "apt-install: switched to $mirror"
        rm -f "$marker"
    fi
    if [[ ! -f $marker ]]; then
        if ! update; then
            echo '::warning::apt-install: mirror update failed (limit 180s)'
            continue
        fi
        touch "$marker"
    fi
    if [[ -z $mirror && ${APT_FORCE_FIRST_DOWNLOAD_FAILURE:-0} == 1 ]]; then
        echo 'apt-install: forcing first download failure'
    elif timeout "$limit" "${apt[@]}" "${install[@]}" --download-only "$@"; then
        echo "apt-install: packages downloaded in $((SECONDS - start))s"
        # Use the normal install path: --no-download breaks apt's local .deb handling.
        exec "${apt[@]}" "${install[@]}" "$@"
    else
        echo "::warning::apt-install: slow or failed mirror download (limit ${limit}s)"
    fi
done
echo 'apt-install: exhausted Ubuntu mirrors' >&2
exit 1
