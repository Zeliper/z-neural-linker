# nl-gui 테스트

| 파일 | 무엇을 보는가 |
| --- | --- |
| `render_headless.rs` | 9종 위젯이 Run/Design 두 모드에서 패닉 없이 그려지는지, 클릭이 이벤트를 내는지 (픽셀은 안 본다) |
| `snapshot.rs` | 그려진 **픽셀**을 `snapshots/` 의 골든 PNG 와 견준다 |

## 골든 이미지 스냅샷

`egui_kittest` 가 wgpu 로 오프스크린 렌더해 PNG 로 굳힌다. **컴포지터가 필요 없어** CI 에서 그대로 돈다.
sway 를 띄우는 `tools/uitest` 와 역할이 다르다 — 저쪽은 실제 창·실제 입력을 보고, 이쪽은 위젯이 그려진 픽셀만 본다.

```sh
cargo test -p nl-gui --test snapshot            # 견주기
UPDATE_SNAPSHOTS=1 cargo test -p nl-gui         # 골든 갱신
```

갱신하면 옛 골든이 `snapshots/<이름>.old.png` 로, 차이가 `snapshots/<이름>.diff.png` 로 남는다.
**새 PNG 를 눈으로 확인한 뒤** 커밋한다. `.old`/`.diff`/`.new` 는 커밋하지 않는다.

| 골든 | 상태 |
| --- | --- |
| `gui-run.png` | Run 모드, 값 없음 (Image·Plot 은 자리표시자) |
| `gui-run-filled.png` | Run 모드, 플롯 30점·값 위젯 숫자·16×16 체커보드 텍스처·슬라이더 7.5 |
| `gui-design.png` | Design 모드, 버튼 선택 (강조 테두리 + 모서리 핸들), 위젯 상호작용은 비활성 |

### 허용 오차

`SnapshotOptions::threshold(0.7)` 는 **픽셀 하나**가 얼마나 달라도 되는지(가중 YIQ 거리)이고,
`max_failed_pixels(64)` 는 그 기준을 넘는 픽셀이 몇 개까지 허용되는지다. 글꼴 안티에일리어싱과
wgpu 백엔드 차이를 흡수할 만큼이되, 경계선 1px 이동 같은 진짜 변화는 잡히는 값이다.
통과시키려고 올리기 전에 `.diff.png` 를 먼저 본다.

### 글꼴

이 스냅샷은 **글꼴을 얹지 않는다**. `nl_gui::font_definitions()` 가 고르는 시스템 CJK 글꼴은 기계마다 달라
골든이 그 기계 전용이 된다. 대신 시험용 라벨을 ASCII 로 두고 egui 기본 글꼴만 쓴다 — 어디서 돌려도 같은 픽셀이 나온다.
한글 렌더링은 `render_headless.rs` 가 픽셀 없이 확인한다.

### 렌더 백엔드가 없을 때

wgpu 어댑터가 없으면 테스트는 이유를 찍고 **건너뛴다**. cargo 는 통과한 테스트의 출력을 삼키므로
CI 는 `NL_SNAPSHOT_REQUIRED=1` 을 켜 둔다 — 그러면 건너뛰는 대신 실패해서 조용한 초록불이 생기지 않는다.

```sh
NL_SNAPSHOT_REQUIRED=1 cargo test -p nl-gui     # CI 에서 이렇게
```

Linux 에 GPU 가 없어도 소프트웨어 래스터라이저(mesa 의 lavapipe, Fedora `mesa-vulkan-drivers`)만 있으면 돈다.
`egui_kittest` 는 어댑터를 고를 때 CPU 를 **먼저** 집으므로 결과가 기계별 GPU 드라이버에 흔들리지 않는다.
