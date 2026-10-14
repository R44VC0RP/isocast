#!/bin/bash
# Deactivate only Isocast's extension, then remove only its verified host app.
set -euo pipefail
# shellcheck source=scripts/vcam-common.sh
source "${BASH_SOURCE[0]%/*}/vcam-common.sh"
if vcam_parse "$@"; then :; else
    code=$?
    if [[ "$code" != 10 ]]; then exit "$code"; fi
    printf 'Usage: scripts/vcam-uninstall.sh [--dry-run]\n'
    exit 0
fi
[[ "$VCAM_MODE" == install ]] || { printf 'Uninstall only accepts --dry-run.\n' >&2; exit 2; }
vcam_tools python3
vcam_run python3 "$VCAM_HELPER" destination "$VCAM_APP"
if (( ! VCAM_DRY_RUN )) && [[ ! -d "$VCAM_APP" ]]; then
    printf 'Isocast Camera Host is absent. No app was removed. If its extension remains, reinstall this host before requesting deactivation.\n'
    vcam_run /usr/bin/systemextensionsctl list
    exit 1
fi
vcam_verify "$VCAM_APP"
if vcam_confirmed "Deactivate only $VCAM_EXTENSION_ID? Close camera clients first." "$VCAM_APP/Contents/MacOS/isocastd" --uninstall-camera-extension; then
    :
else
    code=$?
    if [[ "$code" == 2 ]]; then
        printf 'Restart macOS, then rerun this command. The host app is kept until deactivation finishes.\n'
    fi
    exit "$code"
fi
vcam_run /usr/bin/systemextensionsctl list
vcam_run python3 "$VCAM_HELPER" destination "$VCAM_APP"
vcam_confirmed "Remove only $VCAM_APP? Recordings, setups, Studio NDI and provisioning profiles are kept." \
    /usr/bin/sudo /bin/rm -rf "${VCAM_APP:?}"
if (( VCAM_DRY_RUN )); then
    printf '\nUninstall dry-run complete; no commands were executed.\n'
else
    printf '\nIsocast camera host removed after deactivation.\n'
fi
printf 'Apple account bundle ids/app groups and Xcode provisioning profiles are retained. Manage only dev.isocast.isocastd and dev.isocast.isocastd.camera in Xcode/your Developer account if no longer needed; never remove Studio NDI profiles.\n'
