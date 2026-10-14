#!/bin/bash
# Shared installer invariants. Sourcing this file executes no external commands.
# These module variables are consumed by the two entry-point scripts.
# shellcheck disable=SC2034
set -euo pipefail
VCAM_TEAM=9G68SMNHEU
VCAM_HOST_ID=dev.isocast.isocastd
VCAM_EXTENSION_ID=dev.isocast.isocastd.camera
VCAM_APP='/Applications/Isocast Camera Host.app'
VCAM_DRY_RUN=0
VCAM_MODE=install
VCAM_REPO=$(cd -- "${BASH_SOURCE[0]%/*}/.." && pwd -P)
VCAM_TARGET=${CARGO_TARGET_DIR:-/Volumes/IsocastBuild/g1-vcam/target}
VCAM_BUILD=${ISOCAST_VCAM_BUILD:-/Volumes/IsocastBuild/g1-vcam/scratch/vcam-installer}

vcam_command() { printf '  '; printf '%q ' "$@"; printf '\n'; }

vcam_run() {
    vcam_command "$@"
    if (( ! VCAM_DRY_RUN )); then "$@"; fi
}

vcam_confirmed() {
    local description=$1 answer
    shift
    printf '\n%s\n' "$description"
    vcam_command "$@"
    if (( VCAM_DRY_RUN )); then
        printf '  # Confirmation required here; dry-run executes nothing.\n'
        return
    fi
    if [[ ! -t 0 ]]; then
        printf 'Run from an interactive terminal; no system step is auto-approved.\n' >&2
        return 3
    fi
    read -r -p 'Continue? [y/N] ' answer || return 3
    case "$answer" in y|Y|yes|YES) "$@" ;; *) printf 'Cancelled before this step.\n'; return 3 ;; esac
}

vcam_parse() {
    while (( $# )); do
        case "$1" in
            --dry-run) VCAM_DRY_RUN=1 ;;
            --unsigned|--check)
                [[ "$VCAM_MODE" == install ]] || { printf 'Choose only one of --unsigned or --check.\n' >&2; return 2; }
                VCAM_MODE=${1#--} ;;
            --build-dir)
                [[ $# -ge 2 ]] || { printf 'Missing --build-dir path\n' >&2; return 2; }
                VCAM_BUILD=$2; shift ;;
            --help|-h) return 10 ;;
            *) printf 'Unknown option: %s\n' "$1" >&2; return 2 ;;
        esac
        shift
    done
    [[ "$VCAM_BUILD" = /* && "$VCAM_TARGET" = /* ]] || {
        printf 'CARGO_TARGET_DIR and --build-dir must be absolute paths.\n' >&2; return 2;
    }
    VCAM_STAGE=$VCAM_BUILD/stage
    VCAM_PROJECT=$VCAM_BUILD/project/IsocastCamera.xcodeproj
    VCAM_PRODUCT=$VCAM_BUILD/derived/Build/Products/Release/Isocast\ Camera\ Host.app
    VCAM_HELPER=$VCAM_REPO/scripts/vcam-package.py
    VCAM_XCODE=(/usr/bin/xcodebuild -project "$VCAM_PROJECT" -scheme IsocastCameraHost
        -configuration Release -jobs 4 -derivedDataPath "$VCAM_BUILD/derived"
        "ISOCAST_REPO_ROOT=$VCAM_REPO" "ISOCAST_VCAM_STAGE=$VCAM_STAGE"
        "ISOCAST_RUST_BINARY=$VCAM_TARGET/release/isocastd")
}

vcam_tools() {
    if (( VCAM_DRY_RUN )); then return; fi
    local tool
    for tool in "$@"; do
        command -v "$tool" >/dev/null || { printf 'Required tool not found: %s\n' "$tool" >&2; return 1; }
    done
}

vcam_verify() {
    vcam_run /usr/bin/codesign --verify --deep --strict "$1"
    vcam_run python3 "$VCAM_HELPER" verify "$@"
    if (( VCAM_DRY_RUN )); then
        printf '  # verify also runs the following read-only checks in memory (no profile dumps):\n'
        local bundle
        for bundle in "$1" "$1/Contents/Library/SystemExtensions/$VCAM_EXTENSION_ID.systemextension"; do
            vcam_command /usr/bin/codesign -d --entitlements :- "$bundle"
            if [[ ${2:-} != --unsigned ]]; then
                vcam_command /usr/bin/codesign -dv --verbose=4 "$bundle"
                vcam_command /usr/bin/security cms -D -i "$bundle/Contents/embedded.provisionprofile"
            fi
        done
        printf '  # Developer ID only: spctl --assess --type execute --verbose=2 APP; not used for local Apple Development.\n'
    fi
}
