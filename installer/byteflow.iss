; ByteFlow Desktop — Windows installer (Inno Setup 6.4+ or 7).
;
; Don't compile this by hand: scripts\installer.ps1 (or scripts\build.ps1
; -Installer) passes the version and the dist folder of a finished release
; build, and writes the setup next to the binaries in dist\v<version>\.
;
;   ISCC /DAppVersion=0.3.0 /DDistDir=..\dist\v0.3.0 /O..\dist\v0.3.0 byteflow.iss
;
; * Installs for the current user (no admin prompt) by default; the
;   privileges dialog offers "all users" too.
; * Detects a running ByteFlow (its single-instance mutex) and asks to close it.
; * Uninstalling removes the program only. The vault (credentials, keys,
;   listing cache, thumbnails) lives in %LOCALAPPDATA%\ByteFlow and is left
;   alone: use Settings → Danger zone → "Wipe everything" to erase it.

#ifndef AppVersion
  #error Pass /DAppVersion=<x.y.z> (scripts\installer.ps1 does).
#endif
#ifndef DistDir
  #error Pass /DDistDir=<dist\vX.Y.Z folder> (scripts\installer.ps1 does).
#endif
#define FontsDir AddBackslash(SourcePath) + "..\crates\byteflow\assets\fonts"
#define IconFile AddBackslash(SourcePath) + "..\crates\byteflow\assets\icon\byteflow.ico"

[Setup]
; Never change AppId: it ties upgrades and the uninstaller together.
AppId={{2863E512-E792-4841-92A8-E595746A0C78}
AppName=ByteFlow
AppVersion={#AppVersion}
AppVerName=ByteFlow {#AppVersion}
AppPublisher=ByteFlow
AppCopyright=ByteFlow
VersionInfoVersion={#AppVersion}
VersionInfoProductName=ByteFlow Desktop
VersionInfoDescription=ByteFlow Desktop Setup
DefaultDirName={autopf}\ByteFlow
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
AppMutex=Local\ByteFlowDesktop.Instance
CloseApplications=yes
WizardStyle=modern dynamic
SetupIconFile={#IconFile}
UninstallDisplayIcon={app}\ByteFlow.exe,0
UninstallDisplayName=ByteFlow
Compression=lzma2/max
SolidCompression=yes
OutputBaseFilename=ByteFlow-Setup-{#AppVersion}-windows-x64

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#DistDir}\byteflow-{#AppVersion}-windows-x64.exe"; DestDir: "{app}"; DestName: "ByteFlow.exe"; Flags: ignoreversion
Source: "{#DistDir}\byteflow-crypto-{#AppVersion}-windows-x64.exe"; DestDir: "{app}"; DestName: "byteflow-crypto.exe"; Flags: ignoreversion
Source: "{#FontsDir}\LICENSES.md"; DestDir: "{app}\licenses"; DestName: "FONTS.md"; Flags: ignoreversion
Source: "{#FontsDir}\OFL.txt"; DestDir: "{app}\licenses"; DestName: "OFL-PlusJakartaSans.txt"; Flags: ignoreversion
Source: "{#FontsDir}\OFL-NotoSansArabic.txt"; DestDir: "{app}\licenses"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\ByteFlow"; Filename: "{app}\ByteFlow.exe"; Comment: "Your Cloudflare R2 file vault"
Name: "{autodesktop}\ByteFlow"; Filename: "{app}\ByteFlow.exe"; Comment: "Your Cloudflare R2 file vault"; Tasks: desktopicon

[Run]
Filename: "{app}\ByteFlow.exe"; Description: "{cm:LaunchProgram,ByteFlow}"; Flags: nowait postinstall skipifsilent
