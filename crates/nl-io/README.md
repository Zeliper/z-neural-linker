# nl-io

화면 캡처 · 마우스/키보드 · HTTP · WebSocket · 자원 조회 어댑터와 **파이프라인 실행기**.

빌더(`nl-app`)의 "시험 실행", 배포 런타임(`nl-runtime`), 명령줄(`nl run`)이 모두 여기 있는 같은
`Runner` 를 쓴다. 그래서 빌더에서 돌던 것이 배포판에서 그대로 돈다.

전체 설계는 [`docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md), 사용자 관점 설명은
[`docs/GUIDE.md`](../../docs/GUIDE.md) 에 있다.

## 모듈 지도

| 파일 | 하는 일 |
| --- | --- |
| `runner.rs` | 파이프라인 틱 루프. 소스 → 모델/로직 → 싱크. 이 크레이트에서 가장 큰 파일이다 |
| `httpd.rs` | `Source::HttpServer` 가 쓰는 최소 HTTP/1.1 서버. 표준 라이브러리 + rustls 만 쓴다 |
| `http.rs` | 바깥으로 나가는 HTTP 호출 (ureq 3, 타임아웃 필수) |
| `screen.rs` | 화면 캡처. 플랫폼마다 다른 백엔드를 고른다 |
| `input.rs` | 마우스·키보드 시뮬레이션 (enigo). 무장 플래그가 꺼져 있으면 실제 입력을 보내지 않는다 |
| `record.rs` | 화면 녹화 → `DataSource::Recorded` 폴더 (`frames/*.png` + `labels.jsonl` + `meta.json`) |
| `resources.rs` | CPU·메모리·GPU 스냅샷 (상태바·자원 패널) |
| `lib.rs` | 공개 API. 여기 re-export 된 것이 계약의 전부다 |

`build.rs` 는 Linux 에서만 일한다. `libxkbcommon.so` 개발 심볼릭 링크가 없는 배포판을 위해
`OUT_DIR` 에 링크를 만들어 준다 — 시스템 개발 패키지를 요구하지 않으려는 것이다.

## 공개 API

`lib.rs` 의 re-export 가 계약이다. 모듈 자체도 `pub` 이지만, 다른 크레이트는 아래 이름만 쓴다.

| 이름 | 쓰임 |
| --- | --- |
| `Runner`, `RunnerHandle`, `RunnerEvent`, `RunnerInput` | 파이프라인 실행. `Runner::new(project, pipeline, base_dir, device).start()` |
| `PREVIEW_MAX_SIDE` | `RunnerEvent::ValuePreview` 썸네일의 최장변 상한 |
| `capture`, `monitors`, `Frame`, `MonitorInfo` | 한 장 캡처와 모니터 목록 |
| `Capturer`, `Backend` | 연결을 유지하는 반복 캡처기와 실제로 쓰인 백엔드 |
| `Recorder`, `RecorderHandle`, `FrameSource` | 녹화. `FrameSource` 로 다른 프레임 공급원을 끼울 수 있다 |
| `perform`, `InputSim`, `parse_key` | 입력 시뮬레이션과 키 이름 해석 |
| `call`, `HttpResponse` | 아웃바운드 HTTP |
| `snapshot`, `ResourceSnapshot` | 자원 스냅샷 |

`httpd` 모듈은 `Server`·`Request`·`Conn`·`TlsAcceptor`·`respond_raw`·`describe_io_error` 와
아래 상한 상수들을 내보낸다. 보통은 `Runner` 가 알아서 쓰므로 직접 부를 일이 없다.

## 보안 규칙

이 크레이트가 바깥과 닿는 유일한 입구라 자원과 권한을 전부 여기서 쥔다.

- **토큰** — `Source::HttpServer::token` 이 있으면 요청마다 `Authorization: Bearer <토큰>` 또는
  `X-NL-Token: <토큰>` 을 요구한다. 비교는 상수 시간이다. 틀리면 401.
- **루프백 전용** — 토큰이 없으면 루프백 주소에만 연다. 바깥에서 닿는 주소는 **소켓을 열기 전에**
  거부한다. 잠깐이라도 무방비로 열려 있으면 안 되기 때문이다.
- **토큰 재정의** — `NL_HTTP_TOKEN` 또는 `NL_HTTP_TOKEN_<포트>` 가 번들에 박힌 토큰을 덮어쓴다.
  포트별 변수가 우선이다. 배포판의 토큰은 앱을 받은 사람이 다 볼 수 있으므로, 서버로 돌릴 때는
  실행 환경에서 새로 준다.
- **Origin** — `Origin` 헤더가 있으면 403. 브라우저에서 온 요청은 토큰이 맞아도 받지 않는다.
- **Host** — `Host` 가 바인드 주소도 `localhost` 도 아니면 400. DNS 리바인딩 차단이다.
  정책은 **실제로 묶인 주소**로 만든다. `:0` 으로 열면 운영체제가 포트를 고르기 때문이다.
- **TLS** — `Source::HttpServer::tls` 에 인증서·키 PEM 경로를 주면 https 로 연다. rustls 에
  `ring` 제공자를 쓴다(시스템 라이브러리 불필요). 인증서가 잘못되면 **포트를 열기 전에** 실패한다.
  TLS 는 도청과 중간자를 막을 뿐 **인증을 대신하지 않는다** — 토큰 규칙은 그대로다.
- **경로 담장** — 파일 소스/싱크, 모델 가중치, TLS 인증서 경로는 모두 `base_dir` 아래로 묶는다.
  절대 경로와 `..` 은 거부하고, 심볼릭 링크로 빠져나가는 것도 막는다. 인증서만은 `base_dir` 다음에
  **실행 파일이 있는 폴더**도 본다. 배포 번들에는 개인키를 담지 않기 때문이다.
- **입력 무장** — `Sink::MouseKeyboard` 는 `Runner::arm_input` 이 켜져 있을 때만 실제 입력을 보낸다.
  기본은 꺼짐(로그만). 실행 중에도 `RunnerHandle::set_armed` 로 끌 수 있고, 액션 직전에 다시 읽는다.
- **WebSocket** — `ws://`·`wss://` 만 받는다.

## 자원 상한

| 무엇 | 값 | 넘으면 |
| --- | --- | --- |
| 요청 머리 전체 | 5초 | 408 |
| 요청 본문 | 30초 | 끊음 |
| 응답 쓰기 | 10초 | 끊음 |
| TLS 핸드셰이크 | 5초 | 끊음 (응답 없음) |
| 머리 바이트 | 16 KiB | 431 |
| 머리 줄 수 | 64 | 431 |
| 본문 크기 | 8 MiB | 413 |
| 동시 연결 | 64 | 503 (TLS 면 조용히 닫음) |
| 틱으로 넘기는 요청 큐 | 16 | 503 |
| 파이프라인 응답 | 10초 | 504 |
| WebSocket·stdin 큐 | 256 | 오래된 것부터 버리고 1초에 한 줄 보고 |
| 이미지 이벤트 백로그 | 4 | 새 프레임을 버림 |
| 틱 속도 | 0.05 ~ 240 Hz | 범위로 잘림 |
| 녹화 쓰기 큐 | 32 | 프레임 버림 |
| 녹화 연속 실패 | 30 | 녹화 중단 |

머리 마감은 소켓 타임아웃만으로 세지 않는다. 한 바이트씩 흘리는 상대를 잡으려면 **총 마감**을
따로 두고 읽기마다 확인해야 한다. TLS 핸드셰이크도 같은 수법을 쓴다.

## 알려진 한계

- **keep-alive 가 없다.** 연결 하나가 요청 하나다. 상태가 없어 동시 연결 수 세기가 정확해지고,
  파이프라인의 "요청 하나씩" 규칙과도 맞는다. 응답마다 `Connection: close` 를 붙인다.
- **요청은 한 번에 하나씩** 처리한다. 앞 요청이 답을 받아야 다음 것을 꺼낸다. 그래야 FIFO 짝짓기가
  정확하다. `tick_hz` 가 곧 초당 처리량의 상한이다.
- **xdg-desktop-portal 백엔드는 느리다.** 한 장에 300ms 를 넘고 초당 1~3장이 한계다. 그래서 캡처는
  **전용 스레드**에서 돌고 틱은 최신 한 장만 집어 간다. 못 따라가 버린 프레임 수는 1초에 한 줄로 알린다.
- **다출력 모델은 Output 노드 이름이 순서를 정한다.** `Graph::output_nodes()` 가 이름순으로
  정렬하고 페이로드 출력 필드와 그 순서로 짝짓는다. 이름을 비워 두면 무작위 id 순이 되어 필드가
  뒤바뀐다. 입력도 같다. `nl inspect` 가 이 순서를 표로 보여 준다.
- **클라이언트 인증서(mTLS)는 받지 않는다.** 누가 부르는지는 토큰으로 가린다.
- Windows 에는 유닉스 파일 권한 비트가 없다. 개인키 파일을 사용자 폴더 밖에 두면 안 된다.

## 플랫폼별 백엔드

캡처는 `Capturer::new()` 가 아래 순서로 시도하고, 실제로 쓰인 것은 `Backend` 로 알려 준다.

| 플랫폼 | 캡처 | 비고 |
| --- | --- | --- |
| Linux Wayland (wlroots) | `Backend::Wayland` — libwayshot | 컴포지터에서 직접 받아 초당 수십 장 |
| Linux Wayland (GNOME·KDE) | `Backend::Portal` — xdg-desktop-portal, zbus | 초당 1~3장. 첫 호출에 권한 창이 뜰 수 있다 |
| Linux X11 | `Backend::X11` — x11rb `GetImage` | XWayland 는 루트가 비어 있어 쓸 수 없다 |
| Windows · macOS | `Backend::XCap` — xcap | |

입력은 어디서나 enigo 다. Linux 에서만 `x11rb`·`wayland` 기능을 켠다. 소켓 오류 번호는 플랫폼마다
달라서(주소 사용 중이면 Linux 98, Windows 10048) `httpd::describe_io_error` 가 한국어로 풀어 준다.

## 시험 게이트

없는 환경에서 조용히 통과하는 시험이 있다. 켜는 스위치는 환경 변수다.

| 변수 | 없으면 | 켜면 |
| --- | --- | --- |
| `NL_TEST_GPU=1` | GPU 경로를 건너뛴다 | wgpu 어댑터로 실제 연산을 돌린다 |
| `NL_E2E=1` | 종단 시험을 건너뛴다 (`nl-cli`) | 학습·빌드·배포판 실행까지 돈다 |
| `DISPLAY`/`WAYLAND_DISPLAY` | 화면 캡처 시험을 건너뛴다 | 실제로 한 장 찍는다 |

화면 캡처 시험은 세션이 있어도 캡처기를 못 열면 환경 문제로 보고 통과한다. 헤드리스 CI 에서
빨간불이 나지 않게 하려는 것이다.

```sh
cargo test -p nl-io                    # 평소
NL_TEST_GPU=1 cargo test -p nl-io      # GPU 경로까지
```
