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

### 콘솔 출력
두 실행 파일 모두 릴리스 빌드에서 `windows_subsystem = "windows"` 다 — 창만 뜨고 검은 콘솔이 같이 뜨지 않는다.
그 대가로 GUI 서브시스템 프로세스는 표준 출력이 갈 곳이 없어, 명령 프롬프트에서 `nl-runtime.exe --version` 을
쳐도 아무것도 보이지 않는다.

`nl-runtime` 은 `--version`·`--help`·`--headless`·인자 오류·파일을 인자로 준 실행처럼 **터미널에서 부른 것이
분명한 경로에서만** `AttachConsole(ATTACH_PARENT_PROCESS)` 로 부모 콘솔에 붙는다(`crates/nl-runtime/src/console.rs`).
- 이미 있는 콘솔에 붙기만 하고 **새로 만들지 않는다** — 탐색기에서 더블클릭하면 조용히 지나간다.
- 붙은 뒤 표준 핸들이 비어 있으면 `CONOUT$`/`CONIN$` 를 열어 꽂는다. 이미 리다이렉트돼 있으면(`> out.txt`)
  건드리지 않아 파이프와 리다이렉트가 그대로 동작한다.
- 창을 띄우는 경로에서는 부르지 않는다.

`nl-app` 은 아직 이 처리를 하지 않는다.

## Windows 산출물을 Linux 에서 만들기

배포 앱의 Windows 산출물은 `nl-runtime.exe` 에 번들을 붙여 만든다. **빌더(`nl-app.exe`)와 런타임 둘 다**
Linux 에서 관리자 권한 없이 크로스 빌드할 수 있다 (2026-09-11 이 저장소에서 실측).

```sh
cargo install cargo-xwin --locked
rustup target add x86_64-pc-windows-msvc
cargo xwin build --release -p nl-runtime --target x86_64-pc-windows-msvc
cargo xwin build --release -p nl-app     --target x86_64-pc-windows-msvc
# → target/x86_64-pc-windows-msvc/release/{nl-runtime.exe, nl-app.exe}
mkdir -p runtimes/x86_64-pc-windows-msvc
cp target/x86_64-pc-windows-msvc/release/nl-runtime.exe runtimes/x86_64-pc-windows-msvc/
```

`cargo-xwin` 이 Microsoft 의 Windows SDK·CRT 를 `~/.cache/cargo-xwin` 에 내려받는다. Visual Studio 는 필요 없다.

| 실측값 | `nl-runtime.exe` | `nl-app.exe` |
| --- | --- | --- |
| 크기 | 53.7 MB | 61.3 MB |
| 형식 | `PE32+ ... (GUI), x86-64` | 같음 |
| 빌드 | 처음 15분 (SDK 1.2 GB 포함) | 의존 캐시가 있으면 3분 30초 |
| wine `--version` | `nl-runtime 0.1.0` | `neural-linker 0.1.0` |
| wine 창 | 해당 없음(헤드리스) | **안 뜸** — 아래 참고 |

디스크는 둘 합쳐 약 3 GB (SDK 캐시 1.2 GB + `target/x86_64-pc-windows-msvc` 1.9 GB).
`rfd`·`winit`·`glow`·`directories`·`open` 을 포함해 컴파일에 손댈 곳은 없었다.

번들 첨부까지 종단 확인했다: 크로스 빌드한 exe 에 번들을 붙이고 다시 읽은 뒤
wine 에서 `--headless --run-for 1` 로 파이프라인이 돌고 멈추는 것까지 봤다.

### wine 에서 창이 뜨지 않는 것
`nl-app.exe` 는 wine 에서 실행되고 winit 초기화까지 가지만 OpenGL 컨텍스트를 만들지 못하고 끝난다.

```
Found no glutin configs matching the template: ... os error 14007
```

wine 의 WGL 이 호스트 GL 을 찾지 못해서다. 헤드리스 sway 안(Xwayland 꺼짐)이라 `winewayland.drv` 만 쓸 수
있고, `LIBGL_ALWAYS_SOFTWARE=1`·`GALLIUM_DRIVER=llvmpipe` 로도 달라지지 않았다. **바이너리 문제가 아니다** —
같은 하네스에서 Linux 네이티브 `nl-app` 은 정상으로 그려진다. 실제 Windows 나 Xwayland 가 있는 wine 에서
다시 확인해야 한다.

### 준비물

`clang` 과 `llvm-lib` 는 clang 패키지에 들어 있지만 **`lld-link` 는 별도 패키지**다.

- 관리자 권한이 있으면: Fedora `sudo dnf install lld`, Debian/Ubuntu `sudo apt install lld`.
- 없으면 rustup 이 들고 있는 `rust-lld` 로 대신한다 — 같은 LLVM 에서 나온 같은 링커다.
  `nl_bundle::tools::ensure_lld_link` 가 자동으로 만들어 주고, 손으로 하려면:

```sh
RL=$(ls ~/.rustup/toolchains/*/lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld | head -1)
mkdir -p ~/.local/bin
cat > ~/.local/bin/lld-link <<EOF
#!/bin/sh
case "\$1" in
  -flavor) exec $RL "\$@" ;;
  *)       exec $RL -flavor link "\$@" ;;
esac
EOF
chmod +x ~/.local/bin/lld-link
```

`case` 로 가르는 이유는 rustc 가 링커 이름을 보고 **스스로 `-flavor link` 를 붙여 보낼 때가 있어서**다.
그때 또 붙이면 `rust-lld` 가 두 번째 `link` 를 입력 파일로 읽고
`could not open 'link': No such file or directory` 로 죽는다.

### 빌더에서

`nl_bundle::tools::cross_build_plan(Target::WindowsX64)` 가 위 절차를 `ToolPlan` 으로 돌려준다
(명령 목록 + 걸리는 시간·디스크 안내). 사용자가 승인하면 `run_cross_build` 가 순서대로 실행하며
출력을 `ToolProgress::Output` 으로 한 줄씩 흘린다. 거부하면 Windows 산출물 없이 Linux 것만 만든다.

Windows 빌드 기계가 따로 있거나 이 경로가 막히면 GitHub Actions 의 `windows-latest` 러너에서 빌드해
`runtimes/` 에 내려받는 방법도 있다 — 다만 지금은 크로스 빌드가 되므로 필요하지 않다.

### Inno Setup 설치 프로그램 만들기 (일부 미확인)
`nl_bundle::windows_installer()` 가 만드는 `.iss` 를 실제로 컴파일하려면 Inno Setup 6 이 필요하다.
`install_inno_setup_plan()` 이 그 설치본을 받아 오는 계획을 돌려준다.

주소와 해시는 **버전이 박혀 있다**. 2026-09-14 에 실제로 내려받아 확인한 값이다.

| 항목 | 값 |
| --- | --- |
| 버전 | 6.7.3 |
| 주소 | `https://github.com/jrsoftware/issrc/releases/download/is-6_7_3/innosetup-6.7.3.exe` |
| sha256 | `9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732` |
| 크기 | 10,592,232 바이트 |

예전에 쓰던 `https://jrsoftware.org/download.php/is.exe` 는 설치본이 아니라 **내려받기 안내 HTML 페이지로
302** 하는 주소였고, 버전이 계속 바뀌어 해시를 박을 수도 없었다. jrsoftware 의 실제 배포처는 GitHub
릴리스이고 `isdl.php` 가 거기로 보낸다. 버전을 올릴 때는 `crates/nl-bundle/src/tools.rs` 의
`INNO_VERSION`·`INNO_TAG`·`INNO_FILE`·`INNO_SHA256`·`INNO_SIZE` 를 함께 고친다
(`curl -sL <주소> | sha256sum`).

`run_tool_plan` 은 **알려진 sha256 이 없는 계획의 실행 단계를 거부한다.** 내려받은 파일은 그대로 남겨
사용자가 발행처에서 해시를 확인한 뒤 직접 실행할 수 있게 하고, 오류로 끝난다. 검증하지 않은 실행 파일을
조용히 돌리지 않기 위해서다. `ToolPlan::verified` 가 그 상태를 요약하므로 승인 모달이 그대로 그리면 된다.

내려받은 도구는 `~/.cache/neural-linker/tools`(0700)에 놓인다. 이 폴더는 `lld-link` 대체 스크립트가
놓이는 자리이자 크로스 빌드 내내 `PATH` 맨 앞에 오는 자리라, 공용 `/tmp` 로 떨어지지 않게
`ProjectDirs` → `$XDG_RUNTIME_DIR` → `$HOME/.cache` 순으로만 물러선다.

아직 확인하지 못한 것(이 개발 환경에 Inno Setup 이 없다):

1. `run_tool_plan` 이 실제로 설치까지 마치는지 (Linux 는 `wine innosetup-6.exe /VERYSILENT`).
2. `find_inno_setup()` 이 wine 접두사 안의 `ISCC.exe` 를 찾는지.
3. `windows_installer()` 가 실제 `setup.exe` 를 만들고, 그것을 `/VERYSILENT /NORESTART` 로 돌리면
   `%LOCALAPPDATA%\Programs\<이름>\` 에 exe 가 놓이는지.

## 업데이트

업데이트는 두 갈래다. **빌더**(`nl-app`)는 Trust A&C 가 배포하고, **배포 앱**(`nl-runtime` + 첨부 번들)은
빌더 사용자가 자기 사용자에게 배포한다. 둘 다 같은 코어(`crates/nl-update`)와 같은 매니페스트 형식을 쓴다.

### 매니페스트 (`latest.json`)

```json
{
  "version": "0.2.0",
  "notes": "고친 것",
  "published_at": "2026-09-14T02:09:05Z",
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

받는 쪽이 거절하는 조건이 넷 있다. 매니페스트를 손으로 만든다면 전부 지켜야 한다.

| 조건 | 이유 |
| --- | --- |
| 주소가 `https://` | 평문으로 받으면 중간자가 매니페스트와 서명을 함께 갈아치운다. 리다이렉트로도 http 로 내려가지 못한다 |
| `published_at` 이 있고 30일 안 | 정품 서명이 붙은 **옛** 매니페스트를 다시 들려주는 재생 공격을 막는다 |
| 자산이 매니페스트와 같은 오리진 | 자산 주소로 내부망을 훑지 못하게 한다. 다른 호스트를 쓰려면 `allowed_asset_hosts` 에 적는다 |
| 모든 자산에 `sha256` | 해시가 없으면 내려받은 것을 믿을 근거가 없다 |

두 생성 스크립트(`make-manifest.sh`, `make-runtimes-manifest.py`)는 `published_at` 을 지금 시각으로 채우고
기본 URL 이 https 가 아니면 실패한다. `PUBLISHED_AT` 환경 변수로 시각을 직접 줄 수도 있다.

### 서명 (minisign, **필수**)

매니페스트가 바꿔치기되면 앱이 엉뚱한 파일을 내려받는다. detached 서명으로 막는다.

```
minisign -G -p minisign.pub -s minisign.key      # 키 쌍 한 번 만들기
MINISIGN_KEY=~/.minisign/nl.key ./make-manifest.sh 0.2.0 <URL> <자산...>
# 또는 직접:  minisign -Sm latest.json -s ~/.minisign/nl.key
```

`latest.json` 옆에 `latest.json.minisig` 를 같이 올린다. 앱은 매니페스트를 검증한 **뒤에야** 읽는다.

**공개키가 없으면 업데이트 기능 자체가 꺼진다.** 예전에는 키가 없으면 검증을 건너뛰고 경고만 남겼지만,
그러면 "서명을 붙여 둔 배포에서 키만 지우면 검증이 사라지는" 구멍이 된다. 지금은 키가 없으면
`Updater` 가 `Disabled` 상태로 남고 확인조차 하지 않으며, 배포 앱은 업데이트 UI 를 아예 감춘다.
옛 `Ed` 형식(비-prehashed) 서명도 더는 받지 않는다 — 지금 minisign 이 내는 `ED` 만 받는다.

공개키는 `minisign.pub` 의 **둘째 줄**(`RWQ…` 로 시작하는 base64 한 줄)이다. 넣는 자리는 갈래마다 다르다.

| 갈래 | 공개키 자리 | 매니페스트 주소 |
| --- | --- | --- |
| 빌더 `nl-app` | `crates/nl-app/src/update_key.rs` 의 `PUBLIC_KEY` (지금은 `None` → 업데이트 꺼짐) | 같은 파일의 `UPDATE_URL` |
| 배포 앱 `nl-runtime` | 번들 매니페스트의 `update_public_key` (없으면 업데이트 꺼짐) | 번들 매니페스트의 `update_url` |

배포 앱 쪽을 번들에 담는 이유는 앱마다 배포 주체가 달라서다 — 빌더 사용자가 자기 키로 서명한다.

`NL_UPDATE_URL` 로 주소를 덮어쓰려면 `NL_UPDATE_INSECURE=1` 을 함께 켜야 한다. 환경 변수 하나로 업데이트
출처가 바뀌면 래퍼 스크립트나 조작된 `.desktop` 파일만으로 실행 파일을 갈아치울 수 있기 때문이다.
켜도 서명 검증과 https 요구는 그대로다. 루프백 http 로 시험하려면 `NL_ALLOW_HTTP=1` 을 함께 쓴다
(바깥 주소에는 어떤 경우에도 열리지 않는다).

### 배포 앱의 동작

번들 매니페스트(`BundleManifest`)의 필드가 정한다.

- `update_url`: 없으면 자동 업데이트를 쓰지 않는다. **https 가 아니면 업데이트를 끈다.**
- `update_public_key`: **없으면 업데이트 기능 전체가 꺼진다** — 서명을 검증할 수 없는 업데이트는 받지 않는다.
  `.nlapp` 은 신뢰할 수 없는 입력이라, 번들이 스스로 "검증하지 말라" 고 말하게 두면 악성 번들이
  평문 매니페스트로 실행 파일을 밀어 넣을 수 있다.
- `auto_update`: 켜면 **파이프라인이 멈춰 있을 때** 새 버전을 미리 내려받는다. 적용은 언제나 사용자 확인을 거친다.
- `arm_input`: **기본 꺼짐.** 꺼져 있으면 `Sink::MouseKeyboard` 는 로그만 남긴다. 받은 사람이 모르는 사이
  커서가 움직이는 일이 없도록 빌더에서 명시적으로 켜야 한다 — 빌드 설정의 **입력 무장** 체크박스(기본 꺼짐)
  또는 `nl build --arm-input`. 켜면 앱 상단 바에 `⚠ 입력 무장` 배지가 붙고, 파이프라인을 처음 시작할 때
  로그에 한 번 안내한다: `이 앱은 마우스·키보드를 실제로 조작합니다 (빌드할 때 입력 무장을 켰습니다).`
  배포판에는 빌더의 Esc 킬 스위치가 없으므로, 무장한 앱에는 GUI 정지 버튼을 넣어 두는 편이 좋다.

시작할 때 한 번 확인하고, 새 버전이 있으면 상단 바에 `⬆ 새 버전 x.y.z` 배지가 뜬다. 누르면 릴리스 노트와
진행률, "지금 적용" 버튼이 있는 창이 열린다. `--no-update` 로 확인 자체를 끌 수 있고, `--headless` 는
확인 결과를 로그로만 알린다(서버형 배포를 사람 확인 없이 바꿔치우지 않는다).

비교 기준은 번들 매니페스트의 `app_version` 이다. semver 가 아니면 업데이트를 끈다 — 비교할 수 없으면
무엇이든 새 버전으로 보이기 때문이다.

### 적용

- **Windows**(`kind: installer`): 설치 프로그램을 `/SILENT /NORESTART /CLOSEAPPLICATIONS` 로 실행하고 앱을 끝낸다.
  설치가 끝나면 설치 프로그램이 앱을 다시 띄운다.
- **Linux**(`kind: binary`): 현재 실행 파일을 같은 폴더의 무작위 이름 임시 파일(`O_EXCL`, 0600)로 복사한 뒤
  rename 으로 바꿔치기하고 1초 뒤 새 프로세스를 띄운다. 배포 앱은 번들이 첨부된 실행 파일 자체가 자산이라
  통째로 바뀐다. 설치 경로가 시스템 영역(`/usr`, `/opt` …)이면 쓸 수 없다는 메시지를 내므로
  `install.sh` 나 패키지 관리자로 갱신한다.

내려받은 자산은 sha256 으로 검증한 뒤에야 최종 이름을 얻는다. 받는 중에는 `.part` 확장자라 중간에 끊긴 파일이
완성본으로 오인되지 않는다. 내려받기 폴더는 사용자 캐시(`~/.cache/neural-linker/updates/<앱>`, 0700)다 —
공용 `/tmp` 를 쓰면 경로가 예측 가능해 다른 로컬 사용자가 선점하거나 심볼릭 링크를 걸 수 있다.

**적용 직전에 sha256 을 다시 계산해 맞춰 본다.** 내려받기와 "지금 적용" 사이는 사용자가 버튼을 누를 때까지
몇 시간일 수 있고, 그 사이에 파일이 바뀌면 검증을 통과한 적 없는 바이너리가 실행된다. 다시 계산한 값이
매니페스트의 값과 다르면 적용을 거부한다.

#### 신뢰의 뿌리와 한계

믿음의 사슬은 **번들에 박힌 공개키 → 매니페스트 서명 → 매니페스트의 sha256 → 자산**이다. 자산 자체에는
별도 서명이 없다.

- Windows 설치 프로그램의 **Authenticode 서명을 검증하지 않는다.** 실행 전에 확인하는 것은 매니페스트가
  말한 sha256 뿐이다. 그 sha256 은 서명된 매니페스트에서 왔으므로 사슬은 닫혀 있지만, 배포 서버와 서명
  키를 동시에 쥔 공격자는 막지 못한다. 설치본에 Authenticode 서명을 붙여 두면 Windows 자체의
  SmartScreen 경고가 줄어들므로 붙이기를 권한다.
- 설치 프로그램은 파일 경로로만 실행할 수 있어, 해시를 다시 확인한 시점과 실행 시점 사이에 아주 짧은
  틈이 남는다. 내려받기 폴더가 사용자 전용(0700)이라 이 틈을 노리려면 이미 그 계정을 쥐고 있어야 한다.

릴리스 절차: 워크스페이스 `version` 올리기 → 양쪽 빌드 → 매니페스트 생성(+ 서명) → 자산과 `latest.json`
(+ `latest.json.minisig`) 을 배포 서버에 올리기.
