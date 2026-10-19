# Package an already-built, locked tree. Unsigned unless explicitly given an
# Authenticode certificate already installed by the release operator. No publishing.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$BinDir,
    [Parameter(Mandatory)][string]$OutputDir,
    [Parameter(Mandatory)][string]$ToolsDir,
    [Parameter(Mandatory)][string]$CrtDir,
    [string]$Version,
    [string]$CertificateThumbprint,
    [string]$SignToolPath,
    [string]$TimestampUrl
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repo = (Resolve-Path "$PSScriptRoot\..\..").Path
$sign = [bool]$CertificateThumbprint
if ($sign -ne [bool]$SignToolPath -or $sign -ne [bool]$TimestampUrl) { throw 'Signing requires CertificateThumbprint, SignToolPath and TimestampUrl together' }
if ($sign) {
    if ($CertificateThumbprint -notmatch '^[0-9a-fA-F]{40}$') { throw 'CertificateThumbprint must be the 40-digit SHA-1 certificate identifier' }
    if ($TimestampUrl -notmatch '^https://[^\s"@]+$') { throw 'TimestampUrl must be HTTPS without credentials' }
    $SignToolPath = (Resolve-Path $SignToolPath).Path
}
function Assert-Signature([string]$Path) {
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Thumbprint -ne $CertificateThumbprint -or -not $signature.TimeStamperCertificate) {
        throw "Expected timestamped Authenticode signature was not valid: $Path"
    }
}
$BinDir = (Resolve-Path $BinDir).Path
$CrtDir = (Resolve-Path $CrtDir).Path
# isocastd starts the WGC/camera helper isocast-capture.exe from its own folder.
$executables = @('isocast.exe', 'isocastd.exe', 'isocast-ui.exe', 'isocast-capture.exe')
foreach ($name in $executables) {
    if (-not (Test-Path -LiteralPath "$BinDir\$name" -PathType Leaf)) { throw "Missing executable: $BinDir\$name" }
}
if (-not (Test-Path "$CrtDir\vcruntime140.dll")) { throw 'CrtDir must be the x64 Microsoft Visual C++ redistributable CRT directory' }
if (Test-Path $OutputDir) { throw 'OutputDir must not exist; keep each package build separate' }
$OutputDir = (New-Item -ItemType Directory $OutputDir).FullName
$ToolsDir = (New-Item -ItemType Directory -Force $ToolsDir).FullName
$payload = (New-Item -ItemType Directory "$OutputDir\payload").FullName

# Inno Setup's licence permits this qualification without a purchase. Pin the
# compiler and its digest, rather than trusting the runner's preinstalled copy.
$innoVersion = '7.1.0'
$innoHash = '0362a383ed217d4c4239b5933866dd96d3eb2102737da92f80f6057a4b40df2f'
$installer = "$ToolsDir\innosetup-$innoVersion-x64.exe"
if (-not (Test-Path $installer)) {
    Invoke-WebRequest "https://github.com/jrsoftware/issrc/releases/download/is-7_1_0/innosetup-$innoVersion-x64.exe" -OutFile $installer -UseBasicParsing
}
if ((Get-FileHash $installer -Algorithm SHA256).Hash -ne $innoHash) { throw 'Inno Setup download SHA-256 mismatch' }
$compilerDir = "$ToolsDir\inno-$innoVersion"
if (-not (Test-Path "$compilerDir\ISCC.exe")) {
    $process = Start-Process $installer -ArgumentList @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/CURRENTUSER', ('/DIR="{0}"' -f $compilerDir)) -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "Inno Setup compiler installation failed: $($process.ExitCode)" }
}
foreach ($name in $executables) { Copy-Item "$BinDir\$name" $payload }
# App-local CRT avoids the machine-wide redistributable's admin prompt.
Copy-Item "$CrtDir\*.dll" $payload
. "$PSScriptRoot\winsparkle.ps1"
$sparkle = Get-WinSparkle $ToolsDir
Copy-Item "$sparkle\x64\Release\WinSparkle.dll", "$sparkle\bin\winsparkle-tool.exe" $payload
Copy-Item "$sparkle\COPYING" "$payload\LICENSE-WinSparkle"
Copy-Item "$sparkle\COPYING.expat" "$payload\LICENSE-WinSparkle-expat"
Copy-Item "$sparkle\isocast-build.json" "$payload\WinSparkle-build.json"
# Read trust/build metadata from the actual packaged executable, not a writable
# config file. This path initializes neither Slint nor the updater/network.
# isocast-ui.exe is a GUI program (no console window), so PowerShell's `&`
# would neither wait for it nor set its exit code: start it explicitly.
$infoFile = "$OutputDir\update-info.json"
$infoRun = Start-Process "$payload\isocast-ui.exe" -ArgumentList '--update-info' -NoNewWindow -Wait -PassThru -RedirectStandardOutput $infoFile
if ($infoRun.ExitCode -ne 0) { throw 'Packaged app cannot report its update identity' }
$info = Get-Content -Raw -LiteralPath $infoFile | ConvertFrom-Json
Remove-Item -LiteralPath $infoFile
if ($Version -and $Version -ne $info.build_version) { throw 'Version must match the build version embedded in the app' }
$Version = $info.build_version
if ($Version -notmatch '^\d+\.\d+\.\d+(\.\d+)?$') { throw 'App build version must be X.Y.Z or X.Y.Z.N' }
if ($sign -and $info.test_only) { throw 'A test-key/feed build cannot enter the production signing path' }
if ($sign) {
    foreach ($name in $executables) {
        & $SignToolPath sign /fd SHA256 /td SHA256 /tr $TimestampUrl /sha1 $CertificateThumbprint "$payload\$name"
        if ($LASTEXITCODE -ne 0) { throw "Authenticode signing failed: $name" }
        Assert-Signature "$payload\$name"
    }
}
foreach ($name in @('LICENSE-MIT', 'LICENSE-APACHE', 'THIRD_PARTY_NOTICES')) {
    Copy-Item "$repo\$name" $payload
}
$files = @(Get-ChildItem $payload -File | Sort-Object Name | ForEach-Object {
    [ordered]@{ name=$_.Name; bytes=$_.Length; sha256=(Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant() }
})
$metadata = [ordered]@{
    version=$Version
    architecture='x64'
    signed=$sign
    certificate_thumbprint=$CertificateThumbprint
    updater=$info
    winsparkle_version='0.9.4'
    winsparkle_patch_sha256=(Get-FileHash "$PSScriptRoot\winsparkle-download.patch" -Algorithm SHA256).Hash.ToLowerInvariant()
    winsparkle_source_build=$true
    compiler="Inno Setup $innoVersion"
    compiler_download_sha256=$innoHash
    files=$files
}
$metadata | ConvertTo-Json -Depth 4 | Set-Content "$payload\package.json" -Encoding UTF8
$arguments = @("/DAppVersion=$Version", "/DPayloadDir=$payload", "/DOutputDir=$OutputDir", "/DIconFile=$repo\crates\isocast-ui\windows\isocast.ico")
if ($sign) {
    # Inno's $q/$f placeholders quote both executable and signing input. It
    # signs the generated uninstaller as well as Setup, before our final hash.
    $command = '$q' + $SignToolPath + '$q sign /fd SHA256 /td SHA256 /tr $q' + $TimestampUrl + '$q /sha1 ' + $CertificateThumbprint + ' $f'
    $arguments += @('/DSignArtifacts=1', "/SIsocast=$command")
}
& "$compilerDir\ISCC.exe" @arguments "$PSScriptRoot\installer.iss"
if ($LASTEXITCODE -ne 0) { throw 'Installer compilation failed' }
$setup = "$OutputDir\Isocast-$Version-windows-x64-setup.exe"
if ($sign) { Assert-Signature $setup }
$hash = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $([IO.Path]::GetFileName($setup))" | Set-Content "$setup.sha256" -Encoding ascii
Write-Output "Installer (Authenticode signed: $sign; test-only updater: $($info.test_only)): $setup"
Write-Output "SHA-256: $hash"
