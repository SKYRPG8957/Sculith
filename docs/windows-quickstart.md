# Windows에서 실행

현재 설치용 패키지는 배포하지 않습니다. Rust와 MSVC 빌드 도구가 필요합니다.
저장소 루트에서 다음 명령을 실행합니다.

```powershell
cargo build --workspace --locked
cargo run -p sculith-server -- run --config examples/crossplay-preview.toml
```

같은 PC의 Java 26.3은 `127.0.0.1:25565`, Bedrock은 `127.0.0.1:19132`를 사용합니다.
종료할 때 Ctrl+C를 누르면 월드를 저장합니다. `world` 폴더는 유지하세요.
공식 Windows Bedrock 앱의 실제 입장은 아직 직접 검증하지 못했습니다.
