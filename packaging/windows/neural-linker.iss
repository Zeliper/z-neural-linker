; Neural Linker — Inno Setup 스크립트 (Windows 설치 프로그램).
; 빌드: iscc /DAppVersion=0.2.0 neural-linker.iss   (iscc = Inno Setup 6 컴파일러)
; 산출물: Output\neural-linker-setup-<버전>.exe — 업데이트 매니페스트의 windows-x86_64 자산(kind=installer).
; 앱의 자동 업데이트는 이 파일을 /SILENT /NORESTART /CLOSEAPPLICATIONS 로 실행하고 종료한다.

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif
#define AppName "Neural Linker"
#define AppExe "nl-app.exe"

[Setup]
AppId={{7B1E2D44-5A6C-4F0B-9E31-2D8C0F5A7E19}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Trust A&C
; 사용자 단위 설치: UAC 없이 조용한 업데이트가 가능하고, 재실행된 앱이 관리자 계정으로 뜨지 않는다.
DefaultDirName={localappdata}\Programs\Neural Linker
PrivilegesRequired=lowest
DefaultGroupName={#AppName}
OutputDir=Output
OutputBaseFilename=neural-linker-setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
ChangesAssociations=yes
CloseApplications=yes
RestartApplications=no
UninstallDisplayIcon={app}\{#AppExe}
WizardStyle=modern

[Languages]
Name: "korean"; MessagesFile: "compiler:Languages\Korean.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
; cargo build --release --target x86_64-pc-windows-msvc 산출물을 옆에 두고 컴파일한다.
Source: "..\..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "바탕 화면 아이콘 만들기"; GroupDescription: "추가 작업:"

[Registry]
; .nlproj 파일 연결 — 더블클릭하면 nl-app.exe "<파일>" 로 열린다.
Root: HKCU; Subkey: "Software\Classes\.nlproj"; ValueType: string; ValueName: ""; ValueData: "NeuralLinker.Project"; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\.nlproj"; ValueType: string; ValueName: "Content Type"; ValueData: "application/x-neural-linker"; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\NeuralLinker.Project"; ValueType: string; ValueName: ""; ValueData: "Neural Linker 프로젝트"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\NeuralLinker.Project\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#AppExe},0"
Root: HKCU; Subkey: "Software\Classes\NeuralLinker.Project\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#AppExe}"" ""%1"""
Root: HKCU; Subkey: "Software\Classes\Applications\{#AppExe}\SupportedTypes"; ValueType: string; ValueName: ".nlproj"; ValueData: ""

[Run]
; 설치(및 자동 업데이트) 뒤 앱을 다시 띄운다. 조용한 설치에서도 실행된다 — 업데이트 흐름이 기대하는 동작.
Filename: "{app}\{#AppExe}"; Description: "{#AppName} 실행"; Flags: nowait postinstall runasoriginaluser
