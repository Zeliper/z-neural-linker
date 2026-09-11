; @APP_NAME@ @APP_VERSION@ — Neural Linker 가 만든 Inno Setup 6 스크립트.
; 컴파일: iscc @APP_SLUG@.iss     (Linux 면 wine ".../Inno Setup 6/ISCC.exe" @APP_SLUG@.iss)
; 산출물: Output\@APP_SLUG@-setup-@APP_VERSION@.exe

[Setup]
; AppId 는 앱 이름에서 뽑은 UUID v5 라 같은 이름이면 언제나 같다 — 새 버전이 덮어쓰기로 설치된다.
AppId={{@APP_ID@}
AppName=@APP_NAME@
AppVersion=@APP_VERSION@
AppPublisher=@PUBLISHER@
; 사용자 단위 설치: UAC 없이 조용한 업데이트가 가능하고, 재실행된 앱이 관리자 계정으로 뜨지 않는다.
DefaultDirName={localappdata}\Programs\@APP_DIR@
PrivilegesRequired=lowest
DefaultGroupName=@APP_NAME@
OutputDir=Output
OutputBaseFilename=@APP_SLUG@-setup-@APP_VERSION@
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes
RestartApplications=no
UninstallDisplayIcon={app}\@APP_SLUG@.exe
WizardStyle=modern
@SETUP_ICON@

[Languages]
Name: "korean"; MessagesFile: "compiler:Languages\Korean.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "@APP_SLUG@.exe"; DestDir: "{app}"; Flags: ignoreversion
@ICON_FILE@

[Icons]
Name: "{group}\@APP_NAME@"; Filename: "{app}\@APP_SLUG@.exe"@ICON_REF@
Name: "{autodesktop}\@APP_NAME@"; Filename: "{app}\@APP_SLUG@.exe"@ICON_REF@; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "바탕 화면 아이콘 만들기"; GroupDescription: "추가 작업:"

[Run]
; 설치(및 조용한 업데이트) 뒤 앱을 다시 띄운다.
Filename: "{app}\@APP_SLUG@.exe"; Description: "@APP_NAME@ 실행"; Flags: nowait postinstall runasoriginaluser
