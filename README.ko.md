# Sculith

**Fast. Light. Safe.**

Rust로 개발하는 베드락 기반 Minecraft 구동기입니다.
서버 하나에 순정 Java와 Bedrock이 접속해 같은 월드·게임 규칙·Rust 플러그인을 사용합니다.

[English](README.md) · [문서](docs/README.md) · [기여 안내](CONTRIBUTING.md)

현재 개발 단계이며 로컬 크리에이티브 건축을 지원합니다.
공개 서버 운영, 서바이벌, 전체 베드락 콘텐츠 호환은 개발 중입니다.
개발 기준 버전은 Bedrock **1.26.63 / protocol 2225**, Java **26.3 / 777**입니다.
[호환성과 검증 상태](docs/development/status.md).

## 빌드와 실행

고정된 Rust 툴체인과 C 컴파일러·링커가 필요합니다.
Windows MSVC 빌드에는 암호화 백엔드용 CMake도 필요합니다.

```sh
git clone https://github.com/SKYRPG8957/Sculith.git
cd Sculith
cargo build --locked
cargo run --locked -- run --config examples/configs/crossplay.toml
```

같은 PC에서 접속합니다.

| 클라이언트 | 주소 |
| --- | --- |
| Bedrock | `127.0.0.1`, 포트 `19132` |
| Java 26.3 | `127.0.0.1:25565` |

새 플레이어는 빈 인벤토리로 시작합니다. 지원 블록은 크리에이티브 목록에서 선택하세요.

Ctrl+C로 정상 종료하면 `world/region.sculith`에 지역을 저장합니다.
플레이어 데이터 저장과 비정상 종료 복구는 아직 없습니다.
이전 프리뷰 배포본은 철회했으며 새 바이너리는 게시하지 않습니다.
[Windows 설정](docs/guides/windows.md).
[외부 접속과 포트 설정](docs/guides/hosting.md).

## 주요 기능

- 두 에디션의 청크·플레이어 표시·이동·건축·채팅 공유
- Stone·Dirt·Grass·Bedrock의 수량을 다루는 크리에이티브 인벤토리 36칸
- 게임 이벤트·명령어·개인 메시지·폼·사운드·정적 모델을 지원하는 [Rust 플러그인 SDK](docs/guides/plugins.md)
- 하나의 베드락 원본 팩과 명시적으로 지원하는 Java 텍스처·사운드·정적 모델 변환
- 버전이 있는 지역 스냅샷과 크기가 제한된 월드 요청 대기열

일반 상자, 제작, 물리·중력, 전투와 임의의 JSON UI·Molang·Script API 처리는 개발 중입니다.

## 프로젝트 구조

| 경로 | 내용 |
| --- | --- |
| `crates/` | 엔진·프로토콜·팩 변환·플러그인 SDK |
| `examples/configs/` | 실행 가능한 서버 설정 |
| `examples/plugins/` | 공개 SDK를 사용하는 Rust 예제 플러그인 |
| `examples/pack/` | 원본 공통 리소스팩 |
| `tests/interop/` | 독립 Java·Bedrock·WebRTC 클라이언트 검사 |
| `scripts/` | 개발 도구와 프로토콜 생성기 |
| `packaging/` | 플랫폼별 실행 템플릿 |
| `docs/` | 사용 가이드·내부 설계·개발 기록 |

[크레이트 역할과 의존성](docs/internals/architecture.md).

## 개발

```sh
python3 scripts/check_architecture.py
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
python3 tests/interop/smoke.py
```

프로토콜 검사와 기여 규칙은 [기여 안내](CONTRIBUTING.md)를 참고하세요.

## 라이선스

[MIT](LICENSE)이며 상업적 이용을 허용합니다.
에스테리아 일대기를 주요 적용 서버로 사용하되, 전용 RPG 규칙은 플러그인과 팩으로 분리합니다.
[프로젝트 범위](docs/development/project-scope.md).

타 프로젝트·라이브러리의 출처는 [서드파티 고지](THIRD_PARTY_NOTICES.md)에 기록합니다.
Mojang·Microsoft와 무관한 독립 프로젝트입니다.
