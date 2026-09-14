# 매니페스트 서명 키

여기에 `neural-linker.key`(비밀키)와 `neural-linker.pub`(공개키)가 놓인다.
**비밀키는 `.gitignore` 가 막는다** — 비밀번호가 없는 키라 파일 자체가 곧 비밀이다.

```sh
# 만들기 (한 번만)
cargo run -p nl-update --example nl-keygen -- keygen

# 서명
cargo run -p nl-update --example nl-keygen -- sign dist/latest.json --key packaging/keys/neural-linker.key

# 검증 (배포 앱이 쓰는 함수를 그대로 부른다)
cargo run -p nl-update --example nl-keygen -- verify dist/latest.json --pubkey packaging/keys/neural-linker.pub
```

`minisign` CLI 를 깔지 않아도 된다. CI 러너에도 설치 단계가 없다.

키를 잃어버리면 새 키로 바꿔야 하고, 그러면 **옛 공개키가 박힌 배포본은 업데이트를 받지 못한다**.
사용자가 새 버전을 직접 내려받아 설치해야 한다. 절차는 `docs/RELEASE.md` 의 "키를 잃어버렸을 때" 를 보라.
