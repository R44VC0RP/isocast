#!/bin/bash
# One command, explicit account/copy/activation approvals; --unsigned stays in scratch.
set -euo pipefail
# shellcheck source=scripts/vcam-common.sh
source "${BASH_SOURCE[0]%/*}/vcam-common.sh"
if vcam_parse "$@"; then :; else
    code=$?
    if [[ "$code" != 10 ]]; then exit "$code"; fi
    printf 'Usage: scripts/vcam-install.sh [--dry-run] [--unsigned | --check] [--build-dir /absolute/path]\n'
    printf '  --unsigned: scratch build and ad-hoc verification; no provisioning, /Applications copy or activation.\n'
    printf '  --check: check the already-installed camera with an isolated synthetic engine.\n'
    exit 0
fi
vcam_tools python3
printf 'Isocast Virtual Camera: %s + %s, team %s\n' "$VCAM_HOST_ID" "$VCAM_EXTENSION_ID" "$VCAM_TEAM"
printf 'Build folder: %s\nSystem destination: %s\n' "$VCAM_BUILD" "$VCAM_APP"

if [[ "$VCAM_MODE" != check ]]; then
    vcam_tools cargo xcodegen xcodebuild codesign
    vcam_run python3 "$VCAM_HELPER" prepare "$VCAM_REPO" "$VCAM_STAGE"
    vcam_run env "CARGO_TARGET_DIR=$VCAM_TARGET" "CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}" \
        nice -n 10 cargo build --manifest-path "$VCAM_REPO/Cargo.toml" --release -p isocast-daemon -p isocast-cli
    vcam_run /bin/mkdir -p "$VCAM_BUILD/project"
    vcam_run xcodegen generate --no-env --spec "$VCAM_REPO/camera-extension/project.yml" --project "$VCAM_BUILD/project"
    vcam_run python3 "$VCAM_HELPER" project-check "$VCAM_PROJECT" "$VCAM_REPO" "$VCAM_STAGE" "$VCAM_TARGET/release/isocastd"
    if (( VCAM_DRY_RUN )); then
        printf '  # project-check inspects resolved target ids/team/capabilities before account access:\n'
        vcam_command /usr/bin/xcodebuild -project "$VCAM_PROJECT" -alltargets -configuration Release \
            -showBuildSettings -json CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO \
            "ISOCAST_REPO_ROOT=$VCAM_REPO" "ISOCAST_VCAM_STAGE=$VCAM_STAGE" "ISOCAST_RUST_BINARY=$VCAM_TARGET/release/isocastd"
    fi
    if [[ "$VCAM_MODE" == unsigned ]]; then
        vcam_run "${VCAM_XCODE[@]}" CODE_SIGNING_ALLOWED=NO CODE_SIGNING_REQUIRED=NO CODE_SIGN_IDENTITY= build
        vcam_run /usr/bin/codesign --force --sign - --options runtime --entitlements "$VCAM_STAGE/Camera.entitlements" \
            "$VCAM_PRODUCT/Contents/Library/SystemExtensions/$VCAM_EXTENSION_ID.systemextension"
        vcam_run /usr/bin/codesign --force --sign - --options runtime --entitlements "$VCAM_STAGE/Host.entitlements" "$VCAM_PRODUCT"
        vcam_verify "$VCAM_PRODUCT" --unsigned
        if (( VCAM_DRY_RUN )); then
            printf '\nUnsigned-build dry-run complete; no commands were executed.\n'
        else
            printf '\nScratch build finished: %s\nNot provisioned or eligible for installation. No Apple-account, /Applications-copy or activation step was run.\n' "$VCAM_PRODUCT"
        fi
        exit 0
    fi
    vcam_run python3 "$VCAM_HELPER" destination "$VCAM_APP"
    vcam_confirmed "Allow Xcode to register only the two Isocast ids, create/download their profiles and sign with team $VCAM_TEAM? This contacts your Apple account and may request Keychain access." \
        "${VCAM_XCODE[@]}" -allowProvisioningUpdates build
    vcam_verify "$VCAM_PRODUCT"
    vcam_run python3 "$VCAM_HELPER" destination "$VCAM_APP"
    vcam_confirmed "Copy/replace only $VCAM_APP with this verified Isocast host? Studio NDI and other apps are refused by the identity check." \
        /usr/bin/sudo python3 "$VCAM_HELPER" replace-host "$VCAM_PRODUCT" "$VCAM_APP"
    vcam_verify "$VCAM_APP"
    printf '\nWhen macOS asks: System Settings > General > Login Items & Extensions > Camera Extensions > enable Isocast Virtual Camera.\n'
    if vcam_confirmed 'Submit the Isocast camera activation request now?' "$VCAM_APP/Contents/MacOS/isocastd" --install-camera-extension; then
        :
    else
        code=$?
        if [[ "$code" == 2 ]]; then
            printf 'Approval or restart is pending. Complete the printed macOS step, then run scripts/vcam-install.sh --check.\n'
        fi
        exit "$code"
    fi
else
    vcam_verify "$VCAM_APP"
fi

vcam_run /usr/bin/systemextensionsctl list
vcam_run "$VCAM_APP/Contents/MacOS/isocastd" --list-virtual-camera

# A private throwaway setup/socket: never restart or alter Ryan's running engine.
if (( VCAM_DRY_RUN )); then
    check_dir=$VCAM_BUILD/check.DRYRUN
    vcam_command /usr/bin/mktemp -d "$VCAM_BUILD/check.XXXXXX"
else
    mkdir -p "$VCAM_BUILD"
    check_dir=$(mktemp -d "$VCAM_BUILD/check.XXXXXX")
fi
vcam_run python3 "$VCAM_HELPER" check-setup "$check_dir"
check_env=(env -u ISOCAST_VCAM_PROBE -u ISOCAST_VCAM_CHILD_TEST
    "ISOCAST_SOCKET=$check_dir/daemon.sock" "ISOCAST_SETUP=$check_dir/live.toml"
    "ISOCAST_DEV_APP=$VCAM_APP" "ISOCAST_PLUGINS=$check_dir/plugins" ISOCAST_MONITOR_SILENT=1)
daemon_pid=
cleanup_check() {
    local result=$?
    trap - EXIT INT TERM
    if (( VCAM_DRY_RUN )); then
        vcam_command python3 "$VCAM_HELPER" shutdown "$check_dir/daemon.sock"
        printf '  # Wait for the owned test daemon; on failure terminate only that child.\n'
    elif [[ -n "$daemon_pid" ]]; then
        python3 "$VCAM_HELPER" shutdown "$check_dir/daemon.sock" >/dev/null 2>&1 || true
        kill -TERM "$daemon_pid" 2>/dev/null || true
        wait "$daemon_pid" 2>/dev/null || true
    fi
    return "$result"
}
trap cleanup_check EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
vcam_command "${check_env[@]}" "$VCAM_APP/Contents/MacOS/isocastd"
if (( VCAM_DRY_RUN )); then
    printf '  # Background child; stdout/stderr go to CHECK_DIR/daemon.log.\n'
else
    "${check_env[@]}" "$VCAM_APP/Contents/MacOS/isocastd" >"$check_dir/daemon.log" 2>&1 &
    daemon_pid=$!
fi
vcam_run python3 "$VCAM_HELPER" wait "$check_dir/daemon.sock"
vcam_run "${check_env[@]}" "$VCAM_TARGET/release/isocast" apply --revision 0 "$check_dir/input.toml" --json
vcam_run "${check_env[@]}" "$VCAM_TARGET/release/isocast" vcam start --json
vcam_run "$VCAM_APP/Contents/MacOS/isocastd" --check-virtual-camera
vcam_run "${check_env[@]}" "$VCAM_TARGET/release/isocast" vcam status --json
vcam_run "${check_env[@]}" "$VCAM_TARGET/release/isocast" vcam stop --json
if (( VCAM_DRY_RUN )); then
    printf '\nInstallation/check dry-run complete; no commands were executed.\n'
else
    printf '\nCamera check passed: AVFoundation received changing 1920x1080 frames.\n'
    printf 'Select Isocast Virtual Camera in your camera app. The test engine stops here; normal use needs a G.1-capable engine/CLI and isocast vcam start.\n'
fi
