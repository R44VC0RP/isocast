# Hosted-only test launcher: the Actions job denies CREATE_BREAKAWAY_FROM_JOB.
# Run the two real-CLI scenarios outside that job, without changing production
# process ownership or detachment policy. Never use this on a developer machine.
# Hosted runners are administrators with UAC off: Limited cannot produce a
# non-elevated token. ASK-34 permits the same SID with no additional groups or
# privileges over the Actions parent; group attributes/privilege states count too.
param([switch]$CleanupOnly)

$ErrorActionPreference = 'Stop'
if ($env:RUNNER_ENVIRONMENT -ne 'github-hosted' -or $env:OS -ne 'Windows_NT') {
    throw 'The isolated CI task runner requires RUNNER_ENVIRONMENT=github-hosted on Windows'
}
if ($env:GITHUB_RUN_ID -notmatch '^\d+$' -or $env:GITHUB_RUN_ATTEMPT -notmatch '^\d+$' -or
    $env:GITHUB_JOB -notmatch '^[a-zA-Z0-9_-]+$') {
    throw 'Missing or invalid GitHub run identity'
}
$taskName = "Isocast-isolated-$env:GITHUB_RUN_ID-$env:GITHUB_RUN_ATTEMPT-$env:GITHUB_JOB"
function Remove-TestTask {
    $task = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
    if ($task) {
        try {
            Stop-ScheduledTask -InputObject $task
        } finally {
            Unregister-ScheduledTask -InputObject $task -Confirm:$false
        }
    }
    if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
        throw "Test task was not deleted: $taskName"
    }
    Write-Host "Test task absent: $taskName"
}
if ($CleanupOnly) {
    Remove-TestTask
    exit 0
}

$root = Join-Path $env:RUNNER_TEMP 'windows-isolated-task'
New-Item -ItemType Directory -Path $root | Out-Null
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$config = @{
    sid = $identity.User.Value
    python = (Get-Command python -CommandType Application | Select-Object -First 1).Source
    repo = $repo
    path = $env:PATH
    root = $root
    temp = $env:RUNNER_TEMP
}
function Get-TokenSnapshot {
    $groups = @(& "$env:SystemRoot\System32\whoami.exe" /groups /fo csv /nh)
    if ($LASTEXITCODE -ne 0) { throw 'Cannot inspect token groups' }
    $privileges = @(& "$env:SystemRoot\System32\whoami.exe" /priv /fo csv /nh)
    if ($LASTEXITCODE -ne 0) { throw 'Cannot inspect token privileges' }
    $groups = @($groups | ConvertFrom-Csv -Header Name,Type,Sid,Attributes |
        ForEach-Object { "$($_.Sid)|$($_.Attributes)" })
    $privileges = @($privileges | ConvertFrom-Csv -Header Name,Description,State |
        ForEach-Object { "$($_.Name)|$($_.State)" })
    if (-not $groups.Count -or -not $privileges.Count) { throw 'Empty token snapshot' }
    return @{
        sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
        groups = $groups
        privileges = $privileges
    }
}
$config.token = Get-TokenSnapshot
$config | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $root 'config.json') -Encoding utf8
# Only this allowlisted environment is passed; no Actions tokens or secrets.
# The task requests Limited and verifies no token gain before any test.
$worker = @'
$ErrorActionPreference = 'Stop'
$code = 1
$token = $null
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
try {
    $config = Get-Content (Join-Path $PSScriptRoot 'config.json') -Raw | ConvertFrom-Json
    $token = Get-TokenSnapshot
    $extraGroups = @($token.groups | Where-Object { $_ -notin $config.token.groups })
    $extraPrivileges = @($token.privileges | Where-Object { $_ -notin $config.token.privileges })
    if ($token.sid -ne $config.sid -or $extraGroups.Count -or $extraPrivileges.Count) {
        throw "Scheduled token differs: SID $($token.sid) vs $($config.sid); added groups [$($extraGroups -join '; ')]; added privileges [$($extraPrivileges -join '; ')]"
    }
    $env:PATH = $config.path
    $env:TEMP = $config.temp
    $env:TMP = $config.temp
    $env:RUNNER_ENVIRONMENT = 'github-hosted'
    Set-Location $config.repo
    Write-Host "Verified same-user token without privilege gain: $($identity.User.Value)"
    & $config.python scripts/e2e.py --daemon target/debug/isocastd.exe --cli target/debug/isocast.exe `
        --only win_isolated --only win_machine_profile --codec h264 --timeout 420 `
        --work (Join-Path $config.root 'work') --keep --report (Join-Path $config.root 'result.json') `
        1> (Join-Path $config.root 'stdout.log') 2> (Join-Path $config.root 'stderr.log')
    $code = $LASTEXITCODE
} catch {
    $_ | Out-String | Set-Content (Join-Path $PSScriptRoot 'launcher-error.log')
} finally {
    @{ exit_code = $code; token = $token } | ConvertTo-Json -Depth 4 |
        Set-Content (Join-Path $PSScriptRoot 'exit.pending') -Encoding utf8
    Move-Item (Join-Path $PSScriptRoot 'exit.pending') (Join-Path $PSScriptRoot 'exit.json')
}
exit $code
'@
("function Get-TokenSnapshot {`n" + ${function:Get-TokenSnapshot}.ToString() + "`n}`n" + $worker) |
    Set-Content (Join-Path $root 'worker.ps1') -Encoding utf8

try {
    if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
        throw "Test task already exists: $taskName"
    }
    $action = New-ScheduledTaskAction -Execute (Join-Path $PSHOME 'pwsh.exe') `
        -Argument ('-NoProfile -NonInteractive -File "' + (Join-Path $root 'worker.ps1') + '"') `
        -WorkingDirectory $repo
    $principal = New-ScheduledTaskPrincipal -UserId $identity.User.Value -LogonType Interactive -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 15) `
        -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName $taskName -Action $action -Principal $principal -Settings $settings | Out-Null
    Start-ScheduledTask -TaskName $taskName
    $started = [DateTime]::UtcNow
    $deadline = [DateTime]::UtcNow.AddMinutes(15)
    $exitFile = Join-Path $root 'exit.json'
    while (-not (Test-Path $exitFile)) {
        if ([DateTime]::UtcNow -ge $deadline) {
            throw 'Isolated CI task exceeded its 15-minute hard timeout'
        }
        $task = Get-ScheduledTask -TaskName $taskName
        if ([DateTime]::UtcNow -gt $started.AddSeconds(10) -and $task.State -notin @('Running', 'Queued')) {
            if (Test-Path $exitFile) { break }
            $info = Get-ScheduledTaskInfo -TaskName $taskName
            throw "Isolated CI task stopped without a report (scheduler result $($info.LastTaskResult))"
        }
        Start-Sleep -Seconds 2
    }
    $result = Get-Content $exitFile -Raw | ConvertFrom-Json
    if ($result.exit_code -ne 0 -or $result.token.sid -ne $config.sid -or
        -not $result.token.groups.Count -or -not $result.token.privileges.Count -or
        @($result.token.groups | Where-Object { $_ -notin $config.token.groups }).Count -or
        @($result.token.privileges | Where-Object { $_ -notin $config.token.privileges }).Count) {
        throw "Isolated CI task failed (exit $($result.exit_code)); see $root"
    }
    $report = Get-Content (Join-Path $root 'result.json') -Raw | ConvertFrom-Json
    if ($report.scenarios.Count -ne 2 -or @($report.scenarios | Where-Object status -ne 'PASS').Count -ne 0 -or
        (($report.scenarios.name | Sort-Object) -join ',') -ne 'win_isolated,win_machine_profile') {
        throw 'Both isolated scenarios must run and PASS; SKIP/PENDING is not qualification'
    }
    Write-Host 'Hosted isolated workspace/profile scenarios passed'
} catch {
    Write-Host "::error::$_"
    throw
} finally {
    Remove-TestTask
}
