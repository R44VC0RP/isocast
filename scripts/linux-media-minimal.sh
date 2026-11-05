#!/bin/bash
# On Ubuntu 24.04 (the build host), verify the engine independently of any
# GStreamer installed by FFmpeg. Decode the resulting files outside the image.
set -euo pipefail
bin=$(realpath "${1:-target/release}")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
ffmpeg -nostdin -v error -f lavfi -i sine -t 1 "$work/clip.wav"
sed "s|/tmp/isocast-linux-app/recordings|/take|" packaging/linux/test-setup.toml > "$work/healthy.toml"
"${CONTAINER_CLI:-docker}" run --rm -i -v "$bin:/app:ro" -v "$work:/work" \
  -v "$(realpath "${ISOCAST_OPENH264:?provide the verified Cisco library}"):/codec.so:ro" \
  -e ISOCAST_OPENH264=/codec.so -e ISOCAST_CALIBRATION=off ubuntu:24.04 bash -se <<'SH'
set -o pipefail
apt-get -qq update >&2
apt-get install --no-install-recommends -qy libpipewire-0.3-0t64 libvulkan1 mesa-vulkan-drivers python3 </dev/null >&2
libs=$(ldconfig -p)
if grep -q libgstreamer <<< "$libs"; then echo 'not a GStreamer-free image' >&2; exit 1; fi
export PATH=/app:$PATH XDG_RUNTIME_DIR=/tmp/runtime
mkdir -m700 "$XDG_RUNTIME_DIR"
isocast doctor --json | python3 -c 'import sys,json; c=next(c for c in json.load(sys.stdin)["checks"] if c["name"]=="media decoders (GStreamer)"); print(c,file=sys.stderr); assert c["status"]=="warning" and "media file sources unavailable" in c["detail"]'
isocast apply --revision 0 /work/healthy.toml >&2
isocast start --json >&2
sleep 1
cp /work/healthy.toml /tmp/clip.toml
cat >> /tmp/clip.toml <<'TOML'
[[sources]]
id = "clip"
kind = "media"
files = ["/work/clip.wav"]
[[scenes]]
id = "unused"
items = [{source = "clip", rect = {x = 0, y = 0, width = 100, height = 100}}]
TOML
isocast apply --revision 1 /tmp/clip.toml >&2
python3 - <<'PY'
import json, subprocess, time
for _ in range(40):
    sources = json.loads(subprocess.check_output(['isocast','status','--json']))['engine']['sources']
    err = next((s.get('error') for s in sources if s['source']=='clip' and s['state']=='failed'), None)
    if err: break
    time.sleep(.25)
assert err and 'isocast-media' in err, sources
print(err, file=__import__('sys').stderr)
PY
isocast stop --wait --json >&2
isocast engine shutdown >&2
cd /take
find . -name '*.mov' -print0 | tar --null -T - -cf /work/files.tar
SH
tar -C "$work" -xf "$work/files.tar"
mapfile -d '' files < <(find "$work" -name '*.mov' -print0)
test "${#files[@]}" = 3
for f in "${files[@]}"; do
  counts=$(ffprobe -v error -count_frames -show_entries stream=codec_name,nb_read_frames -of csv=p=0 "$f")
  echo "$counts"
  grep -Eq '^h264,[1-9][0-9]*$' <<< "$counts"
  grep -Eq '^aac,[1-9][0-9]*$' <<< "$counts"
done
