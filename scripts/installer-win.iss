; NuPhyLab Windows installer, driven by scripts/package-win.mjs:
;   iscc installer-win.iss /DAppVersion=1.0.11 /DSourceDir=<layout> /DOutputDir=<dir> /DOutputFile=<name>
; Per-user install (no UAC): HID access and global key hooks need no elevation,
; and the macOS build is likewise a per-user application.
#define AppName "NuPhyLab"
#define AppPublisher "NuPhyLab"
#define AppURL "https://github.com/bohu8264/NuPhyLab"
; Stable identity: Windows ties uninstall entries and upgrade-in-place to this
; GUID, never to the version or the certificate.
#define AppId "{{8E5B4C6D-9A21-4E7F-B3D8-1C2F5A7E9B40}}"

[Setup]
AppId={#AppId}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} v{#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\NuPhyLab.exe
OutputDir={#OutputDir}
OutputBaseFilename={#OutputFile}
LicenseFile={#SourceDir}\LICENSE
Compression=lzma2
SolidCompression=yes
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
CloseApplications=no
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\NuPhyLab.exe"
Name: "{group}\{cm:UninstallProgram,{#AppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\NuPhyLab.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\NuPhyLab.exe"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[Code]
// The shell owns its Node/HIDBridge children through a kill-on-close Job
// Object, so stopping NuPhyLab.exe brings the whole tree down. HIDBridge.exe
// is killed by name too in case a previous session crashed mid-exit. Generic
// node.exe is never touched: the user may run unrelated Node processes.
procedure StopOurProcesses;
var
  ResultCode: Integer;
begin
  Exec(ExpandConstant('{cmd}'), '/C taskkill /f /im NuPhyLab.exe /t', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
  Exec(ExpandConstant('{cmd}'), '/C taskkill /f /im HIDBridge.exe /t', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopOurProcesses;
  Result := '';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    StopOurProcesses;
end;
