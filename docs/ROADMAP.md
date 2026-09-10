# 로드맵

2026-09-10 시작. 설계는 `docs/ARCHITECTURE.md`. 각 마일스톤은 "빌더에서 만들고 배포판에서 도는" 수직 조각을 하나씩 늘린다.

## M0 — 뼈대와 첫 수직 조각 (진행 중)
- [x] 워크스페이스·문서·`nl-core` 데이터 모델 계약
- [ ] `nl-core`: 그래프 op/undo/diff, 형상 추론, 검증, 직렬화 왕복 테스트
- [ ] `nl-engine`: burn 인터프리터(Input/Linear/Conv2d/Pool/Flatten/Activation/Dropout/BatchNorm/Add/Concat/Output),
      CPU(ndarray)/GPU(wgpu) 장치 선택, 학습 루프(SGD/Adam/AdamW, MSE/CrossEntropy/BCE), safetensors 체크포인트, 추론 세션
- [ ] `nl-io`: 자원 조회, 화면 캡처, 입력 시뮬레이션, HTTP 호출 — 최소 API
- [ ] `nl-app`: 프로젝트 관리(새로/열기/저장/최근), 모델 캔버스, 인스펙터, 학습 뷰(손실 플롯), 자원 패널, 빌드 뷰(Linux tar.gz)
- [ ] `nl-runtime`: 번들 로드 + GUI 레이아웃 렌더 + 추론
- [ ] `tools/uitest` 이식(app_id `neural-linker`), egui_kittest 헤드리스 렌더 테스트
- [ ] 패키징(`packaging/linux`, `packaging/windows`) 이식

## M1 — 데이터·페이로드·파이프라인
- 데이터셋: CSV, 이미지 폴더, 녹화(화면 + 입력 라벨) 가져오기와 미리보기
- 페이로드 편집기(필드·Transform 체인), 인코더/디코더 실행(`codec`)
- 파이프라인 캔버스: 화면 캡처 → 모델 → 마우스/키보드, HTTP 폴링 → 모델 → HTTP 호출, stdio JSON 연결
- 파이프라인 시험 실행(빌더 내부) + 킬 스위치
- 자동 저장·복구(trust-pms `recovery.rs` 이식)

## M2 — GUI 디자이너·Windows 배포·도구 설치
- GUI 디자이너(위젯 팔레트·드래그 배치·바인딩) + 런타임 공용 렌더러
- Windows 빌드: 런타임 바이너리 매니페스트 내려받기(동의 팝업), zip + Inno Setup(가능할 때)
- 자동 업데이트(trust-pms `update.rs` 이식), 매니페스트 서명(minisign)
- 도구 설치 관리자: 상태 점검 → 동의 → 설치 → 재점검

## M3 — 모델 관리·고급 레이어·상호운용
- 모델 레지스트리: 실행 기록 비교(지표 표), 버전 태그, 가중치 내보내기/가져오기
- 레이어: Embedding, LSTM/GRU, MultiHeadAttention, Transformer 블록, Residual 템플릿
- ONNX 가져오기(tract 로 추론 전용) / 내보내기(검토)
- 학습 상황 프리셋: 분류·회귀·화면 상태 분류·행동 복제(입력 라벨) 템플릿

## M4 — 협업·서버형 배포
- `--headless` 런타임 + HTTP/WS 서빙, 프로젝트 동기화 서버(trust-pms `pms-server` 계열, op 경로 재사용)
