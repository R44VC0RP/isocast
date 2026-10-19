#!/usr/bin/env bash
# Packaging-only Hyper-V guest. Never changes host settings or other lanes.
set -euo pipefail

host=${EXON_DESK_HOST:-exon-desk-lan}
user=${ISOCAST_WINDOWS_VM_USER:-isocast}
scratch=${ISOCAST_WINDOWS_VM_SCRATCH:-/Volumes/IsocastBuild/windows-pkg/scratch}
case "$host" in exon-desk-lan|exon-desk) ;; *) echo 'use exon-desk-lan or exon-desk' >&2; exit 2 ;; esac
case "$user" in isocast|packager) ;; *) echo 'guest user must be isocast or packager' >&2; exit 2 ;; esac
export EXON_DESK_HOST="$host"
name=isocast-pkg

usage() {
  echo 'usage: windows-vm.sh up | ensure-desktop [--restart] | ssh [COMMAND…] | ps [SCRIPT] | revert-clean --yes | down' >&2
  exit 2
}

host_ps() { exon-desk-ps; }

address() {
  host_ps <<'PS'
$ErrorActionPreference = 'Stop'
$ip = Get-VMNetworkAdapter -VMName isocast-pkg | ForEach-Object IPAddresses | Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' -and $_ -notlike '169.254.*' } | Select-Object -First 1
if (-not $ip) { throw 'isocast-pkg has no IPv4 address yet; start it and wait for guest networking' }
Write-Output $ip
PS
}

connect() {
  local ip identity
  ip=$(address | tr -d '\r')
  identity=$(ssh -G "$host" 2>/dev/null | awk '$1 == "identityfile" { print $2; exit }')
  identity=${identity/#\~/$HOME}
  [[ -f "$identity" ]] || { echo "personal SSH identity not found: $identity" >&2; exit 1; }
  mkdir -p "$scratch"
  # Trust the guest key through the already authenticated host, not ssh-keyscan.
  # The bootstrap credential is DPAPI-protected on the host and never printed.
  if [[ ! -f "$scratch/known_hosts" ]]; then
    local key
    key=$(host_ps <<'PS'
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Security
$bytes = [Security.Cryptography.ProtectedData]::Unprotect([IO.File]::ReadAllBytes('C:\isocast-dev\vm\guest-credential.bin'), $null, [Security.Cryptography.DataProtectionScope]::LocalMachine)
$credential = [pscredential]::new('isocast', (ConvertTo-SecureString ([Text.Encoding]::UTF8.GetString($bytes)) -AsPlainText -Force))
Invoke-Command -VMName isocast-pkg -Credential $credential -ScriptBlock {
    Get-Content 'C:\ProgramData\ssh\ssh_host_ed25519_key.pub'
}
PS
    )
    key=$(printf '%s' "$key" | tr -d '\r')
    [[ "$key" == ssh-ed25519\ * && "$key" != *$'\n'* ]] || { echo 'invalid guest host key' >&2; exit 1; }
    (umask 077; printf '%s %s\n' "$name" "$key" > "$scratch/known_hosts")
  fi
  ssh -i "$identity" -o IdentitiesOnly=yes -o HostKeyAlias="$name" \
    -o UserKnownHostsFile="$scratch/known_hosts" -o StrictHostKeyChecking=yes \
    -o ConnectTimeout=10 -o PasswordAuthentication=no -o KbdInteractiveAuthentication=no \
    -J "$host" "$user@$ip" "$@"
}

[[ $# -gt 0 ]] || usage
command=$1
shift
case "$command" in
  up)
    [[ $# -eq 0 ]] || usage
    host_ps <<'PS'
$ErrorActionPreference = 'Stop'
if ((Get-VM -Name isocast-pkg).State -ne 'Running') { Start-VM -Name isocast-pkg }
$deadline = (Get-Date).AddMinutes(3)
$ready = $false
while ((Get-Date) -lt $deadline -and -not $ready) {
    $ip = Get-VMNetworkAdapter -VMName isocast-pkg | ForEach-Object IPAddresses | Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' -and $_ -notlike '169.254.*' } | Select-Object -First 1
    if ($ip) {
        $client = [Net.Sockets.TcpClient]::new()
        try { $ready = $client.ConnectAsync($ip, 22).Wait(2000) -and $client.Connected } catch { }
        finally { $client.Dispose() }
    }
    if (-not $ready) { Start-Sleep -Seconds 2 }
}
if (-not $ready) { throw 'Guest SSH did not become reachable in three minutes; the VM was left running' }
Get-VM -Name isocast-pkg | Select-Object Name,State
PS
    ;;
  ensure-desktop)
    restart=0
    if [[ $# -eq 1 && $1 == --restart ]]; then restart=1; elif [[ $# -ne 0 ]]; then usage; fi
    # Restarts need explicit consent: other lanes may have work in this guest.
    # Configuration is idempotent; never rotate an already configured password.
    tmo=$(command -v timeout || command -v gtimeout)
    "$tmo" --foreground -k 10 240 "$0" up >&2
    script_dir=$(cd "$(dirname "$0")" && pwd)
    result=$(ISOCAST_WINDOWS_VM_USER=isocast "$tmo" --foreground -k 10 120 "$0" ps < "$script_dir/windows/ensure-desktop.ps1")
    printf '%s\n' "$result" >&2
    if printf '%s' "$result" | python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin)["restart_required"] else 1)'; then
      [[ $restart == 1 ]] || { echo 'Desktop configuration needs a guest restart. When this VM is free, run windows-vm.sh ensure-desktop --restart.' >&2; exit 1; }
      "$tmo" --foreground -k 10 120 exon-desk-ps <<'PS'
$ErrorActionPreference='Stop'
Stop-VM -Name isocast-pkg
Start-VM -Name isocast-pkg
PS
      "$tmo" --foreground -k 10 240 "$0" up >&2
    fi
    # The registry settings are not proof of autologon: wait for the real shell.
    ISOCAST_WINDOWS_VM_USER=isocast "$tmo" --foreground -k 10 150 "$0" ps <<'PS'
$deadline=(Get-Date).AddSeconds(120)
do {
    $sessions=@(Get-CimInstance Win32_Process -Filter "Name='explorer.exe'" | Where-Object { (Invoke-CimMethod -InputObject $_ -MethodName GetOwner).User -eq 'packager' } | Select-Object -ExpandProperty SessionId -Unique)
    $locked=@(Get-Process LogonUI -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -in $sessions })
    if ($sessions.Count -eq 1 -and $sessions[0] -gt 0 -and $locked.Count -eq 0) {
        @{ok=$true;user='packager';session=$sessions[0];credential_storage='LSA secret';plaintext_registry_password=$false} | ConvertTo-Json -Compress
        exit 0
    }
    Start-Sleep -Seconds 2
} while ((Get-Date) -lt $deadline)
throw 'packager did not reach an unlocked interactive desktop within 120 seconds'
PS
    ;;
  ssh) connect "$@" ;;
  ps)
    [[ $# -le 1 ]] || usage
    if [[ $# -eq 1 ]]; then script=$1; else script=$(cat); fi
    script=$'$ErrorActionPreference = "Stop"\n$ProgressPreference = "SilentlyContinue"\n'"$script"
    encoded=$(printf '%s' "$script" | iconv -f UTF-8 -t UTF-16LE | base64 | tr -d '\n')
    # The guest's login shell is PowerShell; without the explicit exit it
    # collapses every nonzero native exit status to 1.
    connect "powershell.exe -NoProfile -NonInteractive -EncodedCommand $encoded; exit \$LASTEXITCODE"
    ;;
  revert-clean)
    [[ $# -eq 1 && $1 == --yes ]] || { echo 'Revert discards all guest changes since clean. Shut the guest down, then use revert-clean --yes.' >&2; exit 2; }
    host_ps <<'PS'
$ErrorActionPreference = 'Stop'
if ((Get-VM -Name isocast-pkg).State -ne 'Off') { throw 'Run windows-vm.sh down first; refusing to interrupt a running guest' }
Get-VMSnapshot -VMName isocast-pkg -Name clean | Restore-VMSnapshot -Confirm:$false
Write-Output 'Restored clean. Run windows-vm.sh up.'
PS
    ;;
  down)
    [[ $# -eq 0 ]] || usage
    host_ps <<'PS'
$ErrorActionPreference = 'Stop'
if ((Get-VM -Name isocast-pkg).State -ne 'Off') { Stop-VM -Name isocast-pkg }
Get-VM -Name isocast-pkg | Select-Object Name,State
PS
    ;;
  *) usage ;;
esac
