# Sculith

[English](README.md) · [기여 안내](CONTRIBUTING.md)

**Rust로 만드는 베드락 기준 Minecraft 크로스플레이 구동기.**

목표는 서버 하나에 최신 Bedrock과 최신에 가까운 순정 Java 클라이언트가 함께 접속하고,
베드락을 원본으로 정의한 콘텐츠와 게임 규칙을 공유하는 것입니다.
순정 Java 호환 모드는 모드를 요구하지 않으며, 선택적인 Fabric Companion은 높은 표현 충실도를 목표로 합니다.
두 모드의 게임 접속과 콘텐츠 호환성은 아직 구현 중입니다.
서버와 공식 플러그인 SDK의 개발 언어는 Rust입니다.

> 기본 실행은 **`discovery-only`**입니다. 선택적으로 베드락의 암호화 로그인·평지 스폰·제한된 크리에이티브 건축을
> 시험할 수 있습니다. **공식 게임 클라이언트의 입장 검증과 실제 Java–Bedrock 크로스플레이는 아직 미완료입니다.**

## 현재 동작하는 기능

| 기능 | 상태 |
| --- | --- |
| 하나의 Rust 서버에서 Java TCP + Bedrock UDP 리스너 실행 | 구현 |
| Java 서버 목록 상태 조회 / 핑 응답 | 구현 |
| Bedrock RakNet unconnected ping / pong | 구현 |
| Bedrock RakNet 세션·압축·서명 검증·암호화 | 선택적 개발 경로 구현. 실제 UDP의 헤드리스 클라이언트로 검증 |
| Bedrock 평지 25청크 전송·스폰 확인·세션 유지/정리 | 개발 경로 구현. 공식 게임 클라이언트 검증은 미완료 |
| 권위 있는 이동·블록 설치/파괴와 세션 간 변경 전송 | 제한된 크리에이티브 경로 구현. 충돌·속도·거리·시야·권한·서버 보유 아이템 검사 |
| 단일 지역 소유권·리비전·불변 스냅샷·팔레트 저장 | 구현. 병렬 지역 틱이나 완성된 물리 엔진은 아님 |
| 버전·체크섬을 검증하는 region snapshot 파일 저장·복구 | 선택적 시작 시 로드·정상 종료 시 저장 연결. 주기적 자동 저장은 미구현 |
| 두 에디션이 읽는 공통 MOTD와 플레이어 수 | 구현. 베드락 초기화가 확인된 세션만 집계 |
| Rust 플러그인 실행 파일 로드 / API 버전 확인 / 생명주기 훅 | 초기 구현. WASI Component 공식 호스트는 검토·구현 대상 |
| 플러그인에서 공통 MOTD 변경 | 구현 |
| 베드락 PNG → 명시적으로 대응시킨 자바 텍스처 팩 변환 | 구현 |
| 온라인 계정 인증·정식 월드 입장 | 구현/검증 진행 중. 공식 discovery 조회 성공 확인, 실제 계정 로그인은 미검증 |
| 멀티플레이 엔티티 표시·일반 인벤토리 거래·물리/중력 | 미구현 |
| NetherNet 전송 | 미구현. RakNet 테스트 결과를 대체 검증으로 사용하지 않음 |
| 게임 이벤트 플러그인 API / 팩 자동 배포 | 미구현 |
| 베드락 모델·UI·스크립트의 자바 변환 | 미구현. 순정 클라이언트 제약에 따른 별도 설계 필요 |

Java 서버 목록에는 프로토콜 `-1`과 개발 단계 문구를 반환하여 플레이 가능한 서버로 광고하지 않습니다.
Java 로그인은 미구현 안내와 함께 종료합니다. Bedrock 세션은 아래의 선택적 개발 설정에서만 처리합니다.

## Windows에서 바로 테스트

1. [테스트 배포](https://github.com/SKYRPG8957/Sculith/releases/tag/v0.1.0-preview.2)에서 `windows-x86_64.zip`을 받습니다.
2. ZIP 전체를 압축 해제하고 **`start.cmd`를 더블클릭**합니다. Rust·Python·Java 설치는 필요 없습니다.
3. 같은 PC의 베드락에서 서버 주소 **`127.0.0.1`**, 포트 **`19132`**로 추가합니다.
4. 종료할 때 서버 창에서 **Ctrl+C**를 누르면 월드를 저장합니다.

기존 `sculith.toml`과 `world/region.sculith`는 보존합니다. 로컬 테스트 신원을 허용하는 개발 프리뷰이며,
**공식 게임 클라이언트 접속·렌더링은 아직 미검증, Java 월드 입장은 미구현**입니다.
설치 명령과 Windows loopback 문제는 [Windows 빠른 시작](docs/windows-quickstart.md)을 참고하세요.
Windows GDK `1.26.63`의 첫 접속 시도에서 `Terracotta / InitialConnection-149`가 보고되어 조사 중입니다.
Preview 2에는 초기 연결을 기록하는 `diagnose.cmd`가 추가됐으며, 해당 오류의 해결을 확인한 버전은 아닙니다.

## 소스에서 빌드와 실행

필요 도구는 Rust (`rust-toolchain.toml`에 고정), C 링커, 스모크 테스트용 Python 3입니다.
외부 Minecraft 서버 프로세스, Java 런타임, 사용자 제공 Microsoft 비밀키는 이 단계에서 필요하지 않습니다.
온라인 베드락 인증 경로는 공식 discovery/OIDC 서비스의 공개키를 HTTPS로 조회해야 합니다.

```sh
git clone https://github.com/SKYRPG8957/Sculith.git
cd Sculith
# 이 클라우드에 설치된 Rust를 현재 셸에서 활성화. 일반 Rust 설치 환경에서는 생략 가능.
. ./scripts/dev-env.sh

cargo build --workspace --locked
cargo run --locked -- check --config sculith.example.toml
cargo run --locked -- run --config sculith.example.toml
```

기본 주소는 Java `127.0.0.1:25565/TCP`, Bedrock `127.0.0.1:19132/UDP`입니다.
하나의 프로세스에서 두 포트를 엽니다. 로컬 개발용으로 시작하며, 외부 노출은 설정의 `bind`를 바꿔야 합니다.
TCP와 UDP는 같은 포트 번호를 사용할 수도 있습니다. 종료는 Ctrl+C입니다.
현재 공개 서버 운영용 빌드가 아닙니다.

별도 설정이 필요하면 `sculith.example.toml`을 `sculith.toml`로 복사해 수정하세요.
인자 없이 실행하면 작업 디렉터리의 `sculith.toml`을 읽고, 그 파일이 없으면 기본값을 사용합니다.
명시한 설정 파일이 없거나 잘못되었으면 오류로 종료하며, 설정 파일을 자동으로 덮어쓰지 않습니다.

## 베드락 스폰 개발 경로

```sh
cargo run --locked -- run --config examples/bedrock-login-preview.toml
```

이 예제는 loopback에서 테스트용 자체 서명 신원을 허용하고, 빈 팩 협상 이후 25개의 평지 청크를 보냅니다.
월드는 단일 소유자에게 스냅샷을 요청하는 방식이며, 초기화가 확인된 세션의 수명과 접속자 수를 관리합니다.
**실제 Microsoft 계정 인증 또는 공식 Bedrock 클라이언트 렌더링을 검증한 예제가 아닙니다.**
제한된 Creative 설정으로 Stone·Dirt·Grass 초기 슬롯을 제공합니다. 이동·블록 입력은 서버의 단일 월드 소유자가
검증하고, 승인된 블록 변경만 관심 청크를 보는 세션에 전송합니다. 일반 인벤토리 거래나 완전한 물리 동작은 아직 없습니다.

이 예제는 `world/region.sculith`를 시작 시 로드하고, Ctrl+C/SIGTERM으로 정상 종료할 때 저장합니다.
다른 설정에서는 `[world]`의 `snapshot_path`를 지정하면 되며, 상대 경로는 설정 파일의 디렉터리 기준입니다.
주기적 자동 저장은 아직 없으므로 강제 종료하면 시작 이후의 변경이 유실될 수 있습니다.
다른 형식이나 손상된 기존 파일이 있으면 덮어쓰지 않고 오류로 종료합니다.

`bedrock.require_authentication`의 기본값은 `true`입니다. 인증을 끄면 loopback 바인딩만 허용합니다.
최대 64세션, IP당 4세션으로 제한하고, 메시지·압축 해제 배치·각 RakNet 신뢰성 큐는 최대 4 MiB입니다.
앱의 전체 수신/송신 대기열에는 각각 16 MiB의 바이트 예산을 적용합니다. 이는 프로세스 전체 RAM 상한은 아닙니다.
정식 온라인 신원을 쓰는 클라이언트는 공식 공개키 조회가 성공해야 합니다.
현재 환경의 제한과 검증 결과는 [개발 상태](docs/status.md)에 기록합니다.

## Rust 플러그인

```sh
cargo build --workspace --locked
cargo run --locked -- run --config examples/with-plugin.toml
```

예제 플러그인은 시작 시 공통 MOTD를 변경합니다. 두 접속부에 같은 변경이 반영됩니다.
플러그인은 별도 Rust 실행 파일이며 TOML의 `[[plugins]]`에 경로를 등록합니다.
Rust의 불안정한 동적 라이브러리 ABI 대신 버전이 명시된 JSON-lines 메시지를 사용합니다.
플러그인은 호스트와 같은 사용자 권한을 가진 **신뢰된 프로그램**이며 샌드박스가 아닙니다.
공식 플러그인 구조는 WASI Component Model과 비교·검증할 예정이며 현재 실행 파일 방식은 초기 구현입니다.
자세한 개발 방법은 [플러그인 문서](docs/plugins.md)를 참고하세요.

## 베드락 팩 → 자바 텍스처 팩

순정 Java는 베드락의 `manifest.json`, 모델, UI, 동작 스크립트를 그대로 해석하지 못합니다.
베드락 팩을 원본으로 유지하고, 에디션별 대응표로 자바 팩을 생성하는 방향입니다.
현재 변환기는 **지정한 PNG를 기존 자바 블록·아이템의 텍스처로 대응시키는 단계**입니다.

저장소의 예제는 Minecraft 에셋을 포함하지 않는 자체 제작 체크무늬 텍스처로 돌의 외형을 바꿉니다.
다음 명령은 Java `26.3`의 팩 형식 `97.1`을 대상으로 합니다.

```sh
mkdir -p packs/generated
cargo run --locked -- pack convert \
  --input examples/pack/bedrock \
  --mapping examples/pack/textures.json \
  --output packs/generated/java-demo \
  --pack-format 97 --pack-format-minor 1
```

출력은 `pack.mcmeta`, `assets/`, 변환 범위가 적힌 `sculith-conversion.json`을 포함하는 디렉터리입니다.
이미 있는 출력은 덮어쓰지 않으므로 재실행할 때 새 출력 경로를 지정하세요.
이 디렉터리는 자바 클라이언트의 리소스팩 폴더에 수동으로 넣을 수 있습니다.
서버의 팩 배포·적용 협상과 실제 클라이언트 렌더링 검증은 아직 구현하지 않았습니다.
변환하지 않은 자산은 결과에 포함되지 않으며, 변환 결과에 제한 사항을 기록합니다.

대응표의 `bedrock`은 원본 팩 내부 PNG 경로이고, `java`는 확장자를 제외한 자바 리소스 ID입니다.
클라이언트에 실제로 존재하는 ID를 사용해야 합니다. 새로운 ID를 적는 것만으로 커스텀 블록이 등록되지는 않습니다.

## 버전 정책

- Bedrock은 최신 정식 프로토콜을 우선 구현·검증하는 것을 목표로 합니다.
- Java는 최신 정식 버전 중심으로, 변환 계층이 검증된 인접 버전까지 지원하는 것을 목표로 합니다.
- 버전 번호만 바꾸거나 클라이언트 프로토콜 번호를 그대로 광고해서 지원한다고 처리하지 않습니다.
- 실제 지원 목록은 로그인·인증·청크·이동·인벤토리·팩 적용 검증을 통과한 뒤 추가합니다.

`protocol-targets.json`은 2026-11-16에 재확인한 Java `26.3 / 777`, Bedrock `1.26.60 / 2225`를
**개발 참고값**으로 기록합니다. 현재 `gameplay_supported_versions`는 두 에디션 모두 빈 목록입니다.
실행 시 자동으로 프로토콜 정의를 교체하지 않습니다.

```sh
cargo run --locked -- versions
python3 scripts/check_protocol_updates.py  # GitHub 최신 정식 메타데이터와 비교. 파일은 변경하지 않음.
```

## 검증

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo build --workspace --locked
python3 scripts/smoke.py
python3 scripts/smoke.py --sessions
```

스모크 테스트는 임시 설정과 사용 가능한 로컬 포트로 실제 서버·Rust 플러그인을 실행합니다.
Java 상태 조회·핑·로그인 거부, Bedrock pong, 공통 플러그인 상태, 잘못된 패킷,
정상 종료, 설정 검증, 스냅샷 저장·재시작 복구·기존 파일 보호, PNG 팩 변환과 기존 출력 보호를 확인합니다.
공식 게임 클라이언트의 월드 접속 테스트를 대체하지 않습니다.
동일한 절차를 GitHub Actions에 등록했습니다.

설계와 단계별 개발 범위는 [아키텍처](docs/architecture.md)와 [로드맵](docs/roadmap.md)에 있습니다.

MIT License. Mojang 또는 Microsoft의 공식 프로젝트가 아닙니다.
