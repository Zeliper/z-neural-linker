# 배포와 설치

## 파일 형식
프로젝트 파일 확장자는 `.nlproj` (내용은 JSON, `ProjectFile`). 앱은 `.json` 도 계속 연다.
`nl-app <파일>` 로 열 수 있어 OS 파일 연결이 그대로 동작한다. `nl-app --version` 은 버전만 찍는다.

## Linux
```
cargo build --release
packaging/linux/install.sh          # ~/.local/bin/nl-app + .desktop + MIME(.nlproj) + 아이콘
packaging/linux/install.sh --uninstall
```
시스템 전역 설치는 같은 파일을 `/usr/local/bin`, `/usr/share/applications`, `/usr/share/mime/packages`,
`/usr/share/icons/hicolor/scalable/apps` 에 두고 `update-mime-database /usr/share/mime` 를 실행하면 된다.

## Windows
Inno Setup 6 로 `packaging/windows/neural-linker.iss` 를 컴파일한다 (`iscc /DAppVersion=0.2.0 neural-linker.iss`).
사용자 단위(`%LOCALAPPDATA%\Programs\Neural Linker`, UAC 없음)로 설치하며 `.nlproj` 연결(`NeuralLinker.Project`)을 HKCU 에 등록하고, 설치·업데이트 뒤 앱을 다시 띄운다.

## 업데이트
앱은 시작할 때(설정으로 끌 수 있음) `latest.json` 을 받아 현재 버전과 비교한다.
- 주소 기본값: `https://updates.trustanc.dev/neural-linker/latest.json`. 앱의 🌐 창에서 바꾸거나 `NL_UPDATE_URL` 환경 변수로 지정.
- 매니페스트: `packaging/make-manifest.sh <버전> <기본 URL> <자산...>` 로 만든다. 자산은 `linux-x86_64`(단일 실행 파일, kind=binary),
  `windows-x86_64`(설치 프로그램, kind=installer) 이고 sha256 으로 검증한다.
- 적용: Windows 는 설치 프로그램을 `/SILENT /NORESTART /CLOSEAPPLICATIONS` 로 실행하고 앱을 끝낸다(설치 프로그램이 다시 띄움).
  Linux 는 현재 실행 파일을 rename 으로 바꿔치기하고 새로 띄운다(시스템 경로면 실패 메시지 — 설치 스크립트로 갱신).
- 자동 설치를 켜면 새 버전을 알아서 내려받고, 저장할 것이 없으면 바로 적용한다. 저장할 것이 있으면 확인 모달이 먼저 뜬다.

릴리스 절차: 워크스페이스 `version` 올리기 → 양쪽 빌드 → 매니페스트 생성 → 자산과 `latest.json` 을 배포 서버에 올리기.
