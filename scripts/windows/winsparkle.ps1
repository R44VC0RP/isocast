# Shared by packaging and appcast generation. Builds WinSparkle from its pinned
# official source with Isocast's download patch applied. Never returns the
# prebuilt SDK: its DLL lacks the patch. Every failure throws; there is no
# fallback to an unpatched DLL.
function Get-WinSparkle([string]$ToolsDir) {
    $version = '0.9.4'
    # Bump when the build recipe changes, so older caches are rebuilt.
    $recipe = 2
    $sourceHash = 'CD77040F828DCE0ADCF8F0205F776238197DEB24F5108BF01A3E5623B703A3B9'
    $sourceSize = 64991527
    # The source's packages.config, restored from nuget.org by hash rather than
    # with nuget.exe. These SHA-256s pin the exact package archives.
    $packages = @(
        @{ Id = 'Gettext.Tools'; Version = '0.26.0'; Hash = '8E2FF0A055778AD72C0077E09A30028011816C9EAC5168AA2C88101A4DDDE1A0' },
        @{ Id = 'Microsoft.Web.WebView2'; Version = '1.0.3967.48'; Hash = 'C66357AC7F324EC9BCAFE5241706A023B4122D8C22300C31DE4B0EB220DB689E' }
    )
    # x64 DLL of the unpatched official 0.9.4 SDK; it must never be returned.
    $unsafeDllHashes = @('9B43B1C16EE39FB9A91B5BD75138767898779510E0836BE2919250607CDBE8AB')
    $marker = 'win_sparkle_isocast_confined_downloads'

    function Invoke-Native([string]$What, [string]$Exe, [string[]]$Arguments) {
        $ErrorActionPreference = 'Continue'
        & $Exe @Arguments | Out-Host
        if ($LASTEXITCODE -ne 0) { throw "$What failed with exit code $LASTEXITCODE" }
    }
    function Get-Pinned([string]$Url, [string]$Path, [string]$Hash) {
        if (-not (Test-Path -LiteralPath $Path)) {
            $ProgressPreference = 'SilentlyContinue'
            Invoke-WebRequest $Url -OutFile "$Path.part" -UseBasicParsing
            if ((Get-FileHash -LiteralPath "$Path.part" -Algorithm SHA256).Hash -ne $Hash) { throw "SHA-256 mismatch: $Path.part" }
            Move-Item -LiteralPath "$Path.part" $Path -Force
        }
        if ((Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash -ne $Hash) { throw "SHA-256 mismatch: $Path" }
    }
    function Test-Build([string]$Out, [string]$PatchHash) {
        try {
            $m = Get-Content -LiteralPath "$Out\isocast-build.json" -Raw | ConvertFrom-Json
            $dll = (Get-FileHash -LiteralPath "$Out\x64\Release\WinSparkle.dll" -Algorithm SHA256).Hash
            $tool = (Get-FileHash -LiteralPath "$Out\bin\winsparkle-tool.exe" -Algorithm SHA256).Hash
            $bytes = [IO.File]::ReadAllBytes("$Out\x64\Release\WinSparkle.dll")
            $hasMarker = [Text.Encoding]::ASCII.GetString($bytes).Contains("$marker`0")
            return ($m.recipe -eq $recipe -and $m.version -eq $version -and $m.source_sha256 -eq $sourceHash -and
                $m.patch_sha256 -eq $PatchHash -and $m.dll_sha256 -eq $dll -and $m.tool_sha256 -eq $tool -and
                $unsafeDllHashes -notcontains $dll -and $hasMarker -and
                (Test-Path -LiteralPath "$Out\COPYING") -and (Test-Path -LiteralPath "$Out\COPYING.expat"))
        } catch {
            return $false
        }
    }

    $patch = Join-Path $PSScriptRoot 'winsparkle-download.patch'
    if (-not (Test-Path -LiteralPath $patch -PathType Leaf)) { throw "WinSparkle security patch missing: $patch" }
    $patchHash = (Get-FileHash -LiteralPath $patch -Algorithm SHA256).Hash
    $ToolsDir = (New-Item -ItemType Directory -Force $ToolsDir).FullName
    # Short folder name: wxWidgets' source tree is deep and MSBuild still has MAX_PATH limits.
    $work = Join-Path $ToolsDir "winsparkle-$version-$($patchHash.Substring(0, 12).ToLowerInvariant())"
    $out = Join-Path $work 'out'
    if (Test-Build $out $patchHash) { return $out }

    $archive = Join-Path $ToolsDir "WinSparkle-$version-src.zip"
    Get-Pinned "https://github.com/vslavik/winsparkle/releases/download/v$version/WinSparkle-$version-src.zip" $archive $sourceHash
    if ((Get-Item -LiteralPath $archive).Length -ne $sourceSize) { throw "WinSparkle source size mismatch: $archive" }
    foreach ($p in $packages) {
        $p.Path = Join-Path $ToolsDir "$($p.Id).$($p.Version).nupkg"
        Get-Pinned "https://api.nuget.org/v3-flatcontainer/$($p.Id.ToLowerInvariant())/$($p.Version)/$($p.Id.ToLowerInvariant()).$($p.Version).nupkg" $p.Path $p.Hash
    }

    if (Test-Path -LiteralPath $work) { Remove-Item -LiteralPath $work -Recurse -Force }
    $src = Join-Path $work 'src'
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [IO.Compression.ZipFile]::ExtractToDirectory($archive, $src)
    foreach ($p in $packages) {
        [IO.Compression.ZipFile]::ExtractToDirectory($p.Path, (Join-Path $src "packages\$($p.Id).$($p.Version)"))
    }

    # Its own repository, so git applies paths relative to the source root even
    # when ToolsDir sits inside another checkout.
    Invoke-Native 'git init' 'git' @('-C', $src, 'init', '-q')
    Invoke-Native 'WinSparkle patch check' 'git' @('-C', $src, '-c', 'core.autocrlf=false', 'apply', '--check', '--whitespace=nowarn', $patch)
    Invoke-Native 'WinSparkle patch' 'git' @('-C', $src, '-c', 'core.autocrlf=false', 'apply', '--whitespace=nowarn', $patch)

    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere)) { throw 'vswhere.exe not found; install Visual Studio Build Tools with the C++ x64 tools' }
    $vs = & $vswhere -latest -products * -requires Microsoft.Component.MSBuild Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -format json | ConvertFrom-Json | Select-Object -First 1
    if (-not $vs) { throw 'No Visual Studio installation with MSBuild and the C++ x64 tools' }
    $msbuild = Join-Path $vs.installationPath 'MSBuild\Current\Bin\amd64\MSBuild.exe'
    if (-not (Test-Path -LiteralPath $msbuild)) { throw "MSBuild not found: $msbuild" }
    # /Brepro drops timestamps, /d1trimfile the build folder from __FILE__ and
    # /PDBALTPATH the PDB folder, so equal inputs and toolchain give equal
    # binaries wherever ToolsDir is. UserRootDir keeps per-user property sheets
    # out of the build. Every build is a fresh tree, so incremental file
    # tracking is useless. MSBuild's FileTracker also crashed (IndexOutOfRange
    # in FileIsUnderNormalizedPath) in a build with redirected TEMP/profile
    # folders.
    $previousCl = $env:_CL_
    $previousLink = $env:_LINK_
    $env:_CL_ = "/Brepro /d1trimfile:$src\"
    $env:_LINK_ = '/Brepro /PDBALTPATH:%_PDB%'
    try {
        Invoke-Native 'WinSparkle build' $msbuild @("$src\WinSparkle.sln", '-nologo', '-m', '-nr:false', '-v:q',
            '-t:WinSparkle;winsparkle-tool', '-p:Configuration=Release', '-p:Platform=x64',
            "-p:UserRootDir=$work\no-user-props", '-p:TrackFileAccess=false', "-flp:LogFile=$work\msbuild.log;Verbosity=normal")
    } finally {
        $env:_CL_ = $previousCl
        $env:_LINK_ = $previousLink
    }

    New-Item -ItemType Directory -Force "$out\x64\Release", "$out\bin" | Out-Null
    Copy-Item -LiteralPath "$src\x64\Release\WinSparkle.dll" "$out\x64\Release\"
    Copy-Item -LiteralPath "$src\x64\Release\winsparkle-tool.exe" "$out\bin\"
    Copy-Item -LiteralPath "$src\COPYING" "$out\COPYING"
    Copy-Item -LiteralPath "$src\3rdparty\expat\expat\COPYING" "$out\COPYING.expat"
    [ordered]@{
        recipe = $recipe
        version = $version
        source_sha256 = $sourceHash
        patch_sha256 = $patchHash
        packages = @($packages | ForEach-Object { [ordered]@{ id = $_.Id; version = $_.Version; sha256 = $_.Hash } })
        visual_studio = "$($vs.displayName) $($vs.installationVersion)"
        dll_sha256 = (Get-FileHash -LiteralPath "$out\x64\Release\WinSparkle.dll" -Algorithm SHA256).Hash
        tool_sha256 = (Get-FileHash -LiteralPath "$out\bin\winsparkle-tool.exe" -Algorithm SHA256).Hash
    } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath "$out\isocast-build.json" -Encoding UTF8
    if (-not (Test-Build $out $patchHash)) { throw "Built WinSparkle failed validation (patch marker $marker or hashes): $out" }
    return $out
}
