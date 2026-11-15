# Rust 플러그인 SDK v1 — 초기 실행 파일 방식

현재 SDK는 서버 생명주기와 공통 MOTD를 확장합니다. 게임 이벤트는 월드 구현과 함께 추가할 예정입니다.
Master Engineering Directive에 따라 공식 호스트는 WASI Component Model을 우선 후보로 검토합니다.
현재 구현이 WASI 샌드박스, region scheduler, Player/World API를 제공한다고 표시하지 않습니다.
플러그인은 신뢰된 별도 실행 파일이며, `.dll`/`.so` 로딩이나 OS 샌드박스를 제공하지 않습니다.

## 예제 빌드와 등록

```sh
cargo build -p sculith-welcome --locked
cargo run --locked -- run --config examples/with-plugin.toml
```

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

메타데이터의 `hooks`에 구독한 이벤트만 전송합니다. 같은 훅을 중복 구독할 수 없습니다.
플러그인 이름은 1~64자의 영문·숫자·`_.-`이고 서버 안에서 유일해야 합니다.
서버 설정 순서대로 이벤트와 액션을 처리합니다.

| 액션 | 효과 |
| --- | --- |
| `SetMotd { motd }` | 공통 상태를 바꾸어 두 에디션의 서버 목록에 반영 |
| `Log { message }` | 플러그인 이름과 함께 호스트 로그에 기록 |

MOTD는 비어 있지 않은 최대 192 UTF-8 바이트이며 제어문자와 `;`를 포함할 수 없습니다.
로그 메시지는 최대 2048 바이트이며 제어문자를 포함할 수 없습니다.
한 응답의 모든 액션을 먼저 검증하며, 하나라도 잘못되면 그 응답의 액션을 적용하지 않습니다.

## 통신과 종료

- `stdin`/`stdout`의 JSON-lines 요청·응답이며 SDK의 API 버전은 `1`입니다.
- 호스트의 `Hello` → 플러그인의 `Ready` 순서로 버전·메타데이터를 확인합니다.
- 각 응답은 요청과 같은 `id`를 반환해야 합니다.
- `stdout`은 프로토콜 전용입니다. 디버그 출력은 `eprintln!` 등으로 `stderr`에 기록하세요.
- 메시지는 줄바꿈을 포함해 최대 64 KiB, 한 응답의 액션은 최대 32개입니다.
- 핸드셰이크와 이벤트 응답 전체에 `timeout_ms`를 적용합니다.
- 실행 중 시간 초과·잘못된 JSON·불일치하는 ID·잘못된 액션이 발생하면 플러그인을 종료하고 비활성화합니다.
- 정상 종료 시 `Stopping`을 전송한 뒤 stdin을 닫습니다. SDK는 EOF에서 종료합니다.
  500ms 안에 종료하지 않는 프로세스는 종료시킵니다. 강제 종료나 머신 장애 시 훅 실행을 보장하지 않습니다.

플러그인은 서버 사용자와 같은 OS 권한을 가집니다. 호스트의 API 검증은 시스템 접근을 제한하는 기능이 아닙니다.
