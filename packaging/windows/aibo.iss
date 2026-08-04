; Per-user Windows installer. It deliberately never asks for elevation:
; binaries live under LocalAppData while the app's existing configuration and
; history remain under AppData and are therefore untouched by upgrades and
; uninstall.

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0-dev"
#endif

#ifndef MySourceExe
  #define MySourceExe "..\..\target\release\aibo.exe"
#endif

#ifndef MyOutputDir
  #define MyOutputDir "..\..\dist"
#endif

#ifndef MySetupIcon
  #define MySetupIcon "aibo.ico"
#endif

[Setup]
AppId={{B640E8CE-6E72-47C3-AB84-99E312265E11}
AppName=aibo
AppVersion={#MyAppVersion}
AppPublisher=aibo
AppPublisherURL=https://github.com/Ameyanagi/aibo
AppSupportURL=https://github.com/Ameyanagi/aibo/issues
AppUpdatesURL=https://github.com/Ameyanagi/aibo/releases
DefaultDirName={localappdata}\Programs\aibo
DefaultGroupName=aibo
DisableProgramGroupPage=yes
DirExistsWarning=no
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#MyOutputDir}
OutputBaseFilename=aibo-windows-x86_64-setup
SetupIconFile={#MySetupIcon}
UninstallDisplayIcon={app}\aibo.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
UsePreviousAppDir=yes
UsePreviousTasks=yes

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "{#MySourceExe}"; DestDir: "{app}"; DestName: "aibo.exe"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\aibo"; Filename: "{app}\aibo.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\aibo"; Filename: "{app}\aibo.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
; The updater invokes Setup silently after an explicit in-app confirmation.
; Let that path relaunch the app too; `skipifsilent` left a successful update
; looking like a crash because nothing came back after the old process exited.
Filename: "{app}\aibo.exe"; Description: "Launch aibo"; Flags: nowait postinstall
