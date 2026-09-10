# Neural Linker

범용 **신경망 모델 빌더 & 실행기**. 레이어 그래프를 캔버스에서 설계하고, CPU/GPU 에서 학습하고, 화면 캡처·마우스/키보드·
API·외부 앱 파이프라인에 연결한 뒤, 프로그램 안에서 GUI 를 디자인해 Linux / Windows 배포판으로 빌드·공유한다.

UI 구조·문서 상태(op 기반 undo)·GUI 테스트 하네스·패키징 방식은 [trust-pms](../trust-pms) 를 잇는다.

## 빌드 & 실행

```bash
cargo run --release -p nl-app            # 빌더
cargo run --release -p nl-runtime -- app.nlapp   # 배포 번들 실행기
cargo test --workspace                   # 단위·헤드리스 렌더 테스트
tools/uitest/uitest.sh start && tools/uitest/uitest.sh app   # 격리 GUI 테스트
```

GPU 는 wgpu(Vulkan/DX12/Metal) 로 사용하므로 CUDA 설치가 필요 없다. NVIDIA/AMD/Intel 모두 드라이버만 있으면 된다.

## 워크스페이스

| 크레이트 | 역할 |
|---|---|
| `nl-core` | 프로젝트·그래프·페이로드·파이프라인·GUI 스펙, op/undo, 형상 추론 (GUI/ML 무의존) |
| `nl-engine` | burn 인터프리터, CPU/GPU 장치, 학습, 체크포인트, 추론 |
| `nl-io` | 화면 캡처, 입력 시뮬레이션, HTTP/WS/stdio, 자원 조회 |
| `nl-app` | 빌더 GUI (eframe/egui, glow) |
| `nl-runtime` | 배포판 실행기 |

자세한 설계는 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), 계획은 [docs/ROADMAP.md](docs/ROADMAP.md).
