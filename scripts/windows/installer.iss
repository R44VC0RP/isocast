; Per-user, ordinary Win32 layout: the clients find the adjacent isocastd.exe.
; No service, PATH change, elevation, forced process exit or user-data cleanup.
#ifndef AppVersion
  #error AppVersion is required
#endif
#ifndef PayloadDir
  #error PayloadDir is required
#endif
#ifndef OutputDir
  #error OutputDir is required
#endif

[Setup]
AppId=Isocast
AppName=Isocast
AppVersion={#AppVersion}
AppPublisher=Isocast
AppPublisherURL=https://github.com/R44VC0RP/isocast
DefaultDirName={localappdata}\Programs\Isocast
DefaultGroupName=Isocast
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.19045
DisableProgramGroupPage=yes
DisableDirPage=yes
UsePreviousAppDir=yes
UninstallDisplayIcon={app}\isocast-ui.exe
#ifdef IconFile
; The installer's own icon: the app's (crates/isocast-ui/windows/isocast.ico).
SetupIconFile={#IconFile}
#endif
OutputDir={#OutputDir}
OutputBaseFilename=Isocast-{#AppVersion}-windows-x64-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=no
RestartApplications=no
RestartIfNeededByRun=no
SetupLogging=yes
TimeStampsInUTC=yes
#ifdef SignArtifacts
SignTool=Isocast
SignedUninstaller=yes
#endif

[Files]
Source: "{#PayloadDir}\*"; DestDir: "{app}"; Flags: ignoreversion notimestamp

[Icons]
Name: "{userprograms}\Isocast"; Filename: "{app}\isocast-ui.exe"; WorkingDir: "{app}"

[Run]
Filename: "{app}\isocast-ui.exe"; Description: "Open Isocast"; Flags: nowait postinstall skipifsilent

[Code]
function CreateFile(FileName: String; DesiredAccess, ShareMode: LongWord;
  SecurityAttributes: NativeUInt; CreationDisposition, FlagsAndAttributes: LongWord;
  TemplateFile: THandle): THandle;
  external 'CreateFileW@kernel32.dll stdcall';
function CloseHandle(Handle: THandle): LongBool;
  external 'CloseHandle@kernel32.dll stdcall';

function FileInUse(const Path: String): Boolean;
var
  Handle: THandle;
begin
  Result := False;
  if not FileExists(Path) then Exit;
  { An exclusive write handle is refused for a running executable image.
    Close it immediately: no content is changed. }
  Handle := CreateFile(Path, $C0000000, 0, 0, 3, $80, 0);
  Result := Handle = THandle(-1);
  if not Result then CloseHandle(Handle);
end;

function FilesInUse(): Boolean;
var
  Found: TFindRec;
begin
  Result := False;
  if FindFirst(ExpandConstant('{app}\*'), Found) then begin
    try
      repeat
        if (CompareText(Found.Name, 'isocast.exe') = 0) or
           (CompareText(Found.Name, 'isocastd.exe') = 0) or
           (CompareText(Found.Name, 'isocast-ui.exe') = 0) or
           (CompareText(ExtractFileExt(Found.Name), '.dll') = 0) then begin
          if FileInUse(ExpandConstant('{app}\') + Found.Name) then begin
            Result := True;
            Exit;
          end;
        end;
      until not FindNext(Found);
    finally
      FindClose(Found);
    end;
  end;
end;

function StopIdleDaemon(const Client: String): Boolean;
var
  Code: Integer;
begin
  { The CLI uses the same atomic shutdown_if_idle API as the Mac updater.
    It refuses busy work, never starts a daemon, and bounds its IPC waits. }
  Result := Exec(Client, 'engine shutdown --json', ExtractFileDir(Client),
    SW_HIDE, ewWaitUntilTerminated, Code) and (Code = 0);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if FileInUse(ExpandConstant('{app}\isocast-ui.exe')) or
     FileInUse(ExpandConstant('{app}\isocast.exe')) then begin
    Result := 'Close Isocast and any running Isocast CLI commands before installing. Setup never closes them for you.';
    Exit;
  end;
  { Use the incoming CLI: older installations may not have this command.
    Extract its app-local runtime too, without changing installed files. }
  ExtractTemporaryFiles('{app}\isocast.exe');
  ExtractTemporaryFiles('{app}\*.dll');
  if not StopIdleDaemon(ExpandConstant('{tmp}\') + '{app}\isocast.exe') then begin
    Result := 'The recording engine is busy or could not be reached. Stop all Isocast work, then retry. No application files have been changed.';
    Exit;
  end;
  if FilesInUse() then
    Result := 'An Isocast executable or DLL is still in use. Close its clients, then retry. Setup never terminates a process.';
end;

function InitializeUninstall(): Boolean;
begin
  { Removing an unused/broken installation must not depend on its CLI still
    being runnable. If no packaged image is in use, no process is affected. }
  Result := not FilesInUse();
  if Result then Exit;
  if not FileInUse(ExpandConstant('{app}\isocast-ui.exe')) and
     not FileInUse(ExpandConstant('{app}\isocast.exe')) then
    if StopIdleDaemon(ExpandConstant('{app}\isocast.exe')) then
      Result := not FilesInUse();
  if not Result then
    SuppressibleMsgBox('Close Isocast and stop all recording, streaming, replay, camera and export work before uninstalling. The engine must accept an idle shutdown. Recordings and settings are kept.', mbError, MB_OK, IDOK);
end;

function InitializeSetup(): Boolean;
var
  Previous: String;
  OldVersion, NewVersion: Int64;
  Index: Integer;
begin
  Result := True;
  { Do not let command-line overrides enable Restart Manager termination. }
  for Index := 1 to ParamCount do begin
    if (CompareText(ParamStr(Index), '/CLOSEAPPLICATIONS') = 0) or
       (CompareText(ParamStr(Index), '/FORCECLOSEAPPLICATIONS') = 0) then begin
      SuppressibleMsgBox('Isocast Setup does not close applications. Stop your work and close Isocast yourself before installing.', mbError, MB_OK, IDOK);
      Result := False;
      Exit;
    end;
  end;
  if RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\Isocast_is1', 'DisplayVersion', Previous) and
     StrToVersion(Previous, OldVersion) and StrToVersion('{#AppVersion}', NewVersion) and
     (OldVersion > NewVersion) then begin
    Result := ExpandConstant('{param:ALLOWDOWNGRADE|0}') = '1';
    if not Result then
      SuppressibleMsgBox('A newer Isocast is installed. To roll back deliberately, close Isocast and run the previous trusted installer with /ALLOWDOWNGRADE=1. Your recordings and settings are kept.', mbError, MB_OK, IDOK);
  end;
end;
