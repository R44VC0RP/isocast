# Runs under SSH. The task's separate supervisor survives transport loss.
$ErrorActionPreference = 'Stop'
$p = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($payload)) | ConvertFrom-Json
$r = $p.request
$folder = $r.folder
$registered = $false
$watchdog = "$($r.task)-Cleanup"
$result = $null
try {
    if ([DateTimeOffset]::UtcNow.ToUnixTimeSeconds() -gt $p.expires) { throw 'Launch expired before reaching Windows' }
    # No password, S4U token or session-0 desktop fallback.
    $shells = @(Get-CimInstance Win32_Process -Filter "Name='explorer.exe'" | Where-Object {
        $owner = Invoke-CimMethod -InputObject $_ -MethodName GetOwner
        $owner.User -eq $r.user -and $_.SessionId -gt 0
    })
    $sessions = @($shells | Select-Object -ExpandProperty SessionId -Unique)
    if ($sessions.Count -ne 1) { throw "Expected one logged-on desktop for $($r.user); found $($sessions.Count). Logon/unlock is required." }
    $r | Add-Member -NotePropertyName session -NotePropertyValue $sessions[0]
    # Independent of SSH AND of the user's logon. It also covers losing the
    # connection between staging files and starting the interactive supervisor.
    # Register with a trigger (not register-then-start) so there is no gap in
    # which an unstarted watchdog could be left behind.
    $cleanup = @"
`$ErrorActionPreference='Stop'
try {
    `$t=Get-ScheduledTask -TaskName '$($r.task)' -ErrorAction SilentlyContinue
    if (`$t) { Stop-ScheduledTask -InputObject `$t; Unregister-ScheduledTask -InputObject `$t -Confirm:`$false }
    for (`$i=0; `$i -lt 10 -and (Test-Path -LiteralPath '$folder'); `$i++) {
        try { Remove-Item -LiteralPath '$folder' -Recurse -Force } catch { Start-Sleep -Seconds 1 }
    }
} finally { Unregister-ScheduledTask -TaskName '$watchdog' -Confirm:`$false }
"@
    $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($cleanup))
    $cleanupAction = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe" -Argument "-NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand $encoded"
    $cleanupPrincipal = New-ScheduledTaskPrincipal -UserId SYSTEM -LogonType ServiceAccount
    $cleanupSettings = New-ScheduledTaskSettingsSet -StartWhenAvailable -ExecutionTimeLimit (New-TimeSpan -Seconds 60) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName $watchdog -Action $cleanupAction -Principal $cleanupPrincipal -Settings $cleanupSettings -Trigger (New-ScheduledTaskTrigger -Once -At (Get-Date).AddSeconds($r.budget + 90)) | Out-Null
    New-Item -ItemType Directory -Path $folder -ErrorAction Stop | Out-Null
    $sid = ([Security.Principal.NTAccount]::new("$env:COMPUTERNAME\$($r.user)")).Translate([Security.Principal.SecurityIdentifier]).Value
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetSecurityDescriptorSddlForm("O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;$sid)")
    Set-Acl -LiteralPath $folder -AclObject $acl
    foreach ($file in $p.files.PSObject.Properties) {
        [IO.File]::WriteAllBytes("$folder/$($file.Name)", [Convert]::FromBase64String($file.Value))
    }
    $r | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath "$folder/request.json" -Encoding UTF8
    $action = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\wscript.exe" -Argument "//B //NoLogo `"$folder/gui-task.vbs`""
    $principal = New-ScheduledTaskPrincipal -UserId "$env:COMPUTERNAME\$($r.user)" -LogonType Interactive -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Seconds ($r.budget + 100)) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
    Register-ScheduledTask -TaskName $r.task -Action $action -Principal $principal -Settings $settings | Out-Null
    $registered = $true
    # packager is non-admin: let the limited supervisor unregister its OWN task.
    $scheduler = New-Object -ComObject Schedule.Service
    $scheduler.Connect()
    $scheduler.GetFolder('\').GetTask($r.task).SetSecurityDescriptor("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;$sid)", 0)
    Start-ScheduledTask -TaskName $r.task
    $deadline = (Get-Date).AddSeconds($r.budget + 15)
    while (-not (Test-Path -LiteralPath "$folder/ready") -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    if (-not (Test-Path -LiteralPath "$folder/ready")) { throw 'Interactive task did not finish: desktop unavailable or task startup timed out' }
    $result = Get-Content -Raw -LiteralPath "$folder/result.json" | ConvertFrom-Json
    if ($result.ok -and (Test-Path -LiteralPath "$folder/screenshot.png")) {
        $result | Add-Member -NotePropertyName png -NotePropertyValue ([Convert]::ToBase64String([IO.File]::ReadAllBytes("$folder/screenshot.png")))
    }
} catch {
    $result = @{ok=$false;error=$_.Exception.Message}
} finally {
    # The supervisor can remove its folder/task concurrently. Check the final
    # state, not a racing Remove/Unregister exception, and publish only AFTER
    # cleanup. A genuine residue remains a failed receipt, not silent success.
    try { if (Test-Path -LiteralPath $folder) { [IO.File]::WriteAllText("$folder/ack", '') } } catch { }
    # Give the task its own finally first; Stop is the bounded fallback.
    if ($registered) {
        $end = (Get-Date).AddSeconds(5)
        while ((Test-Path -LiteralPath $folder) -and (Get-Date) -lt $end) { Start-Sleep -Milliseconds 200 }
        $task = Get-ScheduledTask -TaskName $r.task -ErrorAction SilentlyContinue
        if ($task) {
            try { Stop-ScheduledTask -InputObject $task } catch { }
            try { Unregister-ScheduledTask -InputObject $task -Confirm:$false } catch { }
        }
    }
    for ($i=0; $i -lt 10 -and (Test-Path -LiteralPath $folder); $i++) {
        try { Remove-Item -LiteralPath $folder -Recurse -Force } catch { Start-Sleep -Milliseconds 200 }
    }
    if ((Test-Path -LiteralPath $folder) -or (Get-ScheduledTask -TaskName $r.task -ErrorAction SilentlyContinue)) {
        $result = @{ok=$false;error='GUI cleanup incomplete; independent SYSTEM watchdog remains armed';task=$r.task}
    } else {
        $guard = Get-ScheduledTask -TaskName $watchdog -ErrorAction SilentlyContinue
        if ($guard) { try { Unregister-ScheduledTask -InputObject $guard -Confirm:$false } catch { } }
        if (Get-ScheduledTask -TaskName $watchdog -ErrorAction SilentlyContinue) {
            $result = @{ok=$false;error='Cleanup watchdog could not be removed';task=$watchdog}
        }
    }
}
$result | ConvertTo-Json -Depth 50 -Compress
