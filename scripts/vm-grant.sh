#!/bin/sh
# Runs inside the test VM, as root (`scripts/vm.sh install` does this).
# Pre-grants the engine's privacy permissions so no prompt ever waits for a
# click: the VM's SIP is off, so they go straight into TCC's databases. They
# follow the code signature's designated requirement (bundle id and signing
# certificate), so they survive rebuilds signed the same way.
set -eu
ENGINE="/Users/lume/Library/Application Support/Isocast/Isocast Dev.app"
SYSTEM_DB="/Library/Application Support/com.apple.TCC/TCC.db"
USER_DB="/Users/lume/Library/Application Support/com.apple.TCC/TCC.db"

ID=$(defaults read "$ENGINE/Contents/Info.plist" CFBundleIdentifier)
codesign -d -r- "$ENGINE" 2>&1 | sed -n 's/^designated => //p' | csreq -r- -b /tmp/isocast-req.bin
HEX=$(xxd -p /tmp/isocast-req.bin | tr -d '\n')

grant() { # database service
  sqlite3 "$1" "INSERT OR REPLACE INTO access (service, client, client_type, auth_value, auth_reason, auth_version, csreq, flags) VALUES ('$2', '$ID', 0, 2, 3, 1, X'$HEX', 0);"
}
grant "$SYSTEM_DB" kTCCServiceScreenCapture
grant "$USER_DB" kTCCServiceCamera
grant "$USER_DB" kTCCServiceMicrophone
# Newer macOS also asks again about screen capture now and then, and
# ScreenCaptureKit refuses until that is answered; replayd keeps the answers
# per bundle id. Approve it for a hundred years, as the Cua image does for
# its own driver.
APPROVALS="/Users/lume/Library/Group Containers/group.com.apple.replayd/ScreenCaptureApprovals.plist"
python3 - "$APPROVALS" "$ID" <<'EOF'
import datetime, os, plistlib, sys
path, bundle = sys.argv[1], sys.argv[2]
approvals = {}
if os.path.exists(path):
    with open(path, "rb") as f:
        approvals = plistlib.load(f)
now = datetime.datetime.now(datetime.timezone.utc).replace(tzinfo=None)
approvals[bundle] = {
    "kScreenCaptureAlertableUsageCount": 0,
    "kScreenCaptureApprovalLastAlerted": now,
    "kScreenCaptureApprovalLastUsed": now,
    "kScreenCapturePrivacyHintDate": now.replace(year=now.year + 100),
    "kScreenCapturePrivacyHintPolicy": 3153600000,
}
os.makedirs(os.path.dirname(path), exist_ok=True)
with open(path, "wb") as f:
    plistlib.dump(approvals, f, fmt=plistlib.FMT_BINARY)
EOF
chown lume:staff "$APPROVALS"
# tccd and replayd reread their state when they restart (launchd restarts
# them).
killall tccd replayd 2>/dev/null || true
echo "granted screen recording, camera and microphone to $ID"
