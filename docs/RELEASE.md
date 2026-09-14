# 릴리스 절차

태그 `v*` 를 밀면 CI(`.github/workflows/release.yml`, Forgejo 는 `.forgejo/` 에 같은 파일)가 Linux 네이티브와
Windows 크로스 빌드를 한 러너에서 만들고 매니페스트에 서명해 릴리스에 붙인다. 사람이 하는 일은
버전 올리기, 태그 밀기, 올라간 것을 확인하고 배포 서버에 올리기다.

설계는 `docs/ARCHITECTURE.md`, 업데이트 형식과 신뢰 모델은 `packaging/README.md` 의 "업데이트" 절에 있다.

---

## 첫 릴리스 전에 반드시 끝낼 것

이 셋이 없으면 자동 업데이트가 **꺼진 채로** 배포된다. 기능이 조용히 빠지는 것이 아니라
아예 켜지지 않으므로, 나가기 전에 확인해야 한다.

### 1. 서명 키

```sh
cargo run -p nl-update --example nl-keygen -- keygen
```

`packaging/keys/neural-linker.key`(비밀키, 0600)와 `.pub`(공개키)를 만든다. `minisign` CLI 는 필요 없다.
비밀키는 `.gitignore` 가 막는다 — 비밀번호가 없는 키라 **파일 자체가 곧 비밀**이다.

```sh
# 서명용 — 비밀키 파일 내용 전체
gh secret set MINISIGN_KEY < packaging/keys/neural-linker.key

# CI 가 올리기 전에 스스로 검증하는 데 쓴다 (.pub 의 둘째 줄, RWQ… 로 시작하는 한 줄)
tail -1 packaging/keys/neural-linker.pub | gh variable set UPDATE_PUBLIC_KEY
```

`MINISIGN_KEY` 가 없으면 CI 는 서명 단계를 건너뛴다(실패하지 않는다). 서명 없는 매니페스트는
앱이 받아들이지 않으므로, 시크릿이 빠진 릴리스는 업데이트가 동작하지 않는다.

### 2. 빌더에 공개키 박기

`crates/nl-app/src/update_key.rs`:

```rust
pub const PUBLIC_KEY: Option<&str> = Some("RWQ…");   // 지금은 None → 빌더 업데이트 꺼짐
pub const UPDATE_URL: &str = "https://updates.trustanc.dev/neural-linker/latest.json";
```

`PUBLIC_KEY` 가 `None` 이면 `Updater` 가 `Disabled` 로 남고 확인조차 하지 않는다.

### 3. 배포 서버

`UPDATE_URL` 과 CI 의 `UPDATE_BASE_URL` 변수가 가리키는 주소가 실제로 살아 있어야 한다.
**https 여야 하고**, 자산은 매니페스트와 같은 오리진에 둔다(다른 호스트를 쓰려면 매니페스트의
`allowed_asset_hosts` 에 적는다). 평문 http 는 앱이 거절한다.

---

## 체크리스트

### ① 버전 올리기

워크스페이스 `Cargo.toml` 의 `[workspace.package] version` 하나만 고치면 전 크레이트가 따라간다.

```sh
scripts/verify-all.sh          # 포맷·클리피·테스트·릴리스 빌드·종단 시험
scripts/verify-all.sh --gui    # 위 + 헤드리스 sway 시나리오(smoke·startup)
```

한 단계가 실패해도 끝까지 돌고 마지막에 실패 목록과 로그 경로를 낸다 — 릴리스 직전에 알아야 할 것은
"무엇이 처음 깨졌나" 가 아니라 "무엇무엇이 깨져 있나" 다. 첫 실패에서 멈추려면 `--fail-fast`.
로그는 `dist-local/verify/<시각>-<pid>/` 아래에 단계별로 남는다.

**다른 것이 돌고 있어도 된다.** 종단 시험은 샘플의 고정 포트(8799·8800)를 쓰지 않고 빈 포트로
옮겨 띄우므로, 같은 기계에서 누가 배포 앱이나 부하 시험을 돌리는 중이어도 통과한다. 포트가 막혔다고
남의 프로세스를 죽이지 마라 — 규칙은 `scripts/README.md`.

### ② CHANGELOG 갱신

[`CHANGELOG.md`](../CHANGELOG.md) 의 `[미출시]` 항목을 새 버전 제목으로 옮기고 날짜를 적는다.
그 위에 빈 `[미출시]` 를 새로 만든다.

```markdown
## [미출시]

## [0.2.0] — 2026-10-01
```

항목은 추가·변경·보안·수정으로 나눈다. **"알려진 제한" 표도 함께 손본다** — 고친 것은 지우고
새로 알게 된 것은 더한다. 이 표가 릴리스 노트에서 사용자가 가장 먼저 보는 부분이다.

매니페스트의 `notes` 는 CI 가 `NOTES="<버전> 릴리스"` 로 채운다. 바꾸려면
`packaging/make-manifest.sh` 를 부르는 워크플로 단계의 `NOTES` 를 고친다.

### ③ 키 확인

```sh
cargo run -p nl-update --example nl-keygen -- verify <직전 릴리스의 latest.json> \
  --pubkey packaging/keys/neural-linker.pub
```

직전 매니페스트가 지금 키로 검증되면 키가 바뀌지 않은 것이다. 키를 바꿔야 한다면
아래 "키를 잃어버렸을 때" 를 먼저 읽는다.

### ④ 로컬 드라이런 (선택)

태그를 밀기 전에 CI 가 무엇을 내놓을지 여기서 먼저 본다. `release.yml` 과 같은 함수
(`packaging/lib.sh`)를 쓰므로 결과가 어긋나지 않는다.

```sh
packaging/release-local.sh --crates nl-runtime,nl-cli --out /tmp/dist-local
```

`--key` 를 주지 않으면 **임시 키**로 서명하고 검증까지 해 본다(형식 확인용, 배포용 아님).
진짜 키로 보려면 `--key packaging/keys/neural-linker.key`, CI 와 같은 방식으로 보려면
`MINISIGN_KEY=... --key env`.

| 옵션 | 뜻 |
| --- | --- |
| `--crates` | 빌드할 크레이트 (기본 `nl-app,nl-runtime,nl-cli`) |
| `--targets` | `linux`, `windows` (기본 둘 다) |
| `--installer` | Inno Setup 이 있으면 Windows setup.exe 도 만든다 (CI 에는 없는 단계) |
| `--skip-build` | 이미 빌드된 산출물을 그대로 포장한다 |

### ⑤ 태그

```sh
git tag -a v0.2.0 -m "v0.2.0" && git push origin v0.2.0
```

태그 없이 시험만 하려면 Actions 에서 `workflow_dispatch` 로 돌린다 — 빌드와 서명은 하고
릴리스 첨부만 건너뛴다.

### ⑥ CI 산출물 확인

릴리스에 아래가 다 붙었는지 본다. 하나라도 빠지면 그 플랫폼 사용자는 업데이트를 받지 못한다.

| 파일 | 쓰임 |
| --- | --- |
| `neural-linker-<ver>-linux-x86_64.tar.gz` · `-windows-x86_64.zip` | 사람이 받는 배포본 |
| `nl-app` · `nl-app.exe` | 빌더 자동 업데이트가 그대로 내려받는 알맹이 |
| `nl-runtime` · `nl-runtime.exe` | 빌더가 배포 앱을 만들 때 쓰는 런타임 |
| `latest.json` + `.minisig` | 빌더 자체 업데이트 매니페스트 |
| `runtimes/latest.json` + `.minisig` | 빌더가 대상별 런타임을 받아 오는 매니페스트 |

`.minisig` 가 없으면 `MINISIGN_KEY` 시크릿이 빠진 것이다. 그대로 올리면 안 된다.

### ⑦ 서명 검증

CI 에도 검증 단계가 있지만(`UPDATE_PUBLIC_KEY` 변수가 있을 때), 올리기 전에 손으로 한 번 더 본다.

```sh
cargo run -p nl-update --example nl-keygen -- verify latest.json --pubkey packaging/keys/neural-linker.pub
cargo run -p nl-update --example nl-keygen -- verify runtimes/latest.json --pubkey packaging/keys/neural-linker.pub
```

이 명령은 배포 앱이 쓰는 `nl_update::verify_manifest` 를 그대로 부른다 — 여기서 통과하면
사용자의 앱에서도 통과한다.

매니페스트 안도 눈으로 본다.

- `version` 이 올린 버전과 같은가
- `published_at` 이 지금인가 (앱은 30일 넘은 매니페스트를 거절한다)
- 자산 `url` 이 https 이고 매니페스트와 같은 오리진인가
- 자산마다 `sha256` 과 `size` 가 있는가

### ⑧ 배포 서버 업로드

자산과 매니페스트를 같은 곳에 올린다. **매니페스트와 서명을 마지막에, 같이 올린다** —
자산보다 먼저 올리면 그 사이에 확인한 사용자가 404 를 만난다.

```
<base>/<버전>/  nl-app  nl-app.exe  *.tar.gz  *.zip
<base>/<버전>/runtimes/  nl-runtime  nl-runtime.exe  latest.json  latest.json.minisig
<base>/         latest.json  latest.json.minisig      ← 마지막
```

올린 뒤 실제로 받아지는지 본다.

```sh
curl -sSfI <base>/latest.json && curl -sSfI <base>/latest.json.minisig
```

### ⑨ 빌더에서 업데이트 확인

직전 버전 빌더를 실행해 새 버전 배지가 뜨는지, 내려받아 적용되는지, 다시 뜬 앱의 `--version` 이
새 버전인지 본다.

```sh
./nl-app --version          # 적용 뒤
```

시험 서버로 돌려 보려면 `NL_UPDATE_URL` 과 `NL_UPDATE_INSECURE=1` 을 함께 켠다.
루프백 http 로 띄웠다면 `NL_ALLOW_HTTP=1` 도 필요하다.

### ⑩ 배포 앱 종단 확인

새 빌더로 `.nlapp` 을 하나 만들어(입력 무장은 끄고) 배포 앱까지 돈다.

- 공개키를 넣은 빌드는 업데이트 배지가 뜨는가
- 공개키를 **비운** 빌드는 업데이트 UI 가 아예 없는가 (이게 정상이다)
- Windows 설치 프로그램이 만들어지고 `%LOCALAPPDATA%\Programs\<이름>\` 에 설치되는가

---

## 롤백

### 나간 릴리스를 되돌릴 때

업데이트는 **버전이 높은 쪽으로만** 간다. 그래서 버전을 낮추는 것으로는 되돌릴 수 없다.

1. **매니페스트를 먼저 되돌린다.** `<base>/latest.json` 과 `.minisig` 를 직전 정상 버전의 것으로
   덮어쓴다. 아직 확인하지 않은 사용자는 이 시점부터 나쁜 버전을 보지 않는다.
   `published_at` 이 30일보다 오래됐으면 앱이 거절하므로, 되돌릴 매니페스트를 **다시 만들어 다시 서명한다.**
2. 나쁜 자산 파일을 서버에서 지운다. 이미 매니페스트를 받은 사용자의 다운로드를 끊는다.
3. **고친 버전을 새 번호로 낸다.** 0.2.0 이 나빴다면 0.2.1 을 낸다. 이미 0.2.0 을 받은 사용자는
   0.2.1 로만 나올 수 있다.
4. GitHub 릴리스는 지우지 말고 "이 버전을 쓰지 마세요" 를 노트에 적는다. 지우면 이미 받은
   사용자가 무엇을 쓰고 있는지 추적할 수 없다.

### 키를 잃어버렸을 때

새 키로 바꾸면 **옛 공개키가 박힌 배포본은 새 매니페스트를 검증하지 못한다.** 그 사용자들은
자동 업데이트를 받지 못하고, 새 버전을 직접 내려받아 설치해야 한다. 자동으로 넘어가는 길은 없다 —
그것이 있으면 서명의 의미가 없다.

1. 새 키를 만들고(`keygen --force`) `MINISIGN_KEY`·`UPDATE_PUBLIC_KEY` 를 갈아 끼운다.
2. `crates/nl-app/src/update_key.rs` 의 `PUBLIC_KEY` 를 새 값으로 바꾼다.
3. 새 버전을 내고, 옛 사용자에게 **직접 내려받아 다시 설치해 달라**고 공지한다.
4. 배포 앱은 번들마다 키가 다르므로 빌더 사용자가 각자 같은 일을 해야 한다.

### 서명 없이 나간 릴리스

`MINISIGN_KEY` 가 빠져 `.minisig` 없이 매니페스트만 올라갔다면, 앱은 서명을 받지 못해
확인에 실패한다(업데이트가 조용히 되는 것이 아니라 실패로 보인다). 시크릿을 채우고
같은 버전으로 워크플로를 다시 돌려 매니페스트와 서명을 함께 덮어쓴다.
