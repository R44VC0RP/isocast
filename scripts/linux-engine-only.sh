#!/bin/bash
# Exercise the shipped tarball, not a target directory. Keep FFmpeg and its
# transitive desktop dependencies on the runner, outside the engine image.
set -euo pipefail
tarball=$(realpath "${1:?usage: linux-engine-only.sh TAR_GZ}")
work=$(mktemp -d)
docker=${CONTAINER_CLI:-docker}
engine="isocast-engine-only-$$"
listener=
cleanup() {
  if [ -n "$listener" ]; then kill "$listener" 2>/dev/null || true; wait "$listener" 2>/dev/null || true; fi
  "$docker" rm -f "$engine" >/dev/null || true
  rm -rf "$work"
}
"$docker" run -d --name "$engine" --network host -v "$tarball:/package.tar.gz:ro" debian:12-slim sleep infinity >/dev/null
trap cleanup EXIT
"$docker" exec "$engine" sh -ec '
  apt-get -qq update
  apt-get -qy install --no-install-recommends libssl3 libgcc-s1 libvulkan1 mesa-vulkan-drivers curl ca-certificates bzip2 >/dev/null
  libs=$(/sbin/ldconfig -p)
  if echo "$libs" | grep -E "lib(gst|pipewire|gtk[-_]|webkit|javascriptcoregtk)"; then exit 1; fi
  install -d -m 700 -o nobody /tmp/server /tmp/server/runtime
'
server() {
  "$docker" exec -i -u nobody -w /tmp/server -e HOME=/tmp/server -e XDG_RUNTIME_DIR=/tmp/server/runtime \
    -e PATH=/tmp/server/.local/bin:/usr/bin:/bin -e ISOCAST_CALIBRATION=off -e ISOCAST_STREAM_KEY=smoke "$engine" "$@"
}
server sh -es <<'INSTALL'
tar xzf /package.tar.gz
archive=isocast-linux-$(uname -m)
"$archive/install.sh" --engine-only
rm -r "$archive" # subsequent commands must use the installed payload
test -x "$HOME/.local/bin/isocastd"
test -x "$HOME/.local/bin/isocast"
test -x "$HOME/.local/lib/isocast/isocast-media"
test ! -e "$HOME/.local/lib/isocast/isocast-ui"
test ! -L "$HOME/.local/bin/isocast-ui"
test ! -e "$HOME/.local/share/applications/dev.isocast.Isocast.desktop"
test ! -d "$HOME/.local/share/icons"
INSTALL
server isocast doctor --json > "$work/doctor.json"
python3 - "$work/doctor.json" <<'PY'
import json, sys
checks = {c['name']: c for c in json.load(open(sys.argv[1]))['checks']}
for name, hint in [('media decoders (GStreamer)', 'GStreamer'), ('PipeWire capture and playback','libpipewire'),
                   ('browser sources','WebKitGTK')]:
    c = checks[name]
    print(f"{name}: {c['detail']}")
    assert c['status'] == 'warning' and hint in c['detail'] and c.get('fix'), c
PY

port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
server sh -c 'cat > take.toml && isocast apply --revision 0 take.toml && isocast start --json' <<TOML
version = 1
name = "Headless stream"
canvas = { width = 320, height = 180, fps = 30 }
[recording]
folder = "/tmp/server/takes"
program = { encoder = {codec = "h264"}, audio = ["tone"] }
[stream]
service = "custom"
server = "rtmp://127.0.0.1:$port/live"
video_bitrate_kbps = 1000
audio_bitrate_kbps = 128
width = 320
height = 180
[[sources]]
id = "bars"
kind = "test_pattern"
width = 320
height = 180
fps = 30
[[sources]]
id = "tone"
kind = "tone"
[[scenes]]
id = "main"
items = [{source = "bars", rect = {x = 0, y = 0, width = 320, height = 180}}]
TOML
timeout 45s ffmpeg -nostdin -hide_banner -v error -listen 1 -i "rtmp://127.0.0.1:$port/live/smoke" \
  -c copy "$work/received.flv" >"$work/listener.log" 2>&1 &
listener=$!
for _ in {1..50}; do
  if ss -Hltn "sport = :$port" | grep -q LISTEN; then break; fi
  kill -0 "$listener" || { cat "$work/listener.log" >&2; exit 1; }
  sleep 0.1
done
ss -Hltn "sport = :$port" | grep -q LISTEN || { echo 'RTMP receiver did not start' >&2; exit 1; }
server isocast stream start --json
sleep 5
server isocast stream stop --json
server isocast stop --wait --json
wait "$listener" || { cat "$work/listener.log" >&2; exit 1; }
listener=
server isocast engine shutdown
server tar -cf - takes | tar -C "$work" -xf -
mapfile -d '' takes < <(find "$work/takes" -name '*.mov' -print0)
test "${#takes[@]}" = 1
for file in "${takes[0]}" "$work/received.flv"; do
  counts=$(ffprobe -v error -count_frames -show_entries stream=codec_name,nb_read_frames -of csv=p=0 "$file")
  echo "${file##*/}: $counts"
  grep -Eq '^h264,[1-9][0-9]*$' <<< "$counts"
  grep -Eq '^aac,[1-9][0-9]*$' <<< "$counts"
done
