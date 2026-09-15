# Windows 실기 검증 지시서

이 프로젝트는 **Linux 에서만 개발·검증됐다.** Windows 는 크로스 컴파일(`cargo xwin`)로
"빌드는 된다" 까지만 확인돼 있고, 실제 Windows 기계에서 띄워 본 사람이 아직 없다.
이 문서는 그 공백을 메우는 사람(또는 에이전트)을 위한 것이다.

배경과 현재 상태는 [`HANDOFF.md`](HANDOFF.md) 를 먼저 읽으라. 설계는
[`ARCHITECTURE.md`](ARCHITECTURE.md), 사용자 관점은 [`GUIDE.md`](GUIDE.md) 에 있다.

## 지금 무엇이 확인돼 있고 무엇이 아닌가

| | Linux | Windows |
| --- | --- | --- |
| 크레이트 8개 (`nl-core`·`nl-io`·`nl-cli`·`nl-bundle`·`nl-update`·`nl-engine`·`nl-gui`·`nl-runtime`) | 통과 | **CI 가 `windows-latest` 에서 돌린다** |
| `nl-app` (빌더 GUI) 테스트 | 통과 | **미지수** — CI Windows 잡에서 제외돼 있다 |
| `nl-app` 실행 | 통과 | **한 번도 안 해 봤다** |
| CLI 한 바퀴 (sample→train→infer→build→배포판 HTTP) | 통과 | **한 번도 안 해 봤다** |
| 설치 프로그램 생성·설치 | wine 으로만 | **실기 미확인** |
| 서비스 등록 (`install-service.ps1`) | 해당 없음 | **미확인** |
| 화면 캡처 | 확인(포털 경로, 2.8fps) | **미확인** — Windows 는 `xcap` 을 쓴다 |

## 준비물

- [rustup](https://rustup.rs/) — 기본 `x86_64-pc-windows-msvc` 툴체인
- Visual Studio Build Tools (MSVC 링커). rustup 설치 과정에서 안내한다
- Git
- (5번을 하려면) [Inno Setup 6](https://jrsoftware.org/isdl.php)
- GPU 는 드라이버만 있으면 된다. CUDA 는 필요 없다 (wgpu → DX12/Vulkan)

## 작업 방식

- 새 브랜치 `agent/windows-check` 에서 작업한다.
- 결과는 `docs/reviews/windows-<날짜>.md` **한 파일**에 쓴다. 기존 리뷰 문서
  (`docs/reviews/*.md`)가 형식의 본보기다.
- **돌려 보지 않은 것을 통과로 적지 마라.** 항목마다 실제 명령과 그 출력을 붙인다.
  안 되면 안 된 대로 에러 메시지를 그대로 적는다. **그게 이 작업의 목적이다** —
  "다 됩니다" 라는 보고서는 아무 값어치가 없다.
- 코드를 고쳐야 하면 고치되, 무엇을 왜 고쳤는지 보고서에 남긴다.
- 끝나면 커밋하고 `origin` 에 push 한다.

Linux 쪽 검증 러너(`scripts/verify-all.sh`)와 GUI 하네스(`tools/uitest`)는 **Windows 에서
돌지 않는다** — sway·wtype·grim 에 기대고 있다. 아래 항목을 손으로 하면 된다.

## 확인할 것 (우선순위 순)

### 1. 기준선 — CI 가 이미 도는 조합

```
cargo test -p nl-core -p nl-io -p nl-cli -p nl-bundle -p nl-update -p nl-engine -p nl-gui -p nl-runtime
```

통과가 기대값이다. 여기서 깨지면 그것부터 보고한다.

### 2. `nl-app` 테스트 — 미지수

```
cargo test -p nl-app
```

CI Windows 잡이 이것만 뺀다(뷰 스냅샷이 많아 빌드 시간이 크다는 이유). 일부 단위 테스트가
`"/tmp/..."` 경로를 문자 그대로 비교하니 거기서 깨질 수 있다 — 그러면 **제품이 아니라 테스트**가
Linux 를 가정한 것이니 테스트 쪽을 고친다.

스냅샷 테스트는 GPU 어댑터나 CJK 글꼴이 없으면 스스로 건너뛴다. `NL_SNAPSHOT_REQUIRED=1` 은
**켜지 마라** — 그건 Linux CI 가 골든을 지키는 장치다.

### 3. 빌더 GUI 가 Windows 에서 뜨는가 — 한 번도 안 해 봤다

```
cargo run --release -p nl-app
```

- 창이 뜨는가, 한글이 깨지지 않는가
- 샘플을 열어 캔버스에 레이어를 놓고 포트를 이어 보라
- 학습 뷰에서 학습을 돌려 손실 곡선이 그려지는가
- `자원` 탭에서 장치 목록이 보이는가 (`--probe` 가 실제 GPU 를 고르는가)
- **스크린샷을 찍어 보고서에 첨부한다**

### 4. CLI 한 바퀴 — Linux 에서는 되는 것이 확인됨

```
cargo run --release -p nl-cli -- sample demo.nlproj
cargo run --release -p nl-cli -- train demo.nlproj --model "XOR MLP" --epochs 30
cargo run --release -p nl-cli -- infer demo.nlproj --model "XOR MLP" --input "[0.8,-0.8]"
cargo run --release -p nl-cli -- build demo.nlproj --target host --out dist
```

Linux 실측(참고값): 각각 0.03초 · 0.96초 · 즉시 · 4초. 학습은 val 정확도 98% 가 나온다.
추론 입력은 **`[-1, 1]` 범위의 부호 있는 값**이어야 한다(라벨이 `(x1>0) != (x2>0)` 이라
`0` 은 결정 경계다).

그다음 만들어진 배포판을 풀어 실행하고 HTTP 로 추론을 요청한다.

```
dist\stage-*\  안의 실행 파일 --headless --run-for 45 --device cpu
curl -X POST -H "content-type: application/json" -d "[0.8,-0.8]" http://127.0.0.1:8799/infer
```

Linux 에서는 기동 후 526ms 만에 첫 200 이 나왔고, 출력이 CLI 추론과 **소수점까지 같았다.**
Windows 에서도 같은 값이 나오는지 본다 — 다르면 그것이 발견이다.

포트 규칙: 샘플은 고정 포트 `8799` 를 쓴다. 다른 것이 그 포트를 쓰고 있으면
`packaging/lib.sh` 의 `nl_rebind_project` 로 빈 포트로 옮긴 사본을 쓰라
(`scripts/README.md` 참고).

### 5. 설치 프로그램 — wine 으로만 확인됨

Inno Setup 6 을 설치한 뒤:

```
cargo run --release -p nl-bundle --example nl-installer -- ^
    target\release\nl-app.exe "Neural Linker" 0.1.0 "Trust A&C" dist
```

- `setup.exe` 가 만들어지는가
- 실행해 `%LOCALAPPDATA%\Programs\<이름>\` 에 실제로 설치되는가
- 설치된 것이 실행되는가
- 제거가 깨끗한가

`nl build --target windows` 로 만든 **배포 앱**의 설치 프로그램도 같은 방식으로 본다.

### 6. 서비스 등록 — 미확인

`packaging/windows/install-service.ps1` 은 `schtasks` 의 ONLOGON 트리거로 배포 앱을 상시
기동한다. Linux 의 systemd 사용자 유닛에 대응하는 것이고, 그쪽은 확인돼 있다.

- 등록 → 로그인 시 자동 기동 → HTTP 응답
- 재시작 뒤에도 뜨는가
- 정상 종료·제거
- 환경 파일(`--env-file`) 권한이 Windows 에서 어떻게 되는가
  — **개인키 권한 비트가 Windows 에 없다는 것이 알려진 한계다**(`CHANGELOG.md`)

### 7. 화면 캡처 — 미확인

```
cargo run --release -p nl-cli -- record out --fps 4 --for 10
```

Windows 는 `xcap` 크레이트를 쓴다(Linux 는 순수 Rust 경로). 실기 확인이 없다.
프레임이 실제로 찍히는가, fps 가 나오는가, 다중 모니터에서 `--monitor` 가 맞는가.

### 8. HTTP 서버 소켓 동작

CI 는 컴파일과 단위 테스트만 본다. 실제로 돌려 봐야 아는 것들이 남아 있다.

- 동시 연결 여러 개
- 읽기 타임아웃 (`DeadlineIo`)
- 토큰 인증·`Origin` 거부·`Host` 검사가 Linux 와 같게 동작하는가
- TLS (`nl tls-cert` 로 인증서를 만들어 `--tls-cert`/`--tls-key` 로 기동)

## 보고서에 꼭 적을 것

1. 위 8개 항목 각각: **했다/안 했다**, 명령, 출력 요약, 판정
2. 고친 것이 있으면 무엇을 왜
3. Windows 에서만 드러난 차이 — 경로, 줄바꿈, 권한, 소켓, 글꼴, GPU 백엔드
4. 다음 사람이 알아야 할 환경 사실 (드라이버, 설치 경로, 걸린 시간)
