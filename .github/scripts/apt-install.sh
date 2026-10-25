#!/bin/bash
# apt-get install for hosted Ubuntu jobs, robust to a slow mirror.
#
#   .github/scripts/apt-install.sh restore DIR   cached .debs from DIR into apt's cache
#   .github/scripts/apt-install.sh PKG...        update (once per job) and install
#   .github/scripts/apt-install.sh harvest DIR   apt's .debs (stale ones dropped) to DIR
#
# The runners' Azure mirror sometimes serves at ~250 kB/s: 200 MB of
# packages then take 14 min and the step times out (three times on #144,
# 2026-10-25), while `apt-get update` stays fast. So packages are first
# downloaded under a time limit; if that runs out (or fails), the rest
# comes from the main Ubuntu archive, keeping what already arrived, and
# only then installed. With the .deb cache (DIR, restored and saved by
# actions/cache) a warm job downloads only what changed.
#
# APT_DOWNLOAD_SECONDS: the first mirror's limit (default 240; 200 MB at
# a normal 5-10 MB/s takes 20-60 s).
set -euo pipefail

apt=(sudo apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30 -o APT::Keep-Downloaded-Packages=true)
archives=/var/cache/apt/archives
limit="${APT_DOWNLOAD_SECONDS:-240}"
marker="${RUNNER_TEMP:-/tmp}/apt-install.updated"

say() { echo "apt-install: $*" >&2; }

# The hosted images' mirrors and the fallback for each (arm64 runners use
# ports.ubuntu.com, which has no second official host: retried as is).
switch_mirror() {
    local f changed=
    for f in /etc/apt/sources.list /etc/apt/sources.list.d/*.sources /etc/apt/sources.list.d/*.list; do
        if [ ! -f "$f" ] || ! grep -q 'azure\.archive\.ubuntu\.com' "$f"; then continue; fi
        sudo sed -i 's#azure\.archive\.ubuntu\.com#archive.ubuntu.com#g' "$f"
        changed=1
    done
    if [ -n "$changed" ]; then
        say "switched to archive.ubuntu.com"
        rm -f "$marker"
        update
    else
        say "no other mirror for this image; retrying the same one"
    fi
}

update() {
    [ -f "$marker" ] && return 0
    if ! timeout 180 "${apt[@]}" update; then
        say "apt-get update failed or took over 180 s"
        [ -n "${switched:-}" ] && return 1
        switched=1
        switch_mirror
        return 0
    fi
    touch "$marker"
}

case "${1:-}" in
restore)
    dir="$2"
    n=0
    if [ -d "$dir" ]; then
        n=$(find "$dir" -maxdepth 1 -name '*.deb' | wc -l)
        [ "$n" = 0 ] || sudo cp "$dir"/*.deb "$archives"/
    fi
    say "restored $n cached .debs"
    exit 0
    ;;
harvest)
    dir="$2"
    # Drop .debs no longer in the index (superseded versions).
    sudo apt-get autoclean -y >/dev/null
    mkdir -p "$dir"
    find "$dir" -maxdepth 1 -name '*.deb' -delete
    n=$(find "$archives" -maxdepth 1 -name '*.deb' | wc -l)
    [ "$n" = 0 ] || cp "$archives"/*.deb "$dir"/
    say "harvested $n .debs ($(du -sh "$dir" | cut -f1)) for the cache"
    exit 0
    ;;
'' | -*)
    echo "usage: apt-install.sh restore DIR | PKG... | harvest DIR" >&2
    exit 2
    ;;
esac

switched=
update
start=$SECONDS
if ! timeout "$limit" "${apt[@]}" install -y --no-install-recommends --download-only "$@"; then
    say "downloading took over ${limit} s or failed ($((SECONDS - start)) s); finishing from another mirror"
    [ -n "$switched" ] || { switched=1; switch_mirror; }
    "${apt[@]}" install -y --no-install-recommends --download-only "$@"
fi
say "packages downloaded in $((SECONDS - start)) s"
"${apt[@]}" install -y --no-install-recommends "$@"
