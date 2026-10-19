param([Parameter(Mandatory)][string]$Folder)
# Independent watchdog: a blocked UIA provider cannot block deadline/cleanup.
$ErrorActionPreference = 'Stop'
$r = Get-Content -Raw -LiteralPath "$Folder/request.json" | ConvertFrom-Json
$worker = $null
try {
    $env:TEMP = $Folder
    $env:TMP = $Folder
    $start = [Diagnostics.ProcessStartInfo]::new()
    $start.FileName = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
    $start.Arguments = "-NoProfile -NonInteractive -STA -ExecutionPolicy Bypass -File `"$Folder/gui-worker.ps1`" -Folder `"$Folder`""
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    $worker = [Diagnostics.Process]::Start($start)
    $stdout = $worker.StandardOutput.ReadToEndAsync()
    $stderr = $worker.StandardError.ReadToEndAsync()
    if (-not $worker.WaitForExit($r.budget * 1000)) {
        $worker.Kill()
        $worker.WaitForExit()
        throw "GUI operation exceeded $($r.budget)s (possibly an unresponsive UI Automation provider)"
    }
    if (-not (Test-Path -LiteralPath "$Folder/result.json")) { throw 'GUI worker exited without a result' }
    [IO.File]::WriteAllText("$Folder/stdout.txt",$stdout.Result)
    [IO.File]::WriteAllText("$Folder/stderr.txt",$stderr.Result)
} catch {
    @{ok=$false;error=$_.Exception.Message} | ConvertTo-Json -Compress | Set-Content -LiteralPath "$Folder/result.json" -Encoding UTF8
} finally {
    if ($worker -and -not $worker.HasExited) { $worker.Kill(); $worker.WaitForExit() }
    [IO.File]::WriteAllText("$Folder/ready", '')
    $deadline = (Get-Date).AddSeconds(60)
    while (-not (Test-Path -LiteralPath "$Folder/ack") -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    # Removing our currently running task is supported; do not Stop ourselves.
    Unregister-ScheduledTask -TaskName $r.task -Confirm:$false -ErrorAction Continue
    Remove-Item -LiteralPath $Folder -Recurse -Force
}
