# Runs as the guest administrator over SSH, never on exon-desk itself.
# Only a pristine packager account can be initialized; later calls are idempotent.
$ErrorActionPreference = 'Stop'
if ($env:COMPUTERNAME -ne 'ISOCAST-PKG') { throw 'Desktop configuration is restricted to the ISOCAST-PKG guest' }
$user = Get-LocalUser -Name packager
$marker = 'HKLM:\SOFTWARE\Isocast\VmDesktop'
$winlogon = 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon'
$configured = (Get-ItemProperty -LiteralPath $marker -Name Version -ErrorAction SilentlyContinue).Version -eq 1
$profile = (Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\$($user.SID.Value)").ProfileImagePath
if (-not $configured) {
    # An administrative password reset invalidates existing DPAPI profile data;
    # never weaken this pristine-baseline restriction for an upgrade/retest.
    if (Test-Path -LiteralPath "$profile\AppData\Local\Programs\Isocast") {
        throw 'Restore the pristine clean baseline before first-time desktop setup; refusing to reset an installed account'
    }
    Add-Type @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
public static class DesktopSecret {
    [StructLayout(LayoutKind.Sequential)] struct Unicode { public ushort Length, MaximumLength; public IntPtr Buffer; }
    [StructLayout(LayoutKind.Sequential)] struct Attributes { public uint Length; public IntPtr Root, Name; public uint Flags; public IntPtr Security, Quality; }
    [DllImport("advapi32.dll")] static extern uint LsaOpenPolicy(IntPtr system, ref Attributes attrs, uint access, out IntPtr handle);
    [DllImport("advapi32.dll")] static extern uint LsaStorePrivateData(IntPtr handle, ref Unicode key, ref Unicode value);
    [DllImport("advapi32.dll")] static extern uint LsaClose(IntPtr handle);
    [DllImport("advapi32.dll")] static extern uint LsaNtStatusToWinError(uint status);
    static Unicode Text(string value) { return new Unicode { Length=(ushort)(value.Length*2), MaximumLength=(ushort)((value.Length+1)*2), Buffer=Marshal.StringToHGlobalUni(value) }; }
    static void Check(uint status) { if (status != 0) throw new Win32Exception((int)LsaNtStatusToWinError(status)); }
    public static void Store(string password) {
        var attrs=new Attributes(); attrs.Length=(uint)Marshal.SizeOf(attrs);
        IntPtr handle; Check(LsaOpenPolicy(IntPtr.Zero,ref attrs,0x20,out handle));
        var key=Text("DefaultPassword"); var value=Text(password);
        try { Check(LsaStorePrivateData(handle,ref key,ref value)); }
        finally { Marshal.ZeroFreeGlobalAllocUnicode(value.Buffer); Marshal.FreeHGlobal(key.Buffer); LsaClose(handle); }
    }
}
'@
    $random = New-Object byte[] 32
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    try { $rng.GetBytes($random) } finally { $rng.Dispose() }
    $password = [Convert]::ToBase64String($random) + 'aA1!'
    # No plaintext file, registry value, argument, stdout or log. The password
    # exists only in this process and in Windows' protected LSA/account stores.
    [DesktopSecret]::Store($password)
    Set-LocalUser -Name packager -Password (ConvertTo-SecureString $password -AsPlainText -Force) -PasswordNeverExpires $true
    $password = $null
    [Array]::Clear($random,0,$random.Length)
}
# Repair non-secret autologon settings without rotating or retrieving the secret.
Remove-ItemProperty -LiteralPath $winlogon -Name DefaultPassword -ErrorAction SilentlyContinue
Remove-ItemProperty -LiteralPath $winlogon -Name AutoLogonCount -ErrorAction SilentlyContinue
Set-ItemProperty -LiteralPath $winlogon -Name DefaultUserName -Value packager
Set-ItemProperty -LiteralPath $winlogon -Name DefaultDomainName -Value $env:COMPUTERNAME
Set-ItemProperty -LiteralPath $winlogon -Name AutoAdminLogon -Value '1'

# Load only the test user's hive when it is not mounted; no machine-wide
# lock-screen/screen-saver policy and no modifications to the admin's profile.
$sid = $user.SID.Value
$loaded = $false
if (-not (Test-Path "Registry::HKEY_USERS\$sid")) {
    & reg.exe load "HKU\$sid" "$profile\NTUSER.DAT" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Could not load packager profile hive' }
    $loaded = $true
}
try {
    $desktop = "Registry::HKEY_USERS\$sid\Control Panel\Desktop"
    Set-ItemProperty -LiteralPath $desktop -Name ScreenSaveActive -Value '0'
    Set-ItemProperty -LiteralPath $desktop -Name ScreenSaverIsSecure -Value '0'
    $policy = "Registry::HKEY_USERS\$sid\Software\Policies\Microsoft\Windows\Control Panel\Desktop"
    New-Item -Path $policy -Force | Out-Null
    Set-ItemProperty -LiteralPath $policy -Name ScreenSaveActive -Value '0'
    Set-ItemProperty -LiteralPath $policy -Name ScreenSaverIsSecure -Value '0'
    $system = "Registry::HKEY_USERS\$sid\Software\Microsoft\Windows\CurrentVersion\Policies\System"
    New-Item -Path $system -Force | Out-Null
    New-ItemProperty -LiteralPath $system -Name DisableLockWorkstation -Value 1 -PropertyType DWord -Force | Out-Null
} finally {
    if ($loaded) {
        [GC]::Collect(); [GC]::WaitForPendingFinalizers()
        & reg.exe unload "HKU\$sid" | Out-Null
        if ($LASTEXITCODE -ne 0) { throw 'Could not unload packager profile hive' }
    }
}
New-Item -Path $marker -Force | Out-Null
New-ItemProperty -LiteralPath $marker -Name Version -Value 1 -PropertyType DWord -Force | Out-Null
$sessions = @(Get-CimInstance Win32_Process -Filter "Name='explorer.exe'" | Where-Object { (Invoke-CimMethod -InputObject $_ -MethodName GetOwner).User -eq 'packager' } | Select-Object -ExpandProperty SessionId -Unique)
$locked = @(Get-Process LogonUI -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -in $sessions })
if ((Get-Item -LiteralPath $winlogon).GetValueNames() -contains 'DefaultPassword') { throw 'Unexpected plaintext registry password' }
@{ok=$true;configured_before=$configured;restart_required=(-not $configured -or $sessions.Count -ne 1 -or $locked.Count -gt 0);credential_storage='LSA secret'} | ConvertTo-Json -Compress
