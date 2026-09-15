# 변경 이력

형식은 [Keep a Changelog](https://keepachangelog.com/ko/1.1.0/)를 따르고 버전은
[유의적 버전](https://semver.org/lang/ko/)을 쓴다.

릴리스 절차는 [`docs/RELEASE.md`](docs/RELEASE.md)에 있다. 태그를 밀기 전에 이 파일의
`[미출시]` 항목을 새 버전으로 옮긴다.

## [미출시] — v0.1.0 준비 중

첫 공개를 향한 M0 전체와 M1 일부다. 2026-09-10 에 시작해 2026-09-14 까지의 작업이다.

### 추가

**핵심 (`nl-core`)**
- 프로젝트 문서 모델, 레이어 그래프, 형상 추론, op 기반 undo/diff, 검증
- 페이로드·데이터셋·파이프라인·GUI 레이아웃 스펙, `.nlproj`/`.nlapp` 번들 포맷
- 샘플 프로젝트를 한 곳에 모은 `sample` 모듈 — 빌더의 "샘플 열기"와 `nl sample` 이 같은 것을 만든다
  (XOR MLP, 사분면 CNN, 추론 API 파이프라인)
- `Transform::Tokenize` — 문자 단위 토크나이저(Embedding 입력용)

**엔진 (`nl-engine`)**
- 순환·어텐션 레이어 `Lstm`·`Gru`·`MultiHeadAttention` (`f085770`).
  `Lstm`/`Gru { hidden, bidirectional, return_sequence }` 는 `[L, D]` 를 받아 `return_sequence` 면 `[L, H]`,
  아니면 마지막 상태 `[H]` 를 낸다(양방향이면 `H` 가 두 배). `MultiHeadAttention { heads, dropout }` 은
  셀프 어텐션으로 `[L, D] → [L, D]` 다(`D % heads == 0`). 셋 다 burn 의 `nn` 모듈 대신 파라미터 텐서와
  게이트 수식으로 직접 구현했다 — 그래프가 런타임에 정해져 `Module` 파생을 쓸 수 없기 때문이다.
  빌더 인스펙터 편집기와 팔레트 분류도 함께 들어왔다 (`91fecd2`)
- burn 0.21 기반 **런타임 정의 그래프** 인터프리터. CPU(ndarray)·GPU(wgpu) 모두 CUDA 설치 없이 돈다
- `probe` 로 실제 동작을 확인해 고르는 `Auto` 장치 선택. UI 스레드용 비차단 짝 `resolve_cached`·`probe_cached`
- 학습 루프(SGD/Adam/AdamW × MSE/CrossEntropy/BCE/MAE), safetensors 체크포인트, 추론 세션
- 데이터 로더: 합성·CSV·이미지 폴더·화면 녹화
- 다입출력 학습, 학습률 스케줄(Step/Cosine/Plateau)·워밍업·조기 종료
- 다출력 페이로드 매핑 `decode_outputs`·`encode_inputs` — 모델이 출력을 여럿 낼 때 페이로드 필드로
  갈라 담는다 (`cd90a6f`)
- 크기와 장치를 보고 고르는 **상주 배치** — 데이터셋을 장치에 올려 두고 에포크마다 셔플만 한다

**입출력 (`nl-io`)**
- 화면 캡처 네 백엔드: wlroots(libwayshot) · xdg-desktop-portal · X11(x11rb) · xcap(Windows·macOS)
- 입력 시뮬레이션(enigo), 자원 조회(sysinfo), 파이프라인 실행기 `Runner`
- WebSocket 소스·싱크 — URL 별 연결 풀·팬아웃·지수 백오프 재접속·`wss`(rustls)
- 인바운드 HTTP 서버 소스와 응답 싱크 — 배포 앱을 그대로 API 로 노출한다. 이진 이미지 본문도 받는다
- 화면 녹화기 `Recorder` — 캡처를 학습용 데이터셋 폴더(`Recorded`)로 바로 저장한다
- 실행 중 입력 무장 토글(`set_armed`), 값 축소판(`ValuePreview`), 틱 통계(`Stats`)

**GUI (`nl-gui`) · 빌더 (`nl-app`) · 배포 실행기 (`nl-runtime`)**
- 빌더 미리보기와 배포 앱이 **같은 렌더러**(`render_layout`)를 공유한다
- 빌더: 모델 캔버스 · 데이터/페이로드 · 학습 대시보드 · 파이프라인 뷰(시험 실행·킬 스위치) ·
  GUI 디자이너(바인딩·미리보기) · 빌드 뷰(도구 상태·동의 모달) · 자원 뷰
- 빌더 자동 저장·복구, 시작 지연 개선(창 12.2초 → 1.6초)
- 배포 실행기: 번들 로드 → GUI 렌더 → 파이프라인 실행. `--headless`·`--run-for`·`--no-update`
- `Binding::ModelOutput` 배선 — 모델 노드의 값과 축소판이 그 모델을 가리키는 위젯에 들어간다

**포장·배포 (`nl-bundle`, `nl-update`, `packaging/`)**
- `.nlapp` zip, 런타임 바이너리 꼬리표 첨부 → **사용자 PC 에 Rust 툴체인이 필요 없다**
- 배포 아카이브(tar.gz/zip), Windows 설치 프로그램(Inno Setup `.iss` 생성·탐색·컴파일), PNG→ICO
- Windows 런타임 **크로스 빌드**(cargo-xwin). 관리자 권한 없이 Linux 에서 만든다 —
  `lld-link` 가 없으면 rustup 의 `rust-lld` 로 대신하는 얇은 스크립트를 만들어 쓴다
- 자동 업데이트 코어 `nl-update`: 매니페스트 확인 · 스트리밍 다운로드 + sha256 · 적용(실행 파일 교체 / 설치 프로그램 실행)
- 서명 키 도구 `nl-keygen`(`keygen`/`sign`/`verify`) — **minisign CLI 가 필요 없다**
- 앱 아이콘 일습: 손으로 쓴 SVG 원본에서 PNG·ICO 를 만들고 실행 파일·설치 프로그램·데스크톱 항목에 연결
- 로컬 릴리스 드라이런 `packaging/release-local.sh` — CI 와 같은 함수(`packaging/lib.sh`)를 쓴다

**서버형 배포**
- **`--work-dir <경로>`**(`NL_WORK_DIR`) — 번들을 고정 폴더에 풀고 그대로 둔다 (`d45abdf`).
  기본은 끝나면 지워지는 임시 폴더다. 같은 번들이면 다시 풀지 않아 재시작이 빠르고,
  `<작업 폴더>/local/` 에 둔 인증서 같은 파일은 번들을 갈아 끼워도 남는다.
  같은 번들인지는 **내용의 지문**으로 본다 — zip 바이트가 아니라 매니페스트·프로젝트·가중치·에셋의
  내용만 해싱하므로 압축 시각 같은 것에 흔들리지 않는다
- **systemd 사용자 유닛 템플릿**과 `install.sh --service` (`e7a5a12`). `Restart=on-failure` 와
  재시작 폭주 가드, 최소 권한 옵션을 켜 둔다. 가이드에 "서버로 운영하기" 절이 생겼다
- **`--env-file <파일>`** — `KEY=VALUE` 줄을 읽어 환경 변수로 넣는다 (`6bef30e`). 토큰을 명령줄에
  적지 않기 위한 것이다. 셸이 아니므로 따옴표를 벗기지도 변수를 치환하지도 않고, 값은 `=` 뒤부터
  줄 끝까지 그대로다. 줄 양끝 공백은 없앤다 — Windows 파일의 `\r` 이 토큰에 섞이면 인증이 조용히
  어긋난다. **이미 있는 환경 변수는 덮어쓰지 않아** 한 번만 다른 값으로 띄울 수 있다.
  systemd 유닛도 `EnvironmentFile=` 대신 이것을 쓴다 — 두 플랫폼이 같은 파일·같은 규칙을 쓰게 했다
- **서버 메모리 안정화** — systemd 유닛에 `Environment=MALLOC_ARENA_MAX=2`. glibc 가 스레드마다
  malloc 아레나를 새로 파 오래 돌리면 RSS 가 분당 55 KiB 안팎으로 계속 늘었다. 2로 묶으니
  분당 0.3 KiB 로 평평해졌다(30분 부하 시험, 워밍업 1분 제외). 누수가 아니라 반환되지 않은 여유
  공간이다. Windows 는 힙이 달라 해당 사항이 없고 스크립트에 그 이유를 적어 두었다
- **Windows 상시 실행** `packaging/windows/install-service.ps1` — 작업 스케줄러 `ONLOGON` 작업을
  만든다(관리자 권한 불필요). 토큰은 작업에 적히지 않는다. 작업 스케줄러에는 환경 변수를 넣어 주는
  기능이 없어 `cmd /c "set TOKEN=… && app.exe"` 가 흔히 쓰이는데, 그러면 토큰이 작업 목록과
  명령줄에 드러난다 — 대신 `--env-file` 이 읽고 스크립트는 그 파일의 ACL 을 현재 사용자로 좁힌다

**명령줄 (`nl-cli`)**
- `nl` 명령 — `inspect`·`devices`·`train`·`infer`·`run`·`record`·`build`·`sample`.
  스크립트·CI 용이라 종료 코드가 계약이다

**보안 · 전송**
- 인바운드 HTTP 서버에 **TLS 옵션** — `Source::HttpServer` 에 인증서를 주면 https 로 연다(`TlsConfig { cert_pem, key_pem }`).
  rustls + ring 이라 시스템 OpenSSL 이 필요 없고, 핸드셰이크 위쪽 HTTP 처리는 평문과 같은 코드다.
  토큰 규칙은 TLS 와 무관하게 그대로다 — TLS 는 도청을 막고 토큰은 호출자를 가린다 (`1e87de1`)
- **`nl tls-cert`** — 자체 서명 인증서를 만드는 하위 명령(rcgen, SAN 지정). `nl run`·`nl build` 는
  `--tls-cert`/`--tls-key` 로 프로젝트 파일을 고치지 않고 붙일 수 있다. 개인키는 0600 으로
  **만들면서** 권한을 준다 — 만든 뒤 조이면 그 사이에 열릴 틈이 있다 (`3bc25a8`)
- 배포판의 인증서 탐색이 두 곳으로 늘었다: 실행기 작업 폴더 → 실행 파일이 있는 폴더.
  둘 다 담장 검사를 지난다. 번들에는 개인키를 담지 않으므로 설치한 기계에 따로 두고 쓴다 (`3bc25a8`)

**시험·CI**
- `egui_kittest` 인프로세스 스냅샷 골든(컴포지터 불필요)
- 헤드리스 sway `tools/uitest` 시나리오 러너 + PPM 골든 비교
- 종단 시험 `NL_E2E=1` — sample → train → infer → build → 배포판 HTTP 추론까지 한 줄로
- GitHub·Forgejo Actions 두 벌(`ci.yml`·`release.yml`), Windows 러너 잡 —
  `nl-gui`·`nl-runtime` 까지 돈다(스냅샷은 어댑터가 없으면 스스로 건너뛴다)
- 전체 검증 러너 `scripts/verify-all.sh` — 포맷·클리피·테스트·릴리스 빌드·종단 시험, `--gui` 면 헤드리스
  시나리오까지. 한 단계가 실패해도 끝까지 돌고 마지막에 실패 목록과 단계별 로그 경로를 낸다 (`ae81f8a`)
- **검증의 포트·폴더 격리** — 종단 시험이 샘플의 고정 포트(XOR 8799, CNN 8800)를 그대로 열지 않고
  프로젝트 복사본을 빈 포트로 옮겨 띄운다. 셸 쪽은 `packaging/lib.sh` 의 `nl_free_port`·
  `nl_rebind_project` 를 쓴다. 산출물 폴더에는 pid 를 붙이고, `release-local.sh` 는 더 이상
  `dist-local/` 을 통째로 지우지 않아 검증 로그가 살아남는다. 규칙과 이유는 새 `scripts/README.md` —
  같은 기계에서 도는 다른 세션의 30분짜리 부하 시험을 포트 충돌로 오해해 끊어 먹은 뒤에 만들었다
- uitest 하네스 안정화 — `[nl-app] input ready` 마커로 키를 받을 수 있는 시점을 기다리고, `key-until` 로
  로그가 나올 때까지 키를 다시 보내며, `UITEST_SHOT_DIR` 로 캡처가 저장소에 떨어지지 않게 한다 (`6c6b6a2`)
- 앱 아이콘 일습 — 손으로 쓴 SVG 에서 PNG·ICO 를 만들어 실행 파일·설치 프로그램·데스크톱 항목에 연결 (`49ccb46`)

### 변경

- 인바운드 HTTP 서버를 tiny_http 에서 **표준 라이브러리만 쓴 자체 서버**(`nl_io::httpd`)로 교체.
  연결 수·헤더 상한과 소켓 타임아웃을 직접 통제하기 위해서다
- 샘플 프로젝트를 `nl-cli` 에서 `nl-core` 로 옮겨 빌더와 CLI 가 같은 것을 만들게 했다
- 릴리스 단계의 실제 내용을 `packaging/lib.sh` 한 곳으로 모아 워크플로와 로컬 스크립트가 같은 함수를 쓴다
- 릴리스 프로필에서 `nl-*` 패키지의 `overflow-checks` 를 켰다(보안 리뷰 권고)
- 전 크레이트 포맷을 정리하고 **CI 의 `cargo fmt --check` 를 강제로 바꿨다**(`continue-on-error` 제거) (`6149b01`)
- 디버그 프로필에서 `nl-engine` 의 `opt-level` 을 2 로 올렸다 — 순환 레이어 테스트가 19배 빨라진다 (`505d431`)

### 확인한 것

- 배포판 설치 한 바퀴: tar.gz 를 임시 HOME 에 풀어 `install.sh` → `nl-app`·`nl-runtime`·`nl` 실행 →
  `.desktop`·MIME 설치 확인 → `--uninstall` 로 잔여물 없이 제거
- systemd 사용자 서비스로 헤드리스 앱 기동 → `curl` 추론 200 → 재시작 뒤에도 응답 → `SIGTERM` 정상 종료
- `--work-dir` 재사용·갱신: 같은 번들이면 "이미 풀려 있어 그대로 씁니다", 다른 번들이면 다시 풀고
  `local/` 은 보존
- `local/server.crt` 로 https 기동 후 `curl --cacert` 로 200, 평문 호출은 거부
- **Windows 런타임을 wine 에서 헤드리스로 띄워 HTTP 추론 200 확인** — 자체 HTTP 서버(`httpd.rs`)가
  Windows 대상에서도 도는 것을 처음 확인했다. wgpu 의 DXGI 경고는 나지만 CPU 경로는 영향이 없다
- Inno Setup 6.7.3 실컴파일·실설치 (wine)
- **빌더 자동 업데이트 한 바퀴** — 루프백 서버가 광고한 새 버전을 배지로 알리고, 내려받고,
  적용해 실행 파일이 매니페스트의 sha256 으로 바뀌고 앱이 다시 뜨는 것까지. 지금까지
  `nl-update` 통합 시험 22건으로만 확인돼 있던 경로다
- 공개키를 비운 빌드는 배지도 없고 업데이트 서버에 **요청을 하나도 보내지 않는다**
- 릴리스 체크리스트 ①~⑩ 을 끝까지 수행 — [`docs/release-dryrun-2026-09-14.md`](docs/release-dryrun-2026-09-14.md)

### 보안

2026-09-14 보안 리뷰(높음 11 · 중간 22 · 낮음 25)와 엔진 정확성 리뷰(버그 7 · 의심 9)를 받아 반영했다.
항목별 처리 현황은 [`docs/reviews/security-2026-09-14.md`](docs/reviews/security-2026-09-14.md) 맨 위 표에 있다.

- **배포 앱의 마우스·키보드 조작을 opt-in 으로 바꿨다**(`BundleManifest.arm_input`, 기본 꺼짐).
  꺼져 있으면 로그만 남는다. 받은 사람이 모르는 사이 커서가 움직이지 않게 한 것이다
- **인바운드 HTTP 서버에 인증을 넣었다** — 상수 시간 토큰 비교, `Origin` 있는 요청 403,
  `Host` 불일치 400. 토큰 없는 서버는 루프백에서만 열린다
- **자동 업데이트를 fail-closed 로 바꿨다.** 서명 공개키가 없으면 기능 자체가 켜지지 않는다.
  https 강제, 매니페스트 신선도(`published_at`) 검사, 자산 동일 오리진 제한,
  적용 직전 sha256 재검증
- 파일 소스·싱크와 모델 가중치 경로를 프로젝트 폴더 안으로 묶었다(`resolve_inside`)
- 번들 읽기에 zip bomb 상한 셋(엔트리 수·엔트리 크기·누적 해제량)
- 빌드 템플릿을 문맥별로 이스케이프했다(셸 단일 인용 · Desktop Entry · Inno 스크립트)
- 외부 도구는 고정 해시로만 받고, 해시가 없는 계획은 내려받기만 하고 실행을 거부한다
- 임시 폴더·다운로드 경로를 사용자 전용(0700)으로 옮기고 심볼릭 링크 추종을 막았다
- 수신 큐와 스트림 채널에 상한을 걸었다(무제한 큐는 상대가 보내는 만큼 메모리를 먹는다)

### 수정

- 상주 배치에서 `detach` 를 빠뜨려 역전파가 데이터셋 전체 버퍼를 다루던 것(실측 252초 → 129초)
- 다출력 모델에서 손실을 첫 Output 으로만 계산하도록 바로잡음
- 빌더 시작 시 장치 열거·probe 가 UI 를 막던 것을 백그라운드로
- 텍스트 편집 중 단축키가 유실되던 것
- Inno Setup 실컴파일로 드러난 결함 둘 — `[Icons]` 의 따옴표 거부, 경로가 되는 자리의 정화 누락
  (고치기 전에는 컴파일은 통과하고 **설치할 때** 실패했다)
- `install.sh` 가 CLI(`nl`)를 설치·제거하지 않던 것, `.desktop` 내용이 다른 프로젝트 것이던 것
- `:0` 으로 바인드할 때 `Host` 검사가 400 을 내던 것
- 헤드리스 스모크 테스트가 무한 대기하던 것(`--run-for`)
- 업데이트를 적용한 뒤 옛 빌더가 끝나지 않아 **창이 둘 남던 것** — `nl_update::apply` 의 계약대로
  `Applied` 를 받으면 앱을 닫는다
- CI 릴리스 아카이브에 명령줄 도구 `nl` 이 빠져 있던 것 — `install.sh` 와 사용자 가이드는 `nl` 이
  배포본에 있다고 보고 쓰는데 워크플로가 `nl-cli` 를 빌드하지 않았다
- 매니페스트 생성 도구가 루프백 http 를 거부해 업데이트 경로를 로컬에서 시험할 수 없던 것 —
  받는 쪽과 같은 조건(`NL_ALLOW_HTTP=1` + 루프백)으로 맞췄다. 바깥 주소는 여전히 거절한다
- `scripts/verify-all.sh` 가 `nl-app` 을 빌드하지 않아 GUI 시나리오가 **낡은 바이너리를 시험하고도
  통과로 보고하던 것**

## 알려진 제한

첫 릴리스 시점에 남아 있는 것들이다. 고칠 계획이 있는 것과 환경 탓인 것을 함께 적는다.

| 항목 | 내용 |
| --- | --- |
| Authenticode 미검증 | Windows 설치본의 코드 서명을 확인하지 않는다. 신뢰의 뿌리는 서명된 매니페스트의 sha256 하나다 |
| `ModelOutput` 의 `field` | 아직 쓰지 않는다. 같은 모델을 가리키는 위젯 여럿은 모두 같은 값을 받는다. 모델이 출력을 여럿 낼 때 갈라 보낸다 |
| KDE·GNOME 화면 캡처 속도 | xdg-desktop-portal Screenshot 경로는 **초당 1~3장**이다. 고 fps 가 필요하면 pipewire ScreenCast 가 필요한데 아직 없다 |
| NVK 드라이버 GPU | 오픈소스 NVK(nouveau) 드라이버의 NVIDIA GPU 는 wgpu 컴퓨트가 죽는다. `Auto` 가 `probe` 로 걸러 다른 장치를 고른다. 공식 드라이버를 깔면 잡힌다 |
| Windows 자동 업데이트 | `latest.json` 에 Windows 자산을 아직 넣지 않는다. 자기 자신을 바꿔칠 수 없어 설치 프로그램이 필요한데 CI 러너에 Inno Setup 이 없다. 그때까지 Windows 사용자는 zip 을 받아 덮어쓴다 |
| 서명 키 미발급 | 공개키가 아직 코드에 박혀 있지 않아 자동 업데이트가 꺼진 상태다. 발급 절차는 `docs/RELEASE.md` |
| Windows 상시 실행 스크립트 미검증 | `install-service.ps1` 을 Windows 실기에서 돌려 보지 못했다. 개발 기계가 리눅스이고 wine 으로는 작업 스케줄러를 흉내 낼 수 없다. PowerShell 7.6 파서로 구문 검사와 명령 문자열 조립까지만 확인했다. 처음 쓸 때 `schtasks /Run` 으로 띄워 `app.log` 를 보라 |
| Windows 개인키 권한 | `nl tls-cert` 가 만드는 개인키에 Windows 에서는 권한 비트를 걸지 않는다. 유닉스 권한이 없어 ACL 을 건드리는 대신 "파일이 사용자 폴더 안에 있다" 는 전제에 기댄다. 여러 사람이 쓰는 Windows 기계라면 ACL 을 직접 조이라 |
| 순환 레이어 성능 | `Lstm`·`Gru` 는 시퀀스 길이에 **선형인 커널 호출**을 낸다. 시점마다 게이트를 한 번씩 계산하므로 길이가 수백이면 눈에 띄게 느리다. 짧은 시퀀스를 먼저 써 보라 |
| 미처리 낮음 2건 | unmaintained 의존성 3건(L23)과 `Cargo.lock` 의 도달 불가 항목(L25). 둘 다 상위 크레이트가 버전을 고정한 것이라 손댈 수 없다 |
