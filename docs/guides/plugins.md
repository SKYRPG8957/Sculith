# Rust 플러그인 SDK v8

현재 SDK는 생명주기·공통 MOTD에 더해 초기화가 확인된 플레이어의 접속/종료와 승인된 블록 변경을 받습니다.
`SendMessage`는 월드 소유자가 접속 상태와 호출량을 검사한 뒤 한 플레이어에게만 보냅니다.
Java와 베드락은 같은 게임 이벤트/API를 사용합니다. [순정 Java 실제 화면과 검증](../development/verification/2026-11-16-plugin-api-v2.md).
v3는 공통 `SimpleForm`과 소유자에게만 돌아오는 선택/취소 응답을 제공합니다.
베드락 기본 폼과 순정 Java 대화상자에서 같은 메뉴를 사용합니다. [공유 폼 예제·제한·실제 화면](forms.md).
v4는 등록된 공통 팩 음원을 개인 재생하는 `PlaySound`를 제공합니다. [원본·매핑·검증](sounds.md).
v5는 등록된 정적 모델을 개인 표시·교체·숨기는 `ShowModel`/`HideModel`을 제공합니다. [범위·소유권·검증](models.md).
v6는 패킷 수신 메타데이터, 승인된 이동·장비·인벤토리 변경·공개 채팅, 플러그인 명령어 등록을 제공합니다. 공개 배포 패키지는 만들지 않습니다.
v7는 인벤토리 조회·아이템 지급과 요청한 플러그인에만 돌아오는 결과를 제공합니다.
v8는 입장 전부터 종료까지의 연결 수명주기와 수신 패킷 메타데이터를 제공합니다.
Master Engineering Directive에 따라 공식 호스트는 WASI Component Model을 우선 후보로 검토합니다.
현재 구현이 WASI 샌드박스, region scheduler, Player/World API를 제공한다고 표시하지 않습니다.
플러그인은 신뢰된 별도 실행 파일이며, `.dll`/`.so` 로딩이나 OS 샌드박스를 제공하지 않습니다.

## 예제 빌드와 등록

```sh
cargo build -p sculith-welcome --locked
cargo run --locked -- run --config examples/configs/welcome.toml
```

기존 `welcome`은 생명주기/MOTD 예제입니다. 새 게임 예제는 다음과 같습니다.

```sh
cargo build --workspace --locked
cargo run --locked -- run --config examples/configs/gameplay.toml
```

이 소스 예제 설정은 Linux/macOS 경로입니다. Windows에서 직접 빌드했다면 플러그인 경로에 `.exe`를 붙이세요.
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
| `PlayerMoved { player_id, tick, x, y, z, yaw, pitch, head_yaw }` | 승인돼 실제로 바뀐 위치·회전. `x/y/z`는 공통 발 좌표. API v6 필요 |
| `PlayerEquipped { player_id, slot, item, count }` | 선택 슬롯이나 손에 든 아이템 종류·수량이 바뀐 뒤. API v6 필요 |
| `PlayerInventoryChanged { player_id, changes }` | 커밋된 슬롯별 컨테이너·인덱스·아이템·수량. 재전송·실패·변경 없는 요청은 제외. API v6 필요 |
| `InventoryResponse { player_id, request_id, result }` | 인벤토리 요청을 보낸 플러그인만 받는 조회·지급 결과. API v7 필요 |
| `PlayerChat { player_id, name, message }` | 초기화된 세션에서 검사한 공개 채팅. 작성자는 서버 프로필 기준. API v6 필요 |
| `PacketReceived { player_id, packet }` | 초기화 완료 후 play 패킷의 에디션·버전·ID·이름·처리 분류·수신 순번. 본문과 계정 정보는 제외. API v6 필요 |
| `Connection { event }` | 연결 열림·단계별 패킷 수신·활성 플레이어 연결·종료. API v8 필요 |
| `Connection { event: PacketSent { .. } }` | 전송 계층이 패킷 전체를 받아들인 뒤의 메타데이터. `Hook::PacketSent`와 `Hook::Connection` 구독, API v9 필요 |
| `CommandSubmitted { player_id, name, command, arguments }` | 명령어를 등록한 플러그인만 수신. 인자는 공개 채팅이나 다른 플러그인에 전달하지 않음. API v6 필요 |

메타데이터의 `hooks`에 구독한 이벤트만 전송합니다. 같은 훅을 중복 구독할 수 없습니다.
플러그인 이름은 1~64자의 영문·숫자·`_.-`이고 서버 안에서 유일해야 합니다.
콜백 요청은 동시에 기다리고, 응답 액션은 서버 설정 순서대로 적용합니다.
32개 플러그인의 응답 시간을 직렬로 더하지 않습니다. 게임 소유자는 콜백 응답을 기다리지 않습니다.
블록 이름은 `minecraft:stone` 같은 공통 이름입니다. 패킷 번호는 별도로 구독한 `PacketReceived`와 `Connection`에서 제공합니다.
게임 훅은 커밋 이후 알림이므로 취소·롤백·권한 검사 대체 용도가 아닙니다. `PacketReceived`는 처리 전 수신 기록이며 본문 검사 성공을 의미하지 않습니다.
서버 전체 종료에서는 남은 `PlayerLeft` 콜백을 보장하지 않으므로 `Stopping` 훅으로 정리하세요.
이동·장비·인벤토리·패킷 관측은 구독자가 있을 때만 게임 훅 큐에 넣습니다. 개인 메시지와 표시 효과는 다시 게임 훅을 발생시키지 않습니다.

인벤토리는 현재 크리에이티브 건축용 36칸과 커서·생성 결과 슬롯을 다룹니다.
Stone/Dirt/Grass/Bedrock만 지원하며 한 스택은 최대 64개입니다.
슬롯 변경 훅의 `container`는 `inventory`, `cursor`, `created_output`이고, 빈 슬롯은 `item: null`, `count: 0`입니다.
베드락 예측용 스택 ID는 SDK에 노출하지 않습니다. 순정 Java 크리에이티브 화면의 커서는 클라이언트가 관리하며,
서버에는 해당 화면이 제출한 슬롯 교체만 전달됩니다. 일반 클릭 요청과 베드락 거래는 소유자의 커서 상태로 계산합니다.
아이템 삭제·임의 슬롯 교체, 일반 상자, 갑옷·보조 손, 드롭·제작·서바이벌은 아직 제공하지 않습니다.

| 액션 | 효과 |
| --- | --- |
| `SetMotd { motd }` | 공통 상태를 바꾸어 두 에디션의 서버 목록에 반영 |
| `Log { message }` | 플러그인 이름과 함께 호스트 로그에 기록 |
| `SendMessage { player_id, message }` | 초기화된 해당 플레이어에게만 시스템 메시지 전달. API v2 필요 |
| `ShowForm { player_id, request_id, form }` | 같은 단순 선택 폼을 양쪽 에디션에 표시. API v3 필요 |
| `PlaySound { player_id, sound, volume, pitch }` | 등록된 팩 음원을 해당 플레이어에게만 재생. API v4 필요 |
| `ShowModel { player_id, model }` / `HideModel { player_id }` | 등록된 정적 모델의 개인 미리보기 표시·교체·숨김. API v5 필요 |
| `RegisterCommand { command: CommandSpec { name, description } }` | `Started`에서 공개 플레이어 명령 등록. `CommandSubmitted` 구독 필수. API v6 필요 |
| `Inventory { player_id, request_id, operation }` | 소유자를 통한 인벤토리 조회·지급. `InventoryResponse` 구독 필수. API v7 필요 |

MOTD는 비어 있지 않은 최대 192 UTF-8 바이트이며 제어문자와 `;`를 포함할 수 없습니다.
로그 메시지는 최대 2048 바이트이며 제어문자를 포함할 수 없습니다.
한 응답의 모든 액션을 먼저 검증하며, 하나라도 잘못되면 그 응답의 액션을 적용하지 않습니다.
메시지는 비어 있지 않은 최대 512 UTF-8 바이트이며 제어문자를 허용하지 않습니다.
소유자가 플레이어별로 초당 4개/순간 8개까지 허용합니다. 접속 종료나 호출량 제한에 따른 메시지 거절은
로그에 남기며, 그 자체로 플러그인을 비활성화하지 않습니다. 메시지는 블록 리비전을 바꾸지 않습니다.
플레이어 `id`는 현재 서버 프로세스의 세션 핸들입니다. 재시작을 넘겨 저장하거나 재사용하지 마세요.

## 인벤토리 API

```rust
use sculith_plugin_api::{Action, InventoryOperation};

Action::Inventory {
    player_id,
    request_id: 1,
    operation: InventoryOperation::Give {
        item: "minecraft:stone".into(),
        count: 65,
    },
}
```

`InventoryOperation::Get`은 현재 상태를 조회합니다. `Give`는 기존 스택을 먼저 채우고 빈 칸에 넣습니다.
한 요청은 1~2304개이며, 공간 부족·ID 할당 실패에서는 전체 지급을 취소합니다.
요청마다 `InventoryResult::Snapshot { inventory }` 또는 `Rejected { reason }`이 돌아옵니다.
성공 응답은 처리 직후 36칸, 선택 슬롯, 서버가 아는 커서·생성 결과를 담습니다. 네트워크 스택 ID는 보내지 않습니다.
`request_id`는 플러그인 내부의 응답 연결용 값입니다. 같은 값으로 지급을 다시 요청하면 새 지급이므로 자동 재시도 키로 사용하지 마세요.

플러그인은 서버 권한으로 지급합니다. 플레이어의 건축 권한을 변경하지 않으며, 초기화 전·종료된 세션은 거부합니다.
플레이어별 조회·지급은 합쳐서 초당 8개·순간 16개이며 클라이언트 거래 예산과 별개입니다.
플러그인별 초당 128개·순간 256개를 넘으면 해당 플러그인을 비활성화합니다.
공간 부족, 유효하지만 미지원인 아이템, 플레이어 호출량 제한은 일반 거절 응답입니다.
형식이 잘못된 액션은 응답 전체를 적용하기 전에 거부하며 기존 정책에 따라 플러그인을 비활성화합니다.

조회는 클라이언트 업데이트나 변경 훅을 만들지 않습니다. 지급은 대상 플레이어의 슬롯과 필요한 손 아이템 표시만 갱신합니다.
자바의 크리에이티브 화면에서 클라이언트가 따로 관리하는 마우스 아이템은 초기화하지 않습니다.
검증용 플러그인은 `tests/fixtures`에 있으며 일반 설정과 서버 기본 빌드에는 포함하지 않습니다.

## 명령어와 패킷 관측

`examples/plugins/events`은 수신·이동·장비·인벤토리 훅을 구독하고 `/hello`를 등록하는 Rust 예제입니다.
빌드 후 `examples/configs/events.toml`로 실행할 수 있습니다.
명령어 이름은 소문자 영문·숫자·밑줄 1~32자, 설명은 1~128 UTF-8 바이트입니다.
플러그인당 32개, 서버 전체 128개까지 시작할 때 일괄 등록하며, 중복·예약 이름 `/help`는 거부합니다.
Java 기본 명령 트리와 베드락 명령 목록에서 같은 이름과 선택적 문자열 인자를 제공합니다.
`/help [페이지]`는 서버가 처리합니다. 명령 인자는 셸에서 실행하지 않습니다.
현재 등록 명령은 모든 접속자가 호출할 수 있습니다. 관리 명령용 권한 API는 아직 없습니다.
채팅과 명령 입력은 플레이어별 초당 2개·순간 4개, 256자·1024바이트로 제한합니다.

`sculith packets`는 Java 777의 상태·방향별 식별자 260개와 베드락 2225의 식별자 244개를 출력합니다.
베드락 239개는 잠긴 Rust 의존성의 실제 패킷 타입이고, 나머지 5개는 독립 Go 풀에서 확인한 미구현 본문입니다.
등록은 전체 게임 동작 구현을 뜻하지 않습니다. `dispatch`의 `unsupported`/`unknown`도 수신 기록에 남으며 본문은 플러그인에 보내지 않습니다.
기존 `PacketReceived`는 초기화 완료 후 play 입력이며 플레이어별 순번을 유지합니다.
`Connection`은 별도 구독입니다. Java TCP의 handshake/status/login/configuration/spawn/play와,
Bedrock 전송 연결 후 network_settings/login/encryption(해당 전송만)/resource_packs/spawn/play를 관찰합니다.
RakNet 검색·재전송, NetherNet HTTP 신호와 ICE/DTLS 자체는 관찰 대상이 아닙니다.
송신 메타데이터는 API v9의 `Hook::PacketSent`로 별도 구독합니다. 기존 v8 구독에는 새 이벤트를 보내지 않습니다.

`ConnectionEvent`는 다음 종류를 제공합니다.

- `Opened { connection_id, transport }`: `java_tcp`, `bedrock_raknet`, `bedrock_nethernet` 중 하나입니다.
- `PacketReceived { connection_id, packet }`: `protocol`, `phase`, `id`, `name`, `dispatch`, `sequence`만 제공합니다.
  Java handshake의 protocol은 아직 선택되지 않아 `None`입니다. 지원하지 않는 버전의 로그인에는 알려진 버전의 패킷 이름을 붙이지 않습니다.
- `PacketSent { connection_id, packet }`: 수신과 같은 메타데이터 필드입니다. Java 프레임 전체 또는 Bedrock 배치의 전송 성공 뒤에만 발생합니다. 부분 송신·실패·취소에는 성공 이벤트를 만들지 않습니다. 클라이언트 수신·적용 확인을 뜻하지 않습니다.
- `PlayerAttached { connection_id, player_id }`: 월드 소유자가 초기화를 승인한 뒤의 연결입니다. `connection_id`는 플레이어 ID와 별개이며 프로세스 안에서 재사용하지 않습니다.
- `Closed { connection_id }`: 정상 종료·거절·시간 초과·서버 작업 취소 때 발생합니다. 머신 장애나 강제 프로세스 종료는 예외입니다.

순번은 연결별·방향별로 각각 1부터 시작하며 단계 변경 뒤에도 이어집니다. 송신 구독을 추가해도 기존 수신 순번에는 빈 번호가 생기지 않습니다.
송신 `dispatch`는 설정·로그인 단계의 `control` 또는 생성된 게임 상태 알림의 `notification`이며, 입력 승인 분류와 구별합니다.
수신은 프레이밍/디코딩이 끝난 패킷을 본문 검사 전에 관찰하므로, 훅 수신은 입력 승인이나 게임 동작 지원을 뜻하지 않습니다.
Bedrock은 디코딩된 배치 전체를 처리 전에 관찰합니다. 같은 배치의 단계는 수신 당시 상태이며, 앞 패킷이 거절돼도 뒤의 디코딩된 패킷 기록은 남습니다. 배치 수량 한도를 넘긴 입력은 배치째 거절합니다.
프레이밍·디코딩에 실패한 입력에는 패킷 훅이 없습니다. Java는 프레임과 식별자를, Bedrock은 배치 내 패킷 타입까지 디코딩한 뒤 관찰합니다. 본문·계정·프로필·IP 주소는 전달하지 않습니다.
월드 이벤트 큐와 연결 큐 사이의 상대 순서는 보장하지 않습니다. 플레이어 조작은 여전히 소유자의 활성 세션 검사를 거칩니다.
128개 연결 관측 큐가 넘치면 해당 훅 구독 플러그인을 기록 후 비활성화합니다. 네트워크 작업은 플러그인 응답을 기다리지 않습니다.
구독자가 없으면 연결별 ID·순번·패킷 이름 생성과 이벤트 전송을 건너뜁니다. 일반 설정에는 관측 플러그인이 자동으로 포함되지 않습니다.

## 통신과 종료

- `stdin`/`stdout`의 JSON-lines 요청·응답이며 최신 API는 `9`, 기존 `1`~`8`도 협상할 수 있습니다.
- 호스트는 `Hello { api_version: 1, supported_versions: [1,2,3,4,5,6,7,8,9], ... }`를 보냅니다.
  기존 v1 SDK는 추가 필드를 무시하고 그대로 동작합니다. SDK는 공통으로 지원하는 가장 높은 버전을 선택합니다.
  새 SDK의 기존 게임 훅은 v2 호스트에서도 동작하고, 폼 훅은 v3가 없는 호스트에서 로드를 거부합니다.
- 협상한 버전보다 높은 훅/액션은 거부합니다. 보관한 실제 v1~v7 Rust 실행 파일도 새 호스트에서 확인했습니다.
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

현재는 사전 취소 훅, 일반 World/Entity API, 인벤토리 삭제·임의 슬롯 교체, region scheduler,
WASI Component 샌드박스와 fuel/memory 제한을 제공하지 않습니다.
