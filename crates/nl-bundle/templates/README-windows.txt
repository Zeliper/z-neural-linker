{{APP_NAME}} {{APP_VERSION}}
Neural Linker 로 만든 배포판입니다 (Windows x86_64).

바로 실행
  {{APP_SLUG}}.exe 를 더블클릭하거나 명령 프롬프트에서 실행합니다.
  설치가 필요 없는 단일 실행 파일이라 폴더째 옮겨도 그대로 동작합니다.

실행 옵션
  {{APP_SLUG}}.exe --version            버전 출력
  {{APP_SLUG}}.exe --headless           GUI 없이 파이프라인만 실행 (Ctrl+C 로 종료)
  {{APP_SLUG}}.exe --device cpu         장치 지정 (cpu | gpu:0 | auto)
  {{APP_SLUG}}.exe 다른앱.nlapp         다른 번들 파일 실행

모델 가중치는 실행할 때 임시 폴더에 풀렸다가 종료 시 지워집니다.
