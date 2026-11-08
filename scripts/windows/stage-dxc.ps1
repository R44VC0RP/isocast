# Stage only the two Windows x64 redistributables, never an SDK/PATH copy.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$Destination, [Parameter(Mandatory)][string]$ToolsDir)
$ErrorActionPreference = 'Stop'
$archiveName = 'dxc_2025_02_20.zip'
$hash = (Get-Content -LiteralPath "$PSScriptRoot/$archiveName.sha256" -Raw).Trim()
$ToolsDir = (New-Item -ItemType Directory -Force $ToolsDir).FullName
$Destination = (New-Item -ItemType Directory -Force $Destination).FullName
$archive = Join-Path $ToolsDir $archiveName
if (-not (Test-Path -LiteralPath $archive)) {
    $ProgressPreference = 'SilentlyContinue'
    $url = "https://github.com/microsoft/DirectXShaderCompiler/releases/download/v1.8.2502/$archiveName"
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        try {
            Write-Host "Downloading $url (attempt $attempt of 3)"
            Invoke-WebRequest -UseBasicParsing $url -Headers @{Accept='application/octet-stream'} -OutFile "$archive.part" -TimeoutSec 60
            break
        } catch {
            Remove-Item -LiteralPath "$archive.part" -Force -ErrorAction SilentlyContinue
            if ($attempt -eq 3) { throw }
            # The release page can keep returning a cached 504. The asset API
            # issues its own redirect for the same immutable release asset.
            $url = 'https://api.github.com/repos/microsoft/DirectXShaderCompiler/releases/assets/231385721'
            $delay = 2 * $attempt
            Write-Warning "DXC download failed; retrying in $delay seconds: $_"
            Start-Sleep -Seconds $delay
        }
    }
    if ((Get-FileHash -LiteralPath "$archive.part").Hash -ne $hash) { throw 'DXC download SHA-256 mismatch' }
    Move-Item -LiteralPath "$archive.part" $archive -Force
}
if ((Get-FileHash -LiteralPath $archive).Hash -ne $hash) { throw "DXC archive SHA-256 mismatch: $archive" }
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::OpenRead($archive)
try {
    foreach ($name in 'dxcompiler.dll','dxil.dll') {
        [IO.Compression.ZipFileExtensions]::ExtractToFile($zip.GetEntry("bin/x64/$name"), (Join-Path $Destination $name), $true)
    }
} finally { $zip.Dispose() }
# Also travels with raw CI/service bundles, not just with the installer.
Copy-Item -LiteralPath "$PSScriptRoot/../../packaging/windows/DXC-LICENSE-LLVM.txt" $Destination -Force
