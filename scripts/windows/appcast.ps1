# Generate the Windows feed alongside (never instead of) Sparkle's Mac feed.
# Takes only a public signature; private signing material belongs to the
# release operator. Generates files locally, never uploads or publishes.
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$PackageDir,
    [Parameter(Mandatory)][string]$Signature,
    [Parameter(Mandatory)][string]$DownloadUrl,
    [Parameter(Mandatory)][string]$ToolsDir,
    [string]$ExpectedCertificateThumbprint,
    [switch]$AllowUnsignedTest
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repo = (Resolve-Path "$PSScriptRoot\..\..").Path
$PackageDir = (Resolve-Path $PackageDir).Path
$metadata = Get-Content "$PackageDir\payload\package.json" -Raw | ConvertFrom-Json
$version = $metadata.version
$info = $metadata.updater
if ($version -notmatch '^\d+\.\d+\.\d+(\.\d+)?$' -or $version -ne $info.build_version) { throw 'Package build identity mismatch' }
if ($info.version -notmatch '^\d+\.\d+\.\d+$') { throw 'Product version must be X.Y.Z' }
if ($Signature -notmatch '^[A-Za-z0-9+/]{86}==$') { throw 'Expected a base64 Ed25519 signature' }
$uri = [uri]$DownloadUrl
if (-not $uri.IsAbsoluteUri -or $uri.UserInfo -or $uri.Fragment -or $uri.Query -or
    ($uri.Scheme -ne 'https' -and -not ($AllowUnsignedTest -and $uri.Scheme -eq 'http' -and $uri.Host -eq '127.0.0.1'))) {
    throw 'DownloadUrl must be HTTPS without credentials (explicit test feeds also allow loopback HTTP)'
}
$installer = "$PackageDir\Isocast-$version-windows-x64-setup.exe"
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) { throw 'Package installer missing' }
$hash = (Get-FileHash $installer -Algorithm SHA256).Hash.ToLowerInvariant()
$checksum = (Get-Content "$installer.sha256" -Raw).Trim()
if ($checksum -ne "$hash  $([IO.Path]::GetFileName($installer))") { throw 'Package checksum does not match the installer' }
if ([uri]::UnescapeDataString($uri.Segments[-1]) -ne [IO.Path]::GetFileName($installer)) { throw 'DownloadUrl filename must match the installer' }
if (-not $AllowUnsignedTest) {
    if ($version -ne $info.version) { throw 'Production Windows build version must match the workspace release version' }
    if ($info.test_only -or -not $metadata.signed -or -not $ExpectedCertificateThumbprint) { throw 'Production appcasts require a production-key build and an expected Authenticode certificate' }
    $key = (Get-Content "$repo\scripts\sparkle\public-ed-key.txt" | Where-Object { $_ -and -not $_.StartsWith('#') }).Trim()
    if ($key -ne $info.public_key -or $info.feed_url -ne 'https://github.com/R44VC0RP/isocast/releases/latest/download/appcast-windows.xml') { throw 'Production app update root/feed mismatch' }
    $auth = Get-AuthenticodeSignature -LiteralPath $installer
    if ($auth.Status -ne 'Valid' -or $auth.SignerCertificate.Thumbprint -ne $ExpectedCertificateThumbprint -or
        $metadata.certificate_thumbprint -ne $ExpectedCertificateThumbprint -or -not $auth.TimeStamperCertificate) { throw 'Installer Authenticode identity/timestamp mismatch' }
} elseif (-not $info.test_only) {
    throw 'Unsigned test feeds require an explicitly test-only app build'
}
. "$PSScriptRoot\winsparkle.ps1"
$sdk = Get-WinSparkle $ToolsDir
& "$sdk\bin\winsparkle-tool.exe" verify --public-key $info.public_key --signature $Signature $installer
if ($LASTEXITCODE -ne 0) { throw 'Installer Ed25519 signature does not match its embedded update key' }
$notes = Get-Content "$repo\docs\releases\$($info.version).md" -Raw -Encoding UTF8
if ([string]::IsNullOrWhiteSpace($notes)) { throw 'Committed release notes are required' }
$xml = [xml]'<?xml version="1.0" encoding="utf-8"?><rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle"><channel><title>Isocast for Windows</title><item/></channel></rss>'
$item = $xml.SelectSingleNode('/rss/channel/item')
function Element([string]$Name, [string]$Value) {
    $node = $xml.CreateElement($Name)
    $node.InnerText = $Value
    [void]$item.AppendChild($node)
}
Element 'title' "Isocast $($info.version) (build $version)"
Element 'pubDate' ((Get-Item $installer).LastWriteTimeUtc.ToString('r'))
Element 'description' ('<pre style="white-space:pre-wrap">' + [Net.WebUtility]::HtmlEncode($notes) + '</pre>')
$enclosure = $xml.CreateElement('enclosure')
$enclosure.SetAttribute('url', $DownloadUrl)
$enclosure.SetAttribute('length', (Get-Item $installer).Length.ToString([Globalization.CultureInfo]::InvariantCulture))
$enclosure.SetAttribute('type', 'application/octet-stream')
$ns = 'http://www.andymatuschak.org/xml-namespaces/sparkle'
foreach ($pair in @(@('version',$version), @('shortVersionString',$info.version), @('os','windows'), @('edSignature',$Signature))) {
    $attribute = $xml.CreateAttribute('sparkle', $pair[0], $ns)
    $attribute.Value = $pair[1]
    [void]$enclosure.Attributes.Append($attribute)
}
[void]$item.AppendChild($enclosure)
$name = if ($AllowUnsignedTest) { 'appcast-windows-test.xml' } else { 'appcast-windows.xml' }
$output = Join-Path $PackageDir $name
if (Test-Path $output) { throw 'Appcast already exists; use a fresh package directory' }
$xml.Save($output)
# Keep the public signature for a deliberate offline rollback. No key material.
[IO.File]::WriteAllText("$installer.ed25519", $Signature + "`n", [Text.UTF8Encoding]::new($false))
Write-Output "Verified Windows appcast: $output"
