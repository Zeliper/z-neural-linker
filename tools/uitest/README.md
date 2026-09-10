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
$U log; $U status; $U stop
```
좌표는 가상 출력 기준이고 앱 창은 출력 전체를 채우므로 캡처의 픽셀 좌표를 그대로 쓰면 된다.

## 주의
- **드래그 모션**: 버튼이 눌린 동안 `swaymsg seat cursor set` 의 이동은 클라이언트에 전달되지 않는다(릴리스가 누른 자리에서
  일어난 것처럼 보임). `drag` 는 그래서 모션을 `wlrctl pointer move`(상대)로 넣는다. 수식키 유지는 `hold`/`release`.
- **모달**: 앱에 모달(저장 확인·복구 제안)이 떠 있으면 뒤쪽 클릭이 전부 막힌다. 클릭이 "안 먹는" 것처럼 보이면 먼저
  전체 캡처로 모달 여부를 확인한다. 강제 종료 뒤 재시작하면 복구 모달이 뜨므로 테스트 시작은 `UITEST_FRESH=1`.
- `pgrep -f`/`pkill -f` 에 명령줄 일부를 넣으면 호출한 셸 자신을 죽일 수 있다. 스크립트는 `/proc/<pid>/cmdline` 을 본다.
- 병렬로 쓰려면 `UITEST_DIR` 을 다르게 준다.
- 앱 로그의 "arboard clipboard: X11 …" 경고는 헤드리스라 X 가 없어서 나는 것으로 무해하다.

## 검증 이력
2026-09-10: 표 탭 클릭, 검색창 타이핑, fcitx5 한글 조합("dkssud" → "안녕"), 자동 저장 복구 모달 표시를 이 하네스로 확인.
