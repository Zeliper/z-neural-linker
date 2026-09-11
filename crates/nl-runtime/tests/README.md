# nl-runtime 테스트

| 자리 | 무엇을 보는가 |
| --- | --- |
| `src/cli.rs` 의 `mod tests` | 명령줄 인자 해석 |
| `src/app.rs` 의 `mod tests` | 앱 상태 전이, 작업 폴더, 업데이트 배지, **골든 이미지 스냅샷** |
| `src/update.rs` 의 `mod tests` | 업데이트 확인·자동 다운로드 조건 |
| `tests/cli_smoke.rs` | 실제 실행 파일을 돌려 `--version`·`.nlapp`·첨부 번들 실행 확인 |

스냅샷이 `src/` 안 단위 테스트인 것은 `nl-runtime` 이 bin 크레이트라 `tests/` 에서 내부를 볼 수 없기 때문이다.
골든 PNG 는 그래도 `tests/snapshots/` 에 떨어진다 (`egui_kittest` 기본 경로).

## 골든 이미지 스냅샷

```sh
cargo test -p nl-runtime runtime_app_snapshot       # 견주기
UPDATE_SNAPSHOTS=1 cargo test -p nl-runtime         # 골든 갱신
NL_SNAPSHOT_REQUIRED=1 cargo test -p nl-runtime     # CI: 건너뜀을 실패로
```

| 골든 | 상태 |
| --- | --- |
| `runtime-app.png` | 파이프라인 실행 중 + 통계(30Hz·틱당 0.4ms) + 새 버전 배지 + 번들 GUI |

상단 바 숫자는 `inject_stats` 로 박고 새 버전은 `UpdateUi::inject` 로 밀어 넣는다. 실제 틱 속도나
네트워크에 흔들리지 않게 하려는 것이다. 자세한 허용 오차·갱신 규칙은 `../../nl-gui/tests/README.md` 와 같다.

### 글꼴

런타임 상단 바는 한국어라 CJK 글꼴 없이 찍으면 두부 글자만 남아 사람이 검토할 수 없다.
그래서 골든을 만든 것과 **같은 글꼴**(Noto Sans CJK Regular)이 있을 때만 비교하고, 없으면 건너뛴다 —
다른 글꼴로 찍혀 영문 모를 불일치가 나는 것보다 낫다.

- Fedora: `google-noto-sans-cjk-fonts`
- Debian/Ubuntu: `fonts-noto-cjk`

`nl-gui` 스냅샷은 라벨이 ASCII 라 이 제약이 없다.
