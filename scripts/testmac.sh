#!/bin/sh
# Run builds and tests on a test Mac instead of Ryan's Mac, so test windows,
# cameras and benchmarks never touch the machine he works on.
#
#   scripts/testmac.sh sync               copy this checkout there (tracked
#                                         files, with uncommitted edits)
#   scripts/testmac.sh build              sync, cargo build --release, sign
#                                         the dev app (scripts/sign-dev.sh)
#   scripts/testmac.sh push [BIN_DIR]     sync, copy already built release
#                                         binaries (isocastd, isocast,
#                                         isocast-ui; default
#                                         $CARGO_TARGET_DIR/release) into
#                                         target/release there and sign the
#                                         dev app: build's result, no rebuild
#   scripts/testmac.sh run '<command>'    sync, then run <command> (one shell
#                                         string) in the checkout there
#   scripts/testmac.sh gui '<command>'    the same, but inside the logged-in
#                                         desktop session (windows, `open`,
#                                         cua-driver): scripts/testmac-bridge.sh
#   scripts/testmac.sh setup              prepare a new test Mac: tools,
#                                         signing keychain, bridge
#   scripts/testmac.sh desktop            EC2 Macs: password, screen sharing,
#                                         automatic login, reboot
#   scripts/testmac.sh vnc                open Screen Sharing to it through
#                                         an SSH tunnel, logged in
#
# The test Mac is $ISOCAST_TESTMAC (an SSH host; default isocast-m2-a).
# The main checkout goes to ~/dev/isocast there, and a worktree to
# ~/dev/isocast-worktrees/<name> with its own dev app (ISOCAST_DEV_APP), so
# agents in different worktrees don't overwrite each other. Scratch output
# goes to ~/isocast-scratch (ISOCAST_SCRATCH, which scripts/bench.py and
# scripts/drag-lag.py use; their lock keeps benchmarks from overlapping).
# Dev apps there share one signing identity, so all have its privacy grants.
set -eu

host="${ISOCAST_TESTMAC:-isocast-m2-a}"
here="$(cd "$(dirname "$0")/.." && pwd)"
name="$(basename "$here")"
if [ "$name" = isocast ]; then
    remote_dir='dev/isocast'
    dev_app=
else
    remote_dir="dev/isocast-worktrees/$name"
    dev_app="export ISOCAST_DEV_APP=\"\$HOME/isocast-scratch/$name/Isocast Dev.app\""
fi
# Over SSH the login keychain stays locked, so the test Mac signs the dev
# app from its own keychain (~/.config/isocast-testmac, a self-signed "Isocast
# Test Signing" identity made by scripts/testmac-setup.sh). A stable
# certificate keeps the app's privacy grants across rebuilds.
remote_env="$dev_app
"'export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH" ISOCAST_SCRATCH="$HOME/isocast-scratch"
mkdir -p "$ISOCAST_SCRATCH"
keys="$HOME/.config/isocast-testmac"
security unlock-keychain -p "$(cat "$keys/keychain-password")" "$keys/signing.keychain-db"
export ISOCAST_SIGN_IDENTITY="$(security find-identity -p codesigning "$keys/signing.keychain-db" |
    awk "/Isocast Test Signing/ { print \$2; exit }")"
cd "$HOME/'"$remote_dir"'"'

sync() {
    ssh "$host" "mkdir -p \"\$HOME/$remote_dir\""
    # Tracked files only (skipping ones deleted in the working tree): no
    # build output, worktrees or private notes.
    git -C "$here" ls-files -z | perl -0 -ne 'chomp; print "$_\0" if -e "'"$here"'/$_"' |
        rsync -a --from0 --files-from=- -e ssh "$here/" "$host:$remote_dir/"
}

case "${1:-}" in
sync)
    sync
    ;;
build)
    sync
    ssh "$host" "$remote_env
cargo build --release 2>&1 | tail -3 && scripts/sign-dev.sh target/release/isocastd"
    ;;
push)
    # Binaries from the build Mac (scripts/remote-build.sh) are the
    # same arch and macOS target as a build there.
    bin_dir="${2:-${CARGO_TARGET_DIR:-$here/target}/release}"
    [ -f "$bin_dir/isocastd" ] || { echo "testmac push: no isocastd in $bin_dir" >&2; exit 2; }
    set --
    for bin in isocastd isocast isocast-ui; do
        [ -f "$bin_dir/$bin" ] && set -- "$@" "$bin_dir/$bin"
    done
    sync
    ssh "$host" "mkdir -p \"\$HOME/$remote_dir/target/release\""
    rsync -az -e ssh "$@" "$host:$remote_dir/target/release/"
    ssh "$host" "$remote_env
scripts/sign-dev.sh target/release/isocastd && echo \"pushed $# binaries to $host:$remote_dir/target/release\""
    ;;
run)
    [ $# -ge 2 ] || { echo "usage: scripts/testmac.sh run '<command>'" >&2; exit 2; }
    sync
    shift
    ssh "$host" "$remote_env
$*"
    ;;
gui)
    [ $# -ge 2 ] || { echo "usage: scripts/testmac.sh gui '<command>'" >&2; exit 2; }
    sync
    shift
    printf '%s\n%s\n' "$remote_env" "$*" |
        ssh "$host" "sh \"\$HOME/$remote_dir/scripts/testmac-bridge.sh\" queue"
    ;;
setup)
    sync
    ssh "$host" "cd \"\$HOME/$remote_dir\" && sh scripts/testmac-setup.sh tools"
    ;;
desktop)
    # The user's password for the desktop session and Screen Sharing lives
    # on Ryan's Mac only.
    password_file="$HOME/.config/isocast-testmac/$host-password"
    mkdir -p "$(dirname "$password_file")"
    [ -f "$password_file" ] || (umask 077 && openssl rand -base64 18 | tr -d '/+=' >"$password_file")
    sync
    ssh "$host" "cd \"\$HOME/$remote_dir\" && sh scripts/testmac-setup.sh desktop" <"$password_file" || true
    echo "rebooting; the desktop session starts by itself (password: $password_file)"
    ;;
vnc)
    # A local port per host, so several sessions can be open at once.
    port=$((5901 + $(printf '%s' "$host" | cksum | cut -d' ' -f1) % 90))
    ssh -f -N -T -o ExitOnForwardFailure=yes -L "$port:localhost:5900" "$host" 2>/dev/null || true
    # The password (base64 letters and digits) logs straight in; it stays in
    # this Mac's URL handling, never on the network outside the tunnel.
    user="$(ssh -G "$host" | awk '/^user / { print $2 }')"
    password_file="$HOME/.config/isocast-testmac/$host-password"
    if [ -f "$password_file" ]; then
        open "vnc://$user:$(cat "$password_file")@localhost:$port"
    else
        open "vnc://$user@localhost:$port"
    fi
    ;;
install-bridge)
    sync
    ssh "$host" "sh \"\$HOME/$remote_dir/scripts/testmac-bridge.sh\" install"
    ;;
*)
    sed -n '2,34p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
