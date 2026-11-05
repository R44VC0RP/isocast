# Stage only the two Windows x64 redistributables, never an SDK/PATH copy.
[CmdletBinding()]
param([Parameter(Mandatory)][string]$Destination, [Parameter(Mandatory)][string]$ToolsDir)
$ErrorActionPreference = 'Stop'
$archiveName = 'dxc_2025_02_20.zip'
$hash = '70b1913a1bfce4a3e1a5311d16246f4ecdf3a3e613abec8aa529e57668426f85'
$ToolsDir = (New-Item -ItemType Directory -Force $ToolsDir).FullName
$Destination = (New-Item -ItemType Directory -Force $Destination).FullName
$archive = Join-Path $ToolsDir $archiveName
if (-not (Test-Path -LiteralPath $archive)) {
    $ProgressPreference = 'SilentlyContinue'
    Invoke-WebRequest -UseBasicParsing "https://github.com/microsoft/DirectXShaderCompiler/releases/download/v1.8.2502/$archiveName" -OutFile "$archive.part"
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
