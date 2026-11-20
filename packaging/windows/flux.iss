; Inno Setup script for Flux. Build with packaging/windows/build-windows.ps1.
; Per-user installation is the default and requires no administrator rights.
; Uninstall removes only installed application files; user data stays in
; %LOCALAPPDATA%\flux and %APPDATA%\flux.

#ifndef MyAppVersion
  #define MyAppVersion "0.0.0"
#endif
#ifndef MyArch
  #define MyArch "x86_64"
#endif
#ifndef MyStage
  #define MyStage "..\..\dist\Flux"
#endif
#ifndef MyOutput
  #define MyOutput "..\..\dist"
#endif

[Setup]
AppId={{96707750-32C7-4982-8664-11AE5B7B5C1B}
AppName=Flux
AppVersion={#MyAppVersion}
AppVerName=Flux {#MyAppVersion}
AppPublisher=NeonMC23
AppPublisherURL=https://github.com/NeonMC23/Flux
AppSupportURL=https://github.com/NeonMC23/Flux
AppUpdatesURL=https://github.com/NeonMC23/Flux
DefaultDirName={autopf}\Flux
DefaultGroupName=Flux
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputDir={#MyOutput}
OutputBaseFilename=Flux-{#MyAppVersion}-windows-{#MyArch}-setup
SetupIconFile=..\..\assets\icons\flux.ico
UninstallDisplayIcon={app}\flux.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
VersionInfoVersion={#MyAppVersion}
VersionInfoCompany=NeonMC23
VersionInfoDescription=Flux installer
VersionInfoProductName=Flux
VersionInfoProductVersion={#MyAppVersion}

#if MyArch == "arm64"
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
#else
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
#endif

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional icons:"; Flags: unchecked

[Files]
Source: "{#MyStage}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\Flux"; Filename: "{app}\flux.exe"
Name: "{autodesktop}\Flux"; Filename: "{app}\flux.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\flux.exe"; Description: "Launch Flux"; Flags: nowait postinstall skipifsilent

; Deliberately no [UninstallDelete] entries for user data. The canonical
; library, queue, ratings, history, lyrics and artwork are not installer files.
