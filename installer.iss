; Installs for the current user only, where the updater may replace the program.
[Setup]
AppId=Quaver
AppName=Quaver
AppVersion={#Version}
AppPublisher=LolloCCP
DefaultDirName={localappdata}\Programs\Quaver
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
LicenseFile=LICENSE
OutputDir=target\release
OutputBaseFilename=quaverinstaller
UninstallDisplayIcon={app}\quaver.exe
CloseApplications=force
WizardStyle=modern

[Tasks]
Name: desktop; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked

[Files]
Source: "target\release\quaver.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Quaver"; Filename: "{app}\quaver.exe"
Name: "{autodesktop}\Quaver"; Filename: "{app}\quaver.exe"; Tasks: desktop

[Run]
Filename: "{app}\quaver.exe"; Description: "{cm:LaunchProgram,Quaver}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
Type: files; Name: "{app}\quaver.old.exe"
Type: files; Name: "{app}\quaver.new"
