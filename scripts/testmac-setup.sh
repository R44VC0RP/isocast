#!/bin/sh
# Prepare a Mac to be an Isocast test Mac (run there, from the synced
# checkout, by `scripts/testmac.sh setup` and `scripts/testmac.sh desktop`).
#
#   testmac-setup.sh tools     Rust, ffmpeg, a numpy venv, the signing
#                              keychain and the desktop-session bridge.
#                              Safe to run again.
#   testmac-setup.sh desktop   EC2 Macs only (needs passwordless sudo): grow
#                              the disk to the EBS volume, give the user the
#                              password read from stdin, turn on screen
#                              sharing and automatic login, never sleep or
#                              lock, then reboot into the desktop session
#                              (the bridge loads at login).
#
# Privacy grants (screen recording, camera, microphone, cua-driver's
# Accessibility) still need a person at the desktop once, over Screen
# Sharing; see "Test Mac" in docs/archive/2026-10-22/notes.md.
set -eu

keys="$HOME/.config/isocast-testmac"
keychain="$keys/signing.keychain-db"
scratch="$HOME/isocast-scratch"

tools() {
    export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
    if ! command -v rustup >/dev/null; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal -c clippy,rustfmt
    fi
    rustup update stable >/dev/null
    rustup component add clippy rustfmt >/dev/null
    command -v ffmpeg >/dev/null || HOMEBREW_NO_AUTO_UPDATE=1 brew install ffmpeg >/dev/null
    mkdir -p "$scratch/main"
    [ -x "$scratch/main/venv/bin/python" ] || {
        python3 -m venv "$scratch/main/venv"
        "$scratch/main/venv/bin/pip" -q install numpy
    }

    # Over SSH the login keychain stays locked, so dev apps are signed from
    # a keychain of our own with a self-signed identity; privacy grants are
    # tied to that certificate and survive rebuilds.
    mkdir -p "$keys"
    chmod 700 "$keys"
    [ -f "$keys/keychain-password" ] || (umask 077 && openssl rand -hex 24 >"$keys/keychain-password")
    password="$(cat "$keys/keychain-password")"
    [ -f "$keychain" ] || security create-keychain -p "$password" "$keychain"
    security set-keychain-settings "$keychain"
    security unlock-keychain -p "$password" "$keychain"
    if ! security find-identity -p codesigning "$keychain" | grep -q "Isocast Test Signing"; then
        work="$(mktemp -d)"
        cat >"$work/cert.cfg" <<EOF
[req]
distinguished_name=dn
x509_extensions=ext
prompt=no
[dn]
CN=Isocast Test Signing
[ext]
basicConstraints=critical,CA:false
keyUsage=critical,digitalSignature
extendedKeyUsage=critical,codeSigning
EOF
        openssl req -x509 -newkey rsa:2048 -nodes -keyout "$work/key.pem" -out "$work/cert.pem" \
            -days 3650 -config "$work/cert.cfg" 2>/dev/null
        export_password="$(openssl rand -hex 12)"
        openssl pkcs12 -export -legacy -inkey "$work/key.pem" -in "$work/cert.pem" -out "$work/id.p12" \
            -passout "pass:$export_password" 2>/dev/null ||
            openssl pkcs12 -export -inkey "$work/key.pem" -in "$work/cert.pem" -out "$work/id.p12" \
                -passout "pass:$export_password"
        security import "$work/id.p12" -k "$keychain" -P "$export_password" -T /usr/bin/codesign >/dev/null
        security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$password" "$keychain" >/dev/null
        rm -rf "${work:?}"
    fi
    # codesign only finds identities in keychains on the search list.
    if ! security list-keychains -d user | grep -q "$keychain"; then
        # shellcheck disable=SC2046
        security list-keychains -d user -s $(security list-keychains -d user | tr -d '"') "$keychain"
    fi

    sh "$(dirname "$0")/testmac-bridge.sh" install >/dev/null
    echo "tools ready: $(rustc --version), $(ffmpeg -version | head -n 1 | cut -d' ' -f1-3)"
}

desktop() {
    user="$(id -un)"
    IFS= read -r password
    [ -n "$password" ] || { echo "desktop: pass the user's new password on stdin" >&2; exit 2; }

    # EC2: the APFS container may start at the AMI's size; grow it to the
    # volume (newer AMIs already have; diskutil then refuses, harmlessly).
    disk="$(diskutil list physical external | awk '/^\/dev\// { print $1; exit }')"
    container="$(diskutil list physical external | awk '/Apple_APFS/ { print $NF; exit }')"
    if [ -n "$disk" ] && [ -n "$container" ]; then
        yes | sudo diskutil repairDisk "$disk" >/dev/null 2>&1 || true
        sudo diskutil apfs resizeContainer "$container" 0 >/dev/null 2>&1 || true
    fi

    # Set once: changing an existing password needs the old one.
    dscl . -authonly "$user" "$password" 2>/dev/null || sudo dscl . -passwd "/Users/$user" "$password"
    sudo launchctl enable system/com.apple.screensharing
    sudo launchctl load -w /System/Library/LaunchDaemons/com.apple.screensharing.plist 2>/dev/null || true
    sudo sysadminctl -autologin set -userName "$user" -password "$password" 2>/dev/null || true
    # sysadminctl can leave out /etc/kcpassword (seen on EC2 macOS 26 and 27),
    # and loginwindow then stops at the login screen: write it ourselves (the
    # password XORed with loginwindow's fixed key, null-padded to 12 bytes).
    if ! sudo test -s /etc/kcpassword; then
        printf '%s' "$password" | /usr/bin/python3 -c '
import sys
key = [125, 137, 82, 35, 210, 188, 221, 234, 163, 185, 31]
data = list(sys.stdin.buffer.read())
data += [0] * (12 - len(data) % 12)
sys.stdout.buffer.write(bytes(b ^ key[i % len(key)] for i, b in enumerate(data)))
' | sudo tee /etc/kcpassword >/dev/null
        sudo chmod 600 /etc/kcpassword
        sudo defaults write /Library/Preferences/com.apple.loginwindow autoLoginUser "$user"
    fi
    sudo pmset -a sleep 0 displaysleep 0 disksleep 0
    defaults -currentHost write com.apple.screensaver idleTime 0
    sysadminctl -screenLock off -password "$password" 2>/dev/null || true
    df -h / | awk 'NR == 2 { print "disk: " $2 " (" $4 " free)" }'
    echo "rebooting into the desktop session"
    sudo shutdown -r now
}

case "${1:-}" in
tools) tools ;;
desktop) desktop ;;
*)
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
