# Rust 플러그인 SDK v3 — 실행 파일 방식의 초기 게임/UI API

현재 SDK는 생명주기·공통 MOTD에 더해 초기화가 확인된 플레이어의 접속/종료와 승인된 블록 변경을 받습니다.
`SendMessage`는 월드 소유자가 접속 상태와 호출량을 검사한 뒤 한 플레이어에게만 보냅니다.
Java와 베드락은 같은 게임 이벤트/API를 사용합니다. [순정 Java 실제 화면과 검증](verification/2026-11-16-plugin-api-v2.md).
v3는 공통 `SimpleForm`과 소유자에게만 돌아오는 선택/취소 응답을 제공합니다.
베드락 기본 폼과 순정 Java 대화상자에서 같은 메뉴를 사용합니다. [공유 폼 예제·제한·실제 화면](forms.md).
Master Engineering Directive에 따라 공식 호스트는 WASI Component Model을 우선 후보로 검토합니다.
현재 구현이 WASI 샌드박스, region scheduler, Player/World API를 제공한다고 표시하지 않습니다.
플러그인은 신뢰된 별도 실행 파일이며, `.dll`/`.so` 로딩이나 OS 샌드박스를 제공하지 않습니다.

## 예제 빌드와 등록

```sh
cargo build -p sculith-welcome --locked
cargo run --locked -- run --config examples/with-plugin.toml
```

기존 `welcome`은 생명주기/MOTD 예제입니다. 새 게임 예제는 다음과 같습니다.

```sh
cargo build --workspace --locked
cargo run --locked -- run --config examples/gameplay-preview.toml
```

이 소스 예제 설정은 Linux/macOS 경로입니다. Windows에서 직접 빌드했다면 플러그인 경로에 `.exe`를 붙이세요.
공개 Preview 12에 동봉한 `plugins/sculith-gameplay-plugin.exe`는 별도 Rust 설치 없이 실행합니다.
데모는 설정한 MOTD를 유지하고, 접속자에게 환영 메시지를 보낸 뒤 승인된 블록 변경의 리비전을 로그에 기록합니다.

외부 Rust 프로젝트는 현재 `sculith-plugin-api`를 로컬 경로 의존성으로 사용할 수 있습니다.
아직 crates.io 패키지를 게시하지 않았습니다.

```toml
[dependencies]
sculith-plugin-api = { path = "../Sculith/crates/sculith-plugin-api" }
```

```rust
use sculith_plugin_api::{Action, Event, Hook, Metadata, Plugin, serve};

struct Example;

impl Plugin for Example {
    fn metadata(&self) -> Metadata {
        Metadata {
            name: "example".into(),
            version: "0.1.0".into(),
            hooks: vec![Hook::Started],
        }
    }

    fn on_event(&mut self, event: Event) -> Vec<Action> {
        match event {
            Event::Started { .. } => vec![Action::SetMotd {
                motd: "Hello Java and Bedrock / discovery only".into(),
            }],
            _ => vec![],
        }
    }
}

fn main() -> std::io::Result<()> {
    serve(Example)
}
```

설정 파일에 실행 파일을 등록합니다. 경로는 **설정 파일 기준**입니다.
플러그인의 작업 디렉터리는 서버가 실행된 디렉터리를 상속합니다.

```toml
[[plugins]]
path = "plugins/my-plugin"
args = []
timeout_ms = 1000
```

`check` 명령은 경로와 실행 가능 여부까지만 확인하고 플러그인을 실행하지 않습니다.
실제 API 핸드셰이크는 `run`에서 확인합니다. 최초 로드 실패는 서버 시작을 실패시킵니다.

## 제공되는 훅과 액션

| 훅 | 시점 |
| --- | --- |
| `Started` | 네트워크 소켓을 확보하고 플러그인 핸드셰이크를 마친 뒤 |
| `Tick` | `server.tick_rate`에 따른 이벤트 시계. 첫 틱부터 발생. 고정 물리 시뮬레이션이 아님 |
| `Stopping` | Ctrl+C, SIGTERM 또는 리스너 오류로 정상 종료 절차에 들어갈 때 |
| `PlayerJoined { player }` | 올바른 초기화 확인 이후. `id`, `name`, `edition` (`java`/`bedrock`) 제공 |
| `PlayerLeft { player_id }` | 초기화됐던 플레이어의 세션 정리 이후 |
| `BlocksChanged { player_id, revision, changes }` | 소유자가 실제로 승인한 변경 이후. 좌표와 `before`/`after` 이름 제공 |
| `FormSubmitted { player_id, request_id, selection }` | 폼을 연 해당 플러그인만 수신. 선택지 인덱스 또는 취소 `None`. API v3 필요 |

메타데이터의 `hooks`에 구독한 이벤트만 전송합니다. 같은 훅을 중복 구독할 수 없습니다.
플러그인 이름은 1~64자의 영문·숫자·`_.-`이고 서버 안에서 유일해야 합니다.
콜백 요청은 동시에 기다리고, 응답 액션은 서버 설정 순서대로 적용합니다.
32개 플러그인의 응답 시간을 직렬로 더하지 않습니다. 게임 소유자는 콜백 응답을 기다리지 않습니다.
블록 이름은 `minecraft:stone` 같은 공통 이름이며, Java/베드락의 패킷 ID를 플러그인에 전달하지 않습니다.
이 훅은 커밋 이후 알림이므로 취소·롤백·권한 검사 대체 용도가 아닙니다.
서버 전체 종료에서는 남은 `PlayerLeft` 콜백을 보장하지 않으므로 `Stopping` 훅으로 정리하세요.
이동 이벤트와 private 메시지는 게임 훅 큐에 넣지 않아 메시지의 재귀 호출도 막습니다.

| 액션 | 효과 |
| --- | --- |
| `SetMotd { motd }` | 공통 상태를 바꾸어 두 에디션의 서버 목록에 반영 |
| `Log { message }` | 플러그인 이름과 함께 호스트 로그에 기록 |
| `SendMessage { player_id, message }` | 초기화된 해당 플레이어에게만 시스템 메시지 전달. API v2 필요 |
| `ShowForm { player_id, request_id, form }` | 같은 단순 선택 폼을 양쪽 에디션에 표시. API v3 필요 |

MOTD는 비어 있지 않은 최대 192 UTF-8 바이트이며 제어문자와 `;`를 포함할 수 없습니다.
로그 메시지는 최대 2048 바이트이며 제어문자를 포함할 수 없습니다.
한 응답의 모든 액션을 먼저 검증하며, 하나라도 잘못되면 그 응답의 액션을 적용하지 않습니다.
메시지는 비어 있지 않은 최대 512 UTF-8 바이트이며 제어문자를 허용하지 않습니다.
소유자가 플레이어별로 초당 4개/순간 8개까지 허용합니다. 접속 종료나 호출량 제한에 따른 메시지 거절은
로그에 남기며, 그 자체로 플러그인을 비활성화하지 않습니다. 메시지는 블록 리비전을 바꾸지 않습니다.
플레이어 `id`는 현재 서버 프로세스의 세션 핸들입니다. 재시작을 넘겨 저장하거나 재사용하지 마세요.

## 통신과 종료

- `stdin`/`stdout`의 JSON-lines 요청·응답이며 최신 API는 `3`, 기존 게임 API는 `2`, 생명주기는 `1`입니다.
- 호스트는 `Hello { api_version: 1, supported_versions: [1,2,3], ... }`를 보냅니다.
  기존 v1 SDK는 추가 필드를 무시하고 그대로 동작합니다. SDK는 공통으로 지원하는 가장 높은 버전을 선택합니다.
  새 SDK의 기존 게임 훅은 v2 호스트에서도 동작하고, 폼 훅은 v3가 없는 호스트에서 로드를 거부합니다.
- 협상한 버전보다 높은 훅/액션은 거부합니다. 보관한 실제 v1·v2 Rust 실행 파일도 새 호스트에서 확인했습니다.
- 각 응답은 요청과 같은 `id`를 반환해야 합니다.
- `stdout`은 프로토콜 전용입니다. 디버그 출력은 `eprintln!` 등으로 `stderr`에 기록하세요.
- 메시지는 줄바꿈을 포함해 최대 64 KiB, 한 응답의 액션은 최대 32개입니다.
- 핸드셰이크와 이벤트 응답 전체에 `timeout_ms`를 적용합니다.
- 실행 중 시간 초과·잘못된 JSON·불일치하는 ID·잘못된 액션이 발생하면 플러그인을 종료하고 비활성화합니다.
- 게임 알림 큐는 128개입니다. 콜백이 느려 이벤트를 놓치면 게임 훅 구독자를 명시적으로 비활성화하고 기록합니다.
  누락된 변경을 정상 처리했다고 조용히 계속하지 않습니다. 생명주기 전용 플러그인은 이 정책의 대상이 아닙니다.
- 정상 종료 시 `Stopping`을 전송한 뒤 stdin을 닫습니다. SDK는 EOF에서 종료합니다.
  500ms 안에 종료하지 않는 프로세스는 종료시킵니다. 강제 종료나 머신 장애 시 훅 실행을 보장하지 않습니다.

플러그인은 서버 사용자와 같은 OS 권한을 가집니다. 호스트의 API 검증은 시스템 접근을 제한하는 기능이 아닙니다.

현재는 명령 등록, 사전 취소 훅, 일반 World/Entity/Inventory API, region scheduler,
WASI Component 샌드박스와 fuel/memory 제한을 제공하지 않습니다.
