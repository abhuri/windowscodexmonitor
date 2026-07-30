#ifndef MyAppVersion
  #define MyAppVersion "0.1.9"
#endif

#define MyAppName "Windows Codex Monitor"
#define MyAppExeName "WindowsCodexMonitor.exe"

[Setup]
AppId={{B828BD9E-287D-4C94-965E-62C73FD4C21E}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher=abhuri
AppPublisherURL=https://github.com/abhuri/windowscodexmonitor
DefaultDirName={localappdata}\Programs\Windows Codex Monitor
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=WindowsCodexMonitor-Setup-win-x64
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
UninstallDisplayIcon={app}\{#MyAppExeName}

[Files]
Source: "..\target\release\windowscodexmonitor.exe"; DestDir: "{app}"; DestName: "{#MyAppExeName}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent
