# GUI 테스트 하네스 (`tools/uitest`)

실제 데스크톱 세션의 키보드·마우스·화면을 **전혀 건드리지 않고** 앱을 띄워 클릭·키 입력·캡처를 하는 도구.
사람이 작업하는 동안 에이전트나 CI 가 백그라운드에서 GUI 검증을 돌릴 수 있다.

## 구조
- **sway 헤드리스**: `WLR_BACKENDS=headless` 로 가상 출력(기본 1600×1000)만 가진 컴포지터를 따로 띄운다.
  실제 입력 장치는 붙이지 않는다(`WLR_LIBINPUT_NO_DEVICES=1`). GPU 렌더링(gles2)을 쓴다.
- **vseat**(`tools/uitest/vseat`, Rust): 그 시트에 가상 포인터·키보드를 만들어 **붙들고 있는** 프로세스.
  장치가 없으면 시트에 pointer capability 가 없어 앱이 `wl_pointer` 를 못 받는다 — 클릭이 아무 데도 닿지 않는다.
- **입력**: 포인터는 `swaymsg seat - cursor set X Y / press / release`(절대 좌표, 드래그 가능), 키보드는 `wtype`
  (가상 키보드 프로토콜, 유니코드 문자열·수식키 조합).
- **캡처**: `grim`(그 컴포지터의 가상 출력만, 영역 지정 가능).
- **한글**: `uitest.sh ime` 가 fcitx5 를 **전용 D-Bus 세션** 안에서 같은 컴포지터에 붙인다(실제 세션의 fcitx5 와 이름 충돌
  없음). 앱 텍스트 필드에서 Ctrl+Space → 한글 조합이 text-input-v3 로 들어온다.

## 사용
```sh
U=tools/uitest/uitest.sh
$U start                      # 헤드리스 sway + vseat
UITEST_FRESH=1 $U app demo.nlproj   # 앱 실행(데이터 폴더 초기화 — 복구 모달이 안 뜬다)
$U shot /tmp/a.png            # 전체 캡처   /  $U shot /tmp/b.png "0,60 400x60" (영역)
$U click 161 45               # 좌클릭      /  click X Y right   /  dblclick X Y
$U drag 300 170 500 170       # 드래그(12단계 보간; 모션은 wlrctl 가상 포인터, 버튼은 sway IPC)
$U hold alt; $U drag 300 170 500 170; $U release alt   # Alt+드래그(노드 복제)
$U key -M ctrl -k s -m ctrl   # Ctrl+S      /  key -k Tab  /  key -k Return  /  key -M alt -k Left -m alt
$U type "sync"                # 문자열 입력
$U ime; $U click 130 77; $U key -M ctrl -k space -m ctrl; $U type "dkssud"   # → 안녕
$U run scenarios/smoke.uit    # 시나리오 실행(아래 "시나리오 러너")
$U log; $U status; $U stop
```
좌표는 가상 출력 기준이고 앱 창은 출력 전체를 채우므로 캡처의 픽셀 좌표를 그대로 쓰면 된다.

## 시나리오 러너

한 줄에 한 명령인 `.uit` 파일을 순서대로 실행한다. 명령 이름은 `uitest.sh` 하위 명령과 같아서
손으로 치던 것을 그대로 옮겨 적으면 된다.

```sh
$U start
UITEST_FRESH=1 $U run tools/uitest/scenarios/smoke.uit
$U stop
```

실패한 단계에서 즉시 멈추고, 어느 파일 몇 번째 줄인지와 함께 전체 화면을 `$UITEST_DIR/fail-<단계>.ppm`
으로 남긴다. 종료 코드로 성공(0)·실패(1)를 알리므로 CI 에 그대로 건다.

### 문법

| 쓰기 | 뜻 |
| --- | --- |
| `# …` | 줄 전체 주석. 줄 맨 앞(앞쪽 공백 허용)에만 쓴다 — 인자 안의 `#` 는 글자 그대로다 |
| `set 이름 값…` | 변수 정의. 명령줄에서 `이름=값` 으로 준 것이 있으면 그쪽이 이겨 기본값 노릇을 한다 |
| `$이름` · `${이름}` | 토큰 안 어디서나 치환. 정의되지 않은 변수는 오류 |
| `include 다른.uit` | 그 자리에 펼친다. 경로는 포함하는 파일 기준. 순환은 오류 |
| `"따옴표"` | 공백이 든 인자를 한 덩어리로 (shlex 규칙) |

명령은 `app`, `wait-app`, `wait-log`, `ime`, `shot`, `expect-shot`, `move`, `click`, `dblclick`,
`drag`, `hold`, `release`, `key`, `key-until`, `type`, `sleep <초>`, `log`, `stop`.

`key-until "<정규식>" <wtype 인자…>` 는 키를 보내고 그 결과가 로그에 보일 때까지 다시 보낸다(기본 5회,
`UITEST_KEY_TRIES`·`UITEST_KEY_WAIT` 로 조절). 컴포지터가 가상 키보드를 등록하기 전에 보낸 키는 조용히
사라지는데, 앱이 `input ready` 를 찍은 뒤에도 하네스를 막 띄운 회차에서는 첫 한두 개가 없어진다.
**여러 번 눌러도 결과가 같은 동작에만 쓴다** — 뷰 전환은 괜찮고, 토글이나 카운터에는 쓰면 안 된다.

좌표 상수는 시나리오 맨 위에 모아 두면 화면이 바뀌었을 때 한 곳만 고친다.

```
set TOOLBAR_Y 45
set SAVE_X 161
click $SAVE_X $TOOLBAR_Y
```

#### 따옴표에서 걸리는 두 가지

`shot` 에 **상대 경로**를 주면 현재 폴더가 아니라 `$UITEST_SHOT_DIR` 아래에 떨어진다. 저장소 루트에서
시나리오를 돌려도 작업 트리가 더러워지지 않는다. 특정 위치에 남기고 싶으면 절대 경로를 준다.

**공백이 든 인자는 반드시 따옴표로 묶는다.** 셸에서 한 덩어리로 주던 것을 시나리오에 그대로 옮기면
토큰이 쪼개져 엉뚱한 자리로 들어간다. `wait-log` 의 정규식과 `shot` 의 영역이 특히 그렇다.

```
wait-log \[nl-app\] ready 10      # ✖ "ready" 가 초 인자로 들어가 "unbound variable"
wait-log "\[nl-app\] ready" 10    # ✔
shot out.png 0,35 760x25          # ✖ grim 이 "invalid geometry" 를 내고 전체 화면을 찍는다
shot out.png "0,35 760x25"        # ✔
```

**`set` 값에 든 공백은 치환해도 쪼개지지 않는다.** 변수는 토큰 하나로 들어가므로 좌표 쌍을 한 변수에
담으면 인자 하나가 되어 버린다. 좌표는 축마다 변수를 따로 둔다.

```
set PARK 800 850
move $PARK                        # ✖ "800 850" 이 한 인자 — "$2: unbound variable"
set PARK_X 800
set PARK_Y 850
move $PARK_X $PARK_Y              # ✔
```

### 화면이 준비되기를 기다리기

캡처를 OCR 할 수는 없으니 "무엇이 떴다" 는 **앱이 로그에 찍는 마커**로 기다린다.

```
wait-log "프로젝트 열림" 10      # app.log 에 그 정규식이 나올 때까지 최대 10초
```

`expect-shot` 은 찍기 전에 잇따른 두 캡처가 같아질 때까지 기다려(기본 8회 × 0.3초) 애니메이션과
커서 깜빡임 때문에 나는 헛실패를 막는다. 다만 **앱이 준비되기 전에도 화면은 멎어 있을 수 있으므로**
프레임 안정 판정만으로 시작을 기다려서는 안 된다. 빌더는 아래 마커를 찍는다.

| 마커 | 언제 | 쓰임 |
| --- | --- | --- |
| `[nl-app] ready` | 첫 화면을 다 그린 뒤 | 첫 캡처를 여기서 기다린다 |
| `[nl-app] focused` | 창이 키보드 포커스를 받은 뒤 | 포커스가 왔다는 것까지만 알려 준다 |
| `[nl-app] input ready` | 포커스 뒤 키보드가 자리를 잡은 뒤 | **키를 넣기 전에 이것을 기다린다** |
| `[nl-app] devices ready` | 장치 열거·확인이 끝난 뒤 | 상태바의 장치 이름이 확정된다 |
| `[nl-app] view <이름>` | 뷰가 바뀔 때마다 | 전환이 실제로 일어났는지 확인 |

화면이 그려진 것과 입력을 받을 수 있는 것은 다르다. 포커스를 받기 전에 보낸 키는 조용히 사라지므로
`ready` 만 기다리고 키를 넣으면 간헐적으로 씹힌다.

포커스 **직후**에도 한 번 더 사라진다. 컴포지터가 그 시점에 키보드를 다시 만들고(winit 이
`non-xkb compatible keymap` 을 찍는 구간) 그 사이의 키는 어디에도 닿지 않는다. 하네스를 처음 띄운
회차에서 `Ctrl+1` 이 유실되던 것이 이 구간이었다. `input ready` 까지 기다리면 사라지지 않는다.

## 골든 이미지

`expect-shot <이름> [x,y WxH]` 는 `tools/uitest/golden/<이름>.ppm` 과 견준다.

- **골든이 없으면** 지금 캡처를 골든으로 삼고 "골든 생성" 으로 알린다(그 단계는 통과).
  만들어진 PPM 을 **눈으로 확인한 뒤** 커밋해야 다음 실행부터 실제 비교가 된다.
- **있으면** 다른 픽셀 비율을 재서 허용 오차를 넘으면 실패하고 차이 이미지를
  `$UITEST_DIR/diff/<이름>.ppm` 에 남긴다(다른 곳은 빨강, 같은 곳은 원본을 어둡게).

| 환경 변수 | 기본 | 뜻 |
| --- | --- | --- |
| `UITEST_TOLERANCE` | `0.5` | 다른 픽셀이 이 비율(%)을 넘으면 실패 |
| `UITEST_PIXEL_DELTA` | `8` | 채널 차이가 이 값 이하인 픽셀은 같은 것으로 (안티에일리어싱 잔 떨림) |
| `UITEST_GOLDEN_DIR` | `tools/uitest/golden` | 골든 위치 |
| `UITEST_SHOT_DIR` | `$UITEST_DIR/shots` | `shot` 에 상대 경로를 주면 떨어지는 곳 |
| `UITEST_KEY_TRIES` | `5` | `key-until` 이 키를 다시 보내는 횟수 |
| `UITEST_KEY_WAIT` | `2` | 한 번 보낸 뒤 결과를 기다리는 시간(초) |
| `UITEST_DIFF_DIR` | `$UITEST_DIR/diff` | 차이 이미지 위치 |
| `UITEST_SETTLE_TRIES` | `8` | 프레임이 멎을 때까지 다시 찍는 횟수 |
| `UITEST_SETTLE_INTERVAL` | `0.3` | 그 사이 간격(초) |
| `UITEST_APP_BIN` | `target/release/nl-app` | 다른 트리에서 빌드한 바이너리로 돌릴 때 |

### 골든 갱신 절차

화면이 의도적으로 바뀌었으면 (1) 옛 골든을 지우고 (2) 시나리오를 한 번 돌려 새로 만들고
(3) 새 PPM 을 눈으로 확인한 뒤 커밋한다.

```sh
rm tools/uitest/golden/03-view3.ppm
UITEST_FRESH=1 $U run tools/uitest/scenarios/smoke.uit   # "골든 생성" 확인
```

골든을 새로 만들 때는 **전환이 실제로 일어났는지 확인하는 `wait-log` 를 시나리오에 두어야 한다.**
고정 `sleep` 만 쓰면 키나 클릭이 한 번 씹혔을 때 이전 화면이 그대로 골든으로 박히고, 그 뒤로는
올바른 화면이 "차이" 로 보고된다. 실제로 그렇게 만들어진 골든을 한 번 걷어냈다.

캡처 전에는 커서를 빈 곳으로 치운다(`move`). 버튼 위에 둔 채로 찍으면 호버 강조가 들어가고,
툴팁이 뜨는 타이밍에 따라 같은 화면이 달라 보인다.

의도치 않은 차이인지 가리려면 먼저 차이 이미지를 본다. PPM 은 대부분의 뷰어가 바로 열고,
PNG 가 필요하면 `magick diff/03-view3.ppm 03-view3.png` 처럼 바꾼다.

허용 오차를 올려 넘기는 것은 마지막 수단이다. 0.5% 는 1600×1000 에서 8000 픽셀이라
작은 위젯 하나가 통째로 바뀌어도 통과할 만큼 이미 넉넉하다.

## 주의
- **드래그 모션**: 버튼이 눌린 동안 `swaymsg seat cursor set` 의 이동은 클라이언트에 전달되지 않는다(릴리스가 누른 자리에서
  일어난 것처럼 보임). `drag` 는 그래서 모션을 `wlrctl pointer move`(상대)로 넣는다. 수식키 유지는 `hold`/`release`.
- **모달**: 앱에 모달(저장 확인·복구 제안)이 떠 있으면 뒤쪽 클릭이 전부 막힌다. 클릭이 "안 먹는" 것처럼 보이면 먼저
  전체 캡처로 모달 여부를 확인한다. 강제 종료 뒤 재시작하면 복구 모달이 뜨므로 테스트 시작은 `UITEST_FRESH=1`.
- `pgrep -f`/`pkill -f` 에 명령줄 일부를 넣으면 호출한 셸 자신을 죽일 수 있다. 스크립트는 `/proc/<pid>/cmdline` 을 본다.
- **앱은 한 번에 하나만**: `app` 과 `run` 은 시작할 때 남아 있는 앱을 내리고 창이 사라진 것까지 확인한다(pid 파일과 `swaymsg -t get_tree` 의 `app_id=neural-linker` 만 본다). 일부러 띄워 둔 앱에 이어 붙이려면 `run … --keep-app`.
- 병렬로 쓰려면 `UITEST_DIR` 을 다르게 준다.
- 앱 로그의 "arboard clipboard: X11 …" 경고는 헤드리스라 X 가 없어서 나는 것으로 무해하다.

## 검증 이력
2026-09-11: 시나리오 러너와 골든 비교를 `scenarios/smoke.uit` 로 확인. 같은 시나리오를 두 번 돌려
8장 전부 일치(뷰 7 만 0.004% 잔 떨림, 허용 0.5% 안), 골든 하나를 일부러 바꿔치기하니 그 단계에서
멈추고 차이 이미지와 `fail-<단계>.ppm` 을 남기며 종료 코드 1. 앱 바이너리는 `UITEST_APP_BIN` 으로
다른 워크트리 것을 썼다.
2026-09-10: 표 탭 클릭, 검색창 타이핑, fcitx5 한글 조합("dkssud" → "안녕"), 자동 저장 복구 모달 표시를 이 하네스로 확인.
