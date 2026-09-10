{{APP_NAME}} {{APP_VERSION}}
Neural Linker 로 만든 배포판입니다 (Linux x86_64).

바로 실행
  ./{{APP_SLUG}}

설치 (~/.local 아래, 관리자 권한 불필요)
  ./install.sh
  ./install.sh --uninstall      제거

실행 옵션
  ./{{APP_SLUG}} --version              버전 출력
  ./{{APP_SLUG}} --headless             GUI 없이 파이프라인만 실행 (Ctrl+C 로 종료)
  ./{{APP_SLUG}} --device cpu           장치 지정 (cpu | gpu:0 | auto)
  ./{{APP_SLUG}} 다른앱.nlapp           다른 번들 파일 실행

모델 가중치는 실행할 때 임시 폴더에 풀렸다가 종료 시 지워집니다.
