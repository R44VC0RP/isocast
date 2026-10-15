#!/bin/sh
# Isocast's test VM: a macOS VM (Lume, cloned from Ryan's Cua macOS image)
# where the real app and engine run with screen recording, camera and
# microphone pre-granted, so agents can drive the app's window and record
# its screen without any prompt and without touching Ryan's screen.
#
#   scripts/vm.sh up            boot it (headless) and expose it to Cua tools
#   scripts/vm.sh install       build, sign, copy in the engine, app and CLI,
#                               and grant permissions
#   scripts/vm.sh sh CMD...     run a command in it (isocast on PATH,
#                               ISOCAST_MONITOR_SILENT=1)
#   scripts/vm.sh e2e [ARGS]    scripts/e2e.py in it on this checkout's
#                               release isocastd ($ISOCAST_BIN_DIR, default
#                               $CARGO_TARGET_DIR/release), synthetic sources;
#                               ARGS go to e2e.py (--only NAME, --hosted…);
#                               the report comes back to $ISOCAST_VM_OUT
#                               (default /Volumes/IsocastBuild/vm/e2e-<time>)
#   scripts/vm.sh status        state, address and permissions
#   scripts/vm.sh down          shut it down
#   scripts/vm.sh save          make the current VM the clean copy (stops it)
#   scripts/vm.sh reset         replace the VM with a fresh copy of the clean one
#
# Cua tools reach it as the Space `direct:127.0.0.1:13211`: a loopback
# forward to its cua-spacesd (Cua's background service can't reach the VM's
# private address itself without the Local Network permission). Apple's
# hardware encoders work in the VM (paravirtualized), so recordings work;
# performance numbers from the VM don't count — benchmark on the host.
# Over SSH, record through the engine (`isocast start`), not `isocast
# record`: `record` captures in the CLI's own process, which belongs to the
# SSH login and has no screen permission (Cua's shell runs in the GUI login,
# so both work there).
set -eu
cd "$(dirname "$0")/.."

VM=isocast-vm
CLEAN=isocast-vm-clean
HOME_DIR=/Volumes/IsocastBuild/vm
STAGE="$HOME_DIR/stage"
KEY="$HOME/.ssh/isocast-vm"
PORT=13211
# The Cua macOS image's documented default account (not a secret of ours).
VM_USER=lume
VM_PASSWORD=lume

ip() {
  # lume's own state goes stale when the VM was started by an earlier
  # `lume run` process (it then reports "stopped" with no address), so fall
  # back to the host's DHCP lease for the VM's MAC address.
  lume get "$VM" --format json 2>/dev/null | python3 -c '
import json, os, re, sys
d = json.load(sys.stdin)
d = d[0] if isinstance(d, list) else d
ip = d.get("ipAddress") or ""
if not ip:
    try:
        mac = json.load(open(os.path.expanduser("~/.lume/" + sys.argv[1] + "/config.json")))["macAddress"]
        want = ":".join(p.lstrip("0") or "0" for p in mac.lower().split(":"))
        for block in open("/var/db/dhcpd_leases").read().split("}"):
            m = re.search(r"ip_address=(\S+)", block)
            if m and "hw_address=1," + want + "\n" in block:
                ip = m.group(1)
                break
    except (OSError, KeyError, ValueError):
        pass
print(ip)' "$VM"
}

running() {
  lume get "$VM" --format json 2>/dev/null | grep -q '"running"' || pgrep -f "lume run $VM( |$)" >/dev/null
}

ssh_vm() {
  ssh -i "$KEY" -o BatchMode=yes -o ConnectTimeout=5 -o StrictHostKeyChecking=accept-new \
    -o UserKnownHostsFile="$HOME_DIR/known_hosts" "$VM_USER@$(ip)" "$@"
}

wait_for() { # port
  i=0
  until nc -z -G 2 "$(ip)" "$1" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -gt 100 ] && { echo "vm: port $1 never answered" >&2; exit 1; }
    sleep 3
  done
}

forward() {
  pidfile="$HOME_DIR/forward.pid"
  if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile")" 2>/dev/null; then
    kill "$(cat "$pidfile")"
  fi
  nohup socat TCP-LISTEN:$PORT,bind=127.0.0.1,reuseaddr,fork "TCP:$(ip):3211" >/dev/null 2>&1 &
  echo $! >"$pidfile"
  i=0
  until nc -z -G 1 127.0.0.1 "$PORT" 2>/dev/null || [ "$i" -gt 50 ]; do
    i=$((i + 1))
    sleep 0.2
  done
  # Register it with Cua (the token is the image's, kept in Cua's own
  # credentials file; never printed).
  CUA_ENV_TOKEN="$(python3 -c '
import json, os
creds = json.load(open(os.path.expanduser("~/.cua/spaces-credentials.json")))
v = creds.get("local:space-07824e5011") or creds.get("direct:127.0.0.1:13211") or ""
print(v if isinstance(v, str) else (v.get("token") or ""))')" \
    cua spaces add "127.0.0.1:$PORT" --name "$VM" --embedded >/dev/null 2>&1 ||
    echo "vm: couldn't register the Cua space (Cua tools won't reach it; ssh still works)" >&2
}

case "${1:-}" in
up)
  mkdir -p "$HOME_DIR"
  if ! running; then
    lume run "$VM" --detach --display none --log-file "$HOME_DIR/lume.log" >/dev/null
  fi
  wait_for 22
  wait_for 3211
  forward
  echo "vm: $VM is up at $(ip); Cua space direct:127.0.0.1:$PORT"
  ;;
install)
  mkdir -p "$STAGE"
  cargo build --release -p isocast-daemon -p isocast-cli
  ISOCAST_DEV_APP="$STAGE/Isocast Dev.app" scripts/sign-dev.sh target/release/isocastd
  ISOCAST_APP="$STAGE/Isocast.app" scripts/app-dev.sh --release
  cp target/release/isocast "$STAGE/isocast"
  cp scripts/vm-grant.sh "$STAGE/vm-grant.sh"
  ssh_vm 'mkdir -p ~/isocast-stage ~/Applications "Library/Application Support/Isocast"'
  rsync -a --delete -e "ssh -i $KEY -o BatchMode=yes -o UserKnownHostsFile=$HOME_DIR/known_hosts" \
    "$STAGE/" "$VM_USER@$(ip):isocast-stage/"
  ssh_vm "set -e
    export PATH=/usr/local/bin:\$PATH
    osascript -e 'quit app \"Isocast\"' 2>/dev/null || true
    isocast engine stop >/dev/null 2>&1 || true
    S=~/isocast-stage
    rm -rf \"Library/Application Support/Isocast/Isocast Dev.app\" Applications/Isocast.app
    cp -R \"\$S/Isocast Dev.app\" \"Library/Application Support/Isocast/\"
    cp -R \"\$S/Isocast.app\" Applications/
    echo $VM_PASSWORD | sudo -S -p '' install -m 755 \"\$S/isocast\" /usr/local/bin/isocast
    echo $VM_PASSWORD | sudo -S -p '' sh \"\$S/vm-grant.sh\"
    isocast restart >/dev/null
    isocast permissions | grep -E '^(screen|camera|microphone)'"
  ;;
sh)
  shift
  ssh_vm "export PATH=/usr/local/bin:\$PATH ISOCAST_MONITOR_SILENT=1; $*"
  ;;
e2e)
  shift
  bin="${ISOCAST_BIN_DIR:-${CARGO_TARGET_DIR:-target}/release}"
  out="${ISOCAST_VM_OUT:-$HOME_DIR/e2e-$(date +%Y%m%d-%H%M%S)}"
  [ -f "$bin/isocastd" ] || { echo "vm: no $bin/isocastd (cargo build --release -p isocast-daemon)" >&2; exit 2; }
  running || { echo "vm: $VM isn't running; scripts/vm.sh up" >&2; exit 1; }
  mkdir -p "$out"
  ssh_vm 'export PATH=/opt/homebrew/bin:$PATH; command -v ffprobe >/dev/null ||
    HOMEBREW_NO_AUTO_UPDATE=1 brew install ffmpeg >/dev/null; mkdir -p ~/isocast-e2e/src/target/release'
  # Tracked files (e2e.py reads the daemon's Info.plist and the example
  # plugin) and the binary under test.
  git ls-files -z | perl -0 -ne 'chomp; print "$_\0" if -f $_' |
    rsync -a --from0 --files-from=- -e "ssh -i $KEY -o BatchMode=yes -o UserKnownHostsFile=$HOME_DIR/known_hosts" \
      ./ "$VM_USER@$(ip):isocast-e2e/src/"
  rsync -a -e "ssh -i $KEY -o BatchMode=yes -o UserKnownHostsFile=$HOME_DIR/known_hosts" \
    "$bin/isocastd" "$VM_USER@$(ip):isocast-e2e/src/target/release/"
  set +e
  ssh_vm "export PATH=/opt/homebrew/bin:/usr/local/bin:\$PATH; cd ~/isocast-e2e/src && rm -rf ~/isocast-e2e/work &&
    mkdir -p ~/isocast-e2e/work && python3 scripts/e2e.py --daemon target/release/isocastd \
      --work ~/isocast-e2e/work --report ~/isocast-e2e/e2e-report.json $*"
  status=$?
  set -e
  rsync -a -e "ssh -i $KEY -o BatchMode=yes -o UserKnownHostsFile=$HOME_DIR/known_hosts" \
    "$VM_USER@$(ip):isocast-e2e/e2e-report.json" "$out/" || true
  echo "vm: e2e exit $status; report $out/e2e-report.json"
  exit "$status"
  ;;
status)
  if running; then
    echo "vm: $VM running at $(ip)"
    ssh_vm 'export PATH=/usr/local/bin:$PATH; isocast permissions 2>&1 | grep -E "^(screen|camera|microphone)"' || true
  else
    echo "vm: $VM stopped"
  fi
  ;;
down)
  pidfile="$HOME_DIR/forward.pid"
  [ -f "$pidfile" ] && kill "$(cat "$pidfile")" 2>/dev/null || true
  running && lume stop "$VM" >/dev/null
  echo "vm: $VM stopped"
  ;;
save)
  "$0" down
  lume delete "$CLEAN" --force >/dev/null 2>&1 || true
  lume clone "$VM" "$CLEAN" >/dev/null
  echo "vm: saved $VM as $CLEAN"
  ;;
reset)
  "$0" down
  lume delete "$VM" --force >/dev/null
  lume clone "$CLEAN" "$VM" >/dev/null
  echo "vm: $VM is a fresh copy of $CLEAN; \`scripts/vm.sh up\` to boot it"
  ;;
*)
  sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
  ;;
esac
