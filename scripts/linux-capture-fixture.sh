#!/usr/bin/env bash
# A headless Linux desktop for the real capture paths (hosted CI and the
# lanes' VMs), with no user session, systemd user units or prompts:
#
#   scripts/linux-capture-fixture.sh deps            apt packages (sudo)
#   scripts/linux-capture-fixture.sh build-portal DIR
#                                                    the patched portal backend
#   scripts/linux-capture-fixture.sh start DIR [WxH] the session; writes
#                                                    DIR/session.env
#   scripts/linux-capture-fixture.sh stop DIR        stop everything it started
#
# start runs, as this user and under $XDG_RUNTIME_DIR:
# - a private D-Bus session bus;
# - PipeWire, WirePlumber and pipewire-pulse;
# - sway on the headless wlroots backend with the pixman renderer (one output,
#   HEADLESS-1, at WxH@30, default 640x360);
# - xdg-desktop-portal with xdg-desktop-portal-wlr, `chooser_type=none` on that
#   output, so ScreenCast never asks;
# - two null sinks: isocast_desktop (the default sink, so system audio
#   captures it) and isocast_mic_feed, remapped as the source isocast_mic (a
#   virtual microphone);
# - v4l2loopback /dev/video30 when the kernel allows it (sudo modprobe).
#   Otherwise session.env says why in ISOCAST_FIXTURE_CAMERA_SKIP.
#
# What plays (the screen picture, tones, the camera feed) belongs to the
# test: scripts/e2e.py's linux_capture scenario starts and stops it.
#
# The portal backend is xdg-desktop-portal-wlr 0.7.1 (Ubuntu 24.04's) built
# with one test-only change. Stock 0.7.1 refuses ScreenCast Start when the
# compositor offers no DMA-BUF format, which a pixman renderer never does;
# the patch requires one only when linux_dmabuf exists (and 0.8.1 refuses
# compositors without linux_dmabuf altogether). It is an external test tool,
# never linked into or shipped with Isocast.
set -euo pipefail

XDPW_VERSION=0.7.1
XDPW_SHA256=4bddc236aa9d1a2b33e7f0bef09a8712ded51e8bc335a2b6c8cd1d088863e676

die() { echo "linux-capture-fixture: $*" >&2; exit 1; }

# wait_for SECONDS DESCRIPTION COMMAND…: poll every 0.1 s.
wait_for() {
    local seconds="$1" what="$2"
    shift 2
    local deadline=$((SECONDS + seconds))
    until "$@" >/dev/null 2>&1; do
        [ "$SECONDS" -lt "$deadline" ] || die "timed out after ${seconds}s waiting for $what"
        sleep 0.1
    done
}

deps() {
    local apt=(sudo apt-get -o Acquire::Retries=3 -o Acquire::http::Timeout=30)
    "${apt[@]}" update
    "${apt[@]}" install -y --no-install-recommends \
        dbus-daemon sway xdg-desktop-portal xdg-desktop-portal-wlr \
        pipewire pipewire-pulse wireplumber pulseaudio-utils mpv ffmpeg v4l-utils \
        meson ninja-build pkg-config libpipewire-0.3-dev libwayland-dev wayland-protocols \
        libinih-dev libgbm-dev libdrm-dev libsystemd-dev
    # The camera: best effort, the scenario skips the camera without it.
    # v4l2loopback needs videodev, which Ubuntu ships in linux-modules-extra.
    "${apt[@]}" install -y --no-install-recommends "linux-modules-extra-$(uname -r)" ||
        echo "linux-capture-fixture: no linux-modules-extra for $(uname -r)" >&2
    if ! "${apt[@]}" install -y --no-install-recommends "linux-headers-$(uname -r)" v4l2loopback-dkms; then
        echo "linux-capture-fixture: v4l2loopback-dkms could not be installed; the camera will be skipped" >&2
    fi
}

build_portal() {
    local dir="$1"
    local out="$dir/xdg-desktop-portal-wlr-$XDPW_VERSION/build/xdg-desktop-portal-wlr"
    [ -x "$out" ] && { echo "$out"; return; }
    mkdir -p "$dir"
    curl -fsSL --retry 3 --max-time 120 \
        "https://github.com/emersion/xdg-desktop-portal-wlr/archive/refs/tags/v$XDPW_VERSION.tar.gz" \
        -o "$dir/xdpw.tar.gz"
    echo "$XDPW_SHA256  $dir/xdpw.tar.gz" | sha256sum -c - >&2
    tar -xf "$dir/xdpw.tar.gz" -C "$dir"
    python3 - "$dir/xdg-desktop-portal-wlr-$XDPW_VERSION/src/screencast/screencast.c" <<'PY'
import sys
from pathlib import Path
p = Path(sys.argv[1])
old = p.read_text()
before = """(cast->ctx->state->screencast_version >= 3 &&
			 cast->screencopy_frame_info[DMABUF].format == DRM_FORMAT_INVALID)"""
after = """(cast->ctx->linux_dmabuf && cast->ctx->state->screencast_version >= 3 &&
			 cast->screencopy_frame_info[DMABUF].format == DRM_FORMAT_INVALID)"""
assert old.count(before) == 1, "xdg-desktop-portal-wlr source changed; update the fixture patch"
p.write_text(old.replace(before, after))
PY
    meson setup "$dir/xdg-desktop-portal-wlr-$XDPW_VERSION/build" "$dir/xdg-desktop-portal-wlr-$XDPW_VERSION" \
        -Dman-pages=disabled -Dsystemd=disabled -Dbuildtype=release >&2
    ninja -C "$dir/xdg-desktop-portal-wlr-$XDPW_VERSION/build" >&2
    echo "$out"
}

# Sets SWAYSOCK and WAYLAND_DISPLAY from sway's sockets; fails until both exist.
sway_sockets() {
    SWAYSOCK="$(find "$XDG_RUNTIME_DIR" -maxdepth 1 -type s -name 'sway-ipc.*.sock' -print -quit)"
    WAYLAND_DISPLAY="$(find "$XDG_RUNTIME_DIR" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' -quit)"
    [ -n "$SWAYSOCK" ] && [ -n "$WAYLAND_DISPLAY" ]
}

# spawn NAME COMMAND…: in the background, its own process group, logged and
# remembered in DIR/pids.
spawn() {
    local name="$1"
    shift
    setsid "$@" >"$dir/logs/$name.log" 2>&1 </dev/null &
    echo "$! $name" >>"$dir/pids"
}

start() {
    dir="$(mkdir -p "$1" && cd "$1" && pwd)"
    local size="${2:-640x360}"
    [ -n "${XDG_RUNTIME_DIR:-}" ] || die "XDG_RUNTIME_DIR must be set"
    mkdir -p "$XDG_RUNTIME_DIR"
    chmod 700 "$XDG_RUNTIME_DIR"
    mkdir -p "$dir/logs" "$dir/config/xdg-desktop-portal" "$dir/config/xdg-desktop-portal-wlr"
    : >"$dir/pids"
    local portal
    portal="$(build_portal "$dir/portal")"

    export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
    export XDG_CURRENT_DESKTOP=sway XDG_SESSION_TYPE=wayland
    spawn dbus dbus-daemon --session --nofork --nopidfile --address="$DBUS_SESSION_BUS_ADDRESS"
    wait_for 10 "the session bus" test -S "$XDG_RUNTIME_DIR/bus"

    spawn pipewire pipewire
    wait_for 10 "PipeWire" test -S "$XDG_RUNTIME_DIR/pipewire-0"
    spawn wireplumber wireplumber
    spawn pipewire-pulse pipewire-pulse
    wait_for 15 "pipewire-pulse" pactl info

    cat >"$dir/sway.conf" <<EOF
output HEADLESS-1 mode ${size}@30Hz
output HEADLESS-1 bg #202020 solid_color
seat seat0 hide_cursor 100
xwayland disable
default_border none
for_window [app_id="isocast-fixture"] fullscreen enable
EOF
    WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_HEADLESS_OUTPUTS=1 WLR_LIBINPUT_NO_DEVICES=1 \
        spawn sway sway -c "$dir/sway.conf"
    wait_for 15 "sway's sockets" sway_sockets
    sway_sockets
    export SWAYSOCK WAYLAND_DISPLAY
    wait_for 10 "sway's output" swaymsg -t get_outputs

    # The patched backend first, so it owns the backend's bus name (the
    # frontend would otherwise activate Ubuntu's stock one).
    printf '[preferred]\ndefault=wlr\norg.freedesktop.impl.portal.ScreenCast=wlr\n' \
        >"$dir/config/xdg-desktop-portal/portals.conf"
    printf '[screencast]\noutput_name=HEADLESS-1\nmax_fps=30\nchooser_type=none\n' \
        >"$dir/config/xdg-desktop-portal-wlr/config"
    XDG_CONFIG_HOME="$dir/config" spawn portal-wlr "$portal" -l INFO
    wait_for 10 "the wlr portal backend" busctl --user status org.freedesktop.impl.portal.desktop.wlr
    XDG_CONFIG_HOME="$dir/config" spawn portal /usr/libexec/xdg-desktop-portal -r
    wait_for 15 "the desktop portal" busctl --user status org.freedesktop.portal.Desktop

    pactl load-module module-null-sink sink_name=isocast_desktop rate=48000 channels=2 \
        sink_properties=device.description=IsocastDesktop >/dev/null
    pactl load-module module-null-sink sink_name=isocast_mic_feed rate=48000 channels=1 \
        sink_properties=device.description=IsocastMicFeed >/dev/null
    pactl load-module module-remap-source master=isocast_mic_feed.monitor source_name=isocast_mic \
        source_properties=device.description=IsocastVirtualMic >/dev/null
    pactl set-default-sink isocast_desktop
    wait_for 10 "the virtual microphone" sh -c 'pactl list short sources | grep -q isocast_mic'

    local camera_skip=
    if ! modinfo v4l2loopback >/dev/null 2>&1; then
        camera_skip="v4l2loopback is not available for kernel $(uname -r)"
    elif ! { sudo -n modprobe videodev && sudo -n modprobe v4l2loopback video_nr=30 \
        card_label=IsocastFixtureCam exclusive_caps=1; } 2>"$dir/logs/modprobe.log"; then
        sudo -n dmesg 2>/dev/null | tail -20 >>"$dir/logs/modprobe.log" || true
        camera_skip="the kernel refused v4l2loopback: $(head -3 "$dir/logs/modprobe.log" | tr '\n' ' ')"
    # A subshell: wait_for dies on timeout, which must skip the camera, not end the fixture.
    elif ! ( wait_for 10 "/dev/video30" test -e /dev/video30 ) 2>/dev/null; then
        camera_skip="v4l2loopback loaded but /dev/video30 did not appear"
    else
        echo "loaded" >"$dir/v4l2loopback"
        # udev sets the new node's mode (root:video 0660 on Ubuntu; the
        # runner's user isn't in `video`, #166), sometimes after a chmod. Let
        # it finish the node's add event first; as root, settle also pings
        # udevd, so an event it hasn't read yet counts.
        sudo -n udevadm settle --timeout=10 || true
        sudo -n chmod 0666 /dev/video30
        test -w /dev/video30 ||
            die "/dev/video30 isn't writable by $(id): $(ls -l /dev/video30 2>&1)"
    fi

    {
        for key in XDG_RUNTIME_DIR DBUS_SESSION_BUS_ADDRESS XDG_CURRENT_DESKTOP XDG_SESSION_TYPE \
            WAYLAND_DISPLAY SWAYSOCK; do
            printf 'export %s=%q\n' "$key" "${!key}"
        done
        printf 'export ISOCAST_FIXTURE_SIZE=%q\n' "$size"
        printf 'export ISOCAST_FIXTURE_MIC=isocast_mic ISOCAST_FIXTURE_MIC_FEED=isocast_mic_feed\n'
        printf 'export ISOCAST_FIXTURE_DESKTOP_SINK=isocast_desktop\n'
        if [ -z "$camera_skip" ]; then
            printf 'export ISOCAST_FIXTURE_CAMERA=/dev/video30\n'
        else
            printf 'export ISOCAST_FIXTURE_CAMERA_SKIP=%q\n' "$camera_skip"
        fi
    } >"$dir/session.env"
    echo "linux-capture-fixture: ready; source $dir/session.env${camera_skip:+ (camera skipped: $camera_skip)}"
}

stop() {
    dir="$(cd "$1" && pwd)"
    if [ -f "$dir/pids" ]; then
        # Newest first: the portal before the bus it uses.
        tac "$dir/pids" | while read -r pid name; do
            kill -TERM -- "-$pid" 2>/dev/null || kill -TERM "$pid" 2>/dev/null || true
            for _ in $(seq 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
            kill -KILL -- "-$pid" 2>/dev/null || true
            echo "linux-capture-fixture: stopped $name"
        done
        rm -f "$dir/pids"
    fi
    if [ -f "$dir/v4l2loopback" ]; then
        sudo -n modprobe -r v4l2loopback || true
        rm -f "$dir/v4l2loopback"
    fi
}

case "${1:-}" in
deps) deps ;;
build-portal) [ $# -eq 2 ] || die "usage: build-portal DIR"; build_portal "$2" >/dev/null ;;
start) [ $# -ge 2 ] || die "usage: start DIR [WxH]"; start "$2" "${3:-640x360}" ;;
stop) [ $# -eq 2 ] || die "usage: stop DIR"; stop "$2" ;;
*) sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
