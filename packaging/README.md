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

업데이트는 두 갈래다. **빌더**(`nl-app`)는 Trust A&C 가 배포하고, **배포 앱**(`nl-runtime` + 첨부 번들)은
빌더 사용자가 자기 사용자에게 배포한다. 둘 다 같은 코어(`crates/nl-update`)와 같은 매니페스트 형식을 쓴다.

### 매니페스트 (`latest.json`)

```json
{
  "version": "0.2.0",
  "notes": "고친 것",
  "assets": {
    "linux-x86_64":   { "url": "…/app-0.2.0-linux-x86_64.tar.gz", "sha256": "…", "kind": "binary",    "size": 1234 },
    "windows-x86_64": { "url": "…/app-setup-0.2.0.exe",           "sha256": "…", "kind": "installer", "size": 5678 }
  }
}
```

대상 키는 앱이 `nl_update::target_key()`(= `<os>-<arch>`)로 만드는 값과 같아야 한다. 자산 종류가 적용 방법을 정한다.

- **빌더용**: `packaging/make-manifest.sh <버전> <자산 기본 URL> <자산 파일...>` — 파일 이름으로 대상과 종류를 정한다.
- **배포 앱용**: 빌더의 빌드가 `nl_bundle::write_manifest(version, notes, artifacts, base_url, out_dir)` 를 불러
  산출물에서 바로 만든다. 셸 스크립트와 결과가 같다.

### 서명 (minisign, 선택)

매니페스트가 바꿔치기되면 앱이 엉뚱한 파일을 내려받는다. detached 서명으로 막는다.

```
minisign -G -p minisign.pub -s minisign.key      # 키 쌍 한 번 만들기
MINISIGN_KEY=~/.minisign/nl.key ./make-manifest.sh 0.2.0 <URL> <자산...>
# 또는 직접:  minisign -Sm latest.json -s ~/.minisign/nl.key
```

`latest.json` 옆에 `latest.json.minisig` 를 같이 올린다. 앱은 공개키가 있을 때만 서명을 받아 검증하고,
검증에 실패하면 매니페스트를 **읽지도 않는다**. 공개키가 없으면 검증을 건너뛰고 경고 로그만 남긴다.

공개키는 `minisign.pub` 의 **둘째 줄**(`RWQ…` 로 시작하는 base64 한 줄)이다. 넣는 자리는 갈래마다 다르다.

| 갈래 | 공개키 자리 | 매니페스트 주소 |
| --- | --- | --- |
| 빌더 `nl-app` | `crates/nl-app/src/update_key.rs` 의 `PUBLIC_KEY` (지금은 `None`) | 같은 파일의 `UPDATE_URL`, `NL_UPDATE_URL` 이 우선 |
| 배포 앱 `nl-runtime` | 번들 매니페스트의 `update_public_key` | 번들 매니페스트의 `update_url`, `NL_UPDATE_URL` 이 우선 |

배포 앱 쪽을 번들에 담는 이유는 앱마다 배포 주체가 달라서다 — 빌더 사용자가 자기 키로 서명한다.

### 배포 앱의 동작

번들 매니페스트(`BundleManifest`)의 세 필드가 정한다.

- `update_url`: 없으면 자동 업데이트를 쓰지 않는다.
- `update_public_key`: 없으면 서명 검증을 건너뛴다.
- `auto_update`: 켜면 **파이프라인이 멈춰 있을 때** 새 버전을 미리 내려받는다. 적용은 언제나 사용자 확인을 거친다.

시작할 때 한 번 확인하고, 새 버전이 있으면 상단 바에 `⬆ 새 버전 x.y.z` 배지가 뜬다. 누르면 릴리스 노트와
진행률, "지금 적용" 버튼이 있는 창이 열린다. `--no-update` 로 확인 자체를 끌 수 있고, `--headless` 는
확인 결과를 로그로만 알린다(서버형 배포를 사람 확인 없이 바꿔치우지 않는다).

비교 기준은 번들 매니페스트의 `app_version` 이다. semver 가 아니면 업데이트를 끈다 — 비교할 수 없으면
무엇이든 새 버전으로 보이기 때문이다.

### 적용

- **Windows**(`kind: installer`): 설치 프로그램을 `/SILENT /NORESTART /CLOSEAPPLICATIONS` 로 실행하고 앱을 끝낸다.
  설치가 끝나면 설치 프로그램이 앱을 다시 띄운다.
- **Linux**(`kind: binary`): 현재 실행 파일을 같은 파일 시스템의 임시 이름으로 복사한 뒤 rename 으로 바꿔치기하고
  1초 뒤 새 프로세스를 띄운다. 배포 앱은 번들이 첨부된 실행 파일 자체가 자산이라 통째로 바뀐다.
  설치 경로가 시스템 영역(`/usr`, `/opt` …)이면 쓸 수 없다는 메시지를 내므로 `install.sh` 나 패키지 관리자로 갱신한다.

내려받은 자산은 sha256 으로 검증한 뒤에야 최종 이름을 얻는다. 받는 중에는 `.part` 확장자라 중간에 끊긴 파일이
완성본으로 오인되지 않는다.

릴리스 절차: 워크스페이스 `version` 올리기 → 양쪽 빌드 → 매니페스트 생성(+ 서명) → 자산과 `latest.json`
(+ `latest.json.minisig`) 을 배포 서버에 올리기.
