; Installs for the current user only, where the updater may replace the program.
[Setup]
AppId=Rustbeat
AppName=Rustbeat
AppVersion={#Version}
AppPublisher=LolloCCP
DefaultDirName={localappdata}\Programs\Rustbeat
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
LicenseFile=LICENSE
; Shown before installing: what Rustbeat sends where.
InfoBeforeFile=PRIVACY.txt
VersionInfoVersion={#Copy(Version, 2)}
VersionInfoProductName=Rustbeat
OutputDir=target\release
OutputBaseFilename=rustbeatinstaller
UninstallDisplayIcon={app}\rustbeat.exe
CloseApplications=force
WizardStyle=modern

[Tasks]
Name: desktop; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked

[Files]
Source: "target\release\rustbeat.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Rustbeat"; Filename: "{app}\rustbeat.exe"
Name: "{autodesktop}\Rustbeat"; Filename: "{app}\rustbeat.exe"; Tasks: desktop

[Run]
Filename: "{app}\rustbeat.exe"; Description: "{cm:LaunchProgram,Rustbeat}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
Type: files; Name: "{app}\rustbeat.old.exe"
Type: files; Name: "{app}\rustbeat.new"
