# Sculith

Rust로 개발하는 Bedrock 기반 Minecraft 서버 구동기입니다.
Java와 Bedrock이 하나의 월드와 리소스, 게임 로직을 사용하는 것을 목표로 합니다.

현재 로컬 접속, 제한된 크리에이티브 건축, Rust 플러그인과 일부 리소스 변환을 구현했습니다.
아직 개발 중이며 설치용 패키지는 배포하지 않습니다.

## 빌드와 실행

Rust를 설치한 뒤 저장소 루트에서 실행합니다.

```sh
cargo build --workspace --locked
cargo run -p sculith-server -- run --config examples/crossplay-preview.toml
```

같은 PC에서 Java 26.3은 `127.0.0.1:25565`, Bedrock은 `127.0.0.1:19132`로 접속합니다.
이 설정은 로컬 테스트용입니다. 공식 Windows Bedrock 앱의 입장은 아직 직접 검증하지 못했습니다.

플러그인 예제는 [examples](examples), 구현 문서는 [docs](docs)에 있습니다.
라이선스는 [MIT](LICENSE)입니다.
