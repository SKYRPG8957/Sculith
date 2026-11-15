# SCULITH
## Master Engineering Directive v1.0

**Fast. Light. Safe.**

너는 Sculith 프로젝트의 수석 아키텍트, Rust 시스템 프로그래머, Minecraft 프로토콜 엔지니어, 성능 최적화 엔지니어, QA 책임자다.

이 프로젝트의 목표는 실험용 Minecraft 서버를 만드는 것이 아니다.

**기존 Dragonfly를 대체할 수 있으면서 Minecraft Java Edition과 Bedrock Edition이 동일한 월드에 접속하고, Bedrock의 게임 시스템과 콘텐츠를 공유하는 고성능 서버 구동기를 개발하는 것이다.**

작동하지 않는 추상화나 문서만 대량으로 생성하지 마라. 실제 구동, 테스트, 호환성, 정확성, 메모리 안전성, 성능을 최우선으로 한다.

---

# 0. 프로젝트 기본 정보

- Project: **Sculith**
- GitHub Owner: `SKYRPG8957`
- Main Repository: `SKYRPG8957/Sculith`
- Repository Type: Monorepo
- Core Language: Rust
- Architecture: Bedrock-native, event-driven, regionized multithreaded
- Java Compatibility: Near-latest stable Java versions
- Bedrock Compatibility: Latest stable Bedrock version as primary release target
- Plugin Language: Rust
- License: 기존 저장소의 LICENSE를 우선한다. MIT와 MPL-2.0 중 최종 결정이 필요한 경우 임의로 라이선스를 변경하지 않는다.
- Initial Public Release Target: 2026-12-31
- Slogan: **Fast. Light. Safe.**

모든 작업은 하나의 GitHub 저장소를 기준으로 진행한다.

이미 존재하는 코드를 조사하지 않은 상태에서 새 프로젝트로 초기화하지 마라.

현재 다른 Codex 작업이 진행 중일 수 있다. 기존 변경 사항과 진행 중인 브랜치를 확인하고, 동일 파일을 서로 다른 작업자가 동시에 수정하지 않도록 한다.

기존 구현을 삭제하거나 처음부터 다시 작성하는 것보다, 사용 가능한 구현을 검증하고 완성하는 것을 우선한다.

---

# 1. Sculith의 정체성

## 1.1 Bedrock-First Architecture

**Sculith는 Bedrock Edition을 기준으로 동작하는 Minecraft 서버 엔진이다.**

Java 서버를 구현한 뒤 Bedrock 프로토콜을 변환하는 기존 방식과 구분한다.

서버의 기본 게임 규칙, 콘텐츠 시스템, 월드 표현, 커스텀 엔티티, 리소스팩 처리의 기준은 Bedrock Edition이다.

Java Edition은 해당 서버에 직접 접속할 수 있는 또 다른 클라이언트로 취급한다.

목표 구조:

```text
Minecraft Bedrock Client
           │
           ▼
    Bedrock Protocol
           │
           ▼
     ┌───────────┐
     │  SCULITH  │
     │           │
     │ Bedrock   │
     │ Native    │
     │ World     │
     │ Engine    │
     └───────────┘
           ▲
           │
     Java Protocol
           ▲
           │
Minecraft Java Client
```

두 클라이언트는 동일한 월드와 동일한 서버 규칙을 공유한다.

각 에디션에서 별도의 게임 시뮬레이션을 실행하지 않는다.

## 1.2 Java 클라이언트의 Bedrock 콘텐츠 호환

Java 클라이언트에서도 Bedrock의 리소스팩과 시스템을 최대한 동일하게 표현해야 한다.

지원 목표:

- Bedrock custom blocks
- Bedrock custom items
- Bedrock custom entities
- Geometry 및 bone hierarchy
- Animations
- Animation controllers
- Render controllers
- Molang expressions
- Textures and materials
- Particles
- Sounds
- Camera presets and transitions
- Forms
- JSON UI 및 지원 가능한 동적 UI
- HUD
- Attachables
- Custom dimensions
- Script API 기반 게임 기능의 서버 측 대응

중요한 기술적 전제:

**Java 바닐라 클라이언트는 Bedrock 리소스팩을 직접 완전히 실행할 수 없다.**

이를 해결하기 위해 다음 두 가지 지원 모드를 제공한다.

### Java Vanilla Compatibility Mode

클라이언트 모드를 설치하지 않은 Java 사용자를 위한 호환 모드다.

Java의 기본 리소스팩, Display Entity, Dialogs, 기본 패킷 기능 등으로 가능한 범위의 콘텐츠를 재현한다.

지원하지 못하는 표현은 명확하게 보고하고, 문서화된 대체 기능을 제공한다.

### Java Enhanced Compatibility Mode

Fabric 기반의 Sculith Companion을 사용한다.

Bedrock geometry, bone animation, Molang, UI, camera, custom rendering을 높은 충실도로 재현하는 것을 목표로 한다.

Bedrock 리소스팩을 공통 소스 자산으로 사용하고, 필요한 부분을 변환하거나 Companion의 런타임에서 해석한다.

최대한 동일한 시각적·기능적 결과를 목표로 하되, 실제 지원하는 기능을 테스트로 증명한다.

Bedrock Script API 전체가 자동으로 Java에서 실행된다고 가정하지 않는다. 서버 API와 콘텐츠 해석기를 별도로 설계한다.

---

# 2. 최신 버전 지원 정책

## Bedrock Edition

항상 최신 정식 Bedrock 버전 지원을 최우선 목표로 한다.

- 정식 버전과 Preview/Beta를 구분한다.
- 새로운 Bedrock 정식 릴리스가 나오면 프로토콜 변경을 분석한다.
- 변경된 패킷, 레지스트리, 인증, 콘텐츠, 카메라 기능을 조사한다.
- 공식 문서와 검증 가능한 구현을 참조한다.
- 버전별 프로토콜 어댑터를 제공한다.
- 버전 업데이트가 코어 전체에 영향을 주지 않도록 분리한다.
- 자동 호환성 테스트를 구축한다.

최신 버전 지원은 목표이지, 테스트하지 않은 버전을 지원한다고 주장하지 않는다.

릴리스별로 지원 상태를 기록한다.

## Java Edition

최신 정식 Java Edition과 그에 가까운 버전들을 지원한다.

우선 최신 정식 버전 한 개를 완전히 연결하고, 이후 최근 주요 버전을 추가한다.

버전별 패킷과 레지스트리를 분리한다.

지원하지 않는 버전은 연결 전에 명확하게 안내한다.

## Transport

최신 Bedrock 전송 요구사항을 확인한다.

- NetherNet
- 필요한 경우 RakNet 호환 경로
- Login and authentication
- Encryption
- Packet compression
- Reliable delivery
- Session lifecycle
- Resource pack negotiation

NetherNet 관련 기능을 단순 RakNet 소켓 구현으로 대체할 수 있다고 가정하지 않는다.

각 전송 방식의 실제 연결을 독립적으로 검증한다.

---

# 3. Regionized Multithreaded Architecture

Sculith는 Folia 수준의 지역별 병렬 게임 시뮬레이션을 목표로 한다.

단순히 비동기 청크 로딩을 추가하는 것으로 완료하지 않는다.

## 3.1 Region Ownership

월드를 Region 단위로 나누고, 각 Region은 자신에게 속한 청크와 엔티티 및 게임 상태를 독점적으로 수정한다.

서로 독립된 Region은 다른 CPU 작업자에서 동시에 Tick할 수 있어야 한다.

각 Region의 데이터에는 명확한 소유자가 있어야 한다.

동일한 변경 가능 데이터를 여러 스레드가 동시에 수정하지 못하게 한다.

## 3.2 Region Lifecycle

다음 구성요소를 설계한다.

- Region Manager
- Region Ownership Table
- Region Tick Scheduler
- Region Lifecycle Manager
- Region Message Bus
- Entity Migration System
- Global Scheduler

지역 생성, 활성화, 병합, 분리, 비활성화를 지원한다.

인접 지역이 서로 충돌하는 방식으로 동시에 처리되지 않도록 한다.

엔티티 이동, 폭발, 유체, 레드스톤, 투사체, 포탈 등 지역 경계 작업의 정합성을 유지한다.

## 3.3 Adaptive CPU Scheduler

서버가 사용할 수 있는 CPU 자원을 효율적으로 활용한다.

다음 작업을 병렬화 대상으로 관리한다.

- Region ticking
- Chunk generation
- Chunk loading
- Lighting
- Entity pathfinding
- Network packet processing
- Packet encoding
- Compression
- World saving
- LOD generation

CPU 사용률을 무조건 100%로 만드는 것이 목표가 아니다.

**게임 틱 지연을 보호하면서 사용 가능한 CPU 자원을 최대한 활용하는 것이 목표다.**

게임 틱이 밀리면 낮은 우선순위의 청크 선행 생성이나 LOD 계산을 늦춘다.

CPU 코어 수, 활성 Region 수, 작업 큐, 메모리 압력, 틱 지연을 기준으로 동시 실행량을 조정한다.

## 3.4 초기 구현 전략

최종 아키텍처는 Regionized Multithreading을 전제로 한다.

하지만 초기에 하나의 Region으로 정확성을 확보할 수 있어야 한다.

개발 순서:

1. Region 소유권 인터페이스 구축
2. 단일 Region 동작 검증
3. 독립된 두 Region의 병렬 Tick 검증
4. 지역 간 메시지 처리
5. Entity migration
6. 안전한 Region 병합·분리
7. 동적 작업 분산
8. 성능 비교 후 기본 활성화

완성되지 않은 병렬 틱을 무리하게 활성화하여 월드를 손상시키지 않는다.

---

# 4. Event-Driven Engine

Sculith의 주요 실행 구조는 이벤트 중심이다.

기본 파이프라인:

```text
Network Packet
   ↓
Protocol Decode
   ↓
Validation
   ↓
Player Intent
   ↓
Region / World Owner
   ↓
Authoritative State Commit
   ↓
Committed Event Batch
   ↓
Interest Manager
   ↓
Protocol Encoder
   ↓
Client
```

다음 구조를 구현한다.

- Typed Events
- Event Registry
- Event Batching
- Bounded Event Queues
- Event Priority
- Interest-Based Subscription
- Region-Local Event Dispatch
- Scheduled Events
- Backpressure
- State Revision Tracking

블록 변경 하나가 발생할 때마다 모든 플레이어에게 전체 상태를 다시 보내지 않는다.

변경된 상태를 추적하고, 해당 데이터를 필요로 하는 플레이어에게만 전송한다.

이벤트가 너무 자주 발생하면 병합할 수 있는 변경은 묶어서 처리한다.

단, 순서가 중요한 게임 이벤트는 무조건 병합하거나 생략하지 않는다.

물리, 레드스톤, 유체, 엔티티 AI처럼 정기적 갱신이 필요한 시스템은 예약 Tick으로 처리한다.

---

# 5. Rust Plugin System

Sculith는 **Rust 플러그인을 공식적으로 지원**해야 한다.

플러그인은 코어의 내부 구조에 무제한 접근하는 방식이 아니라, 안정적인 공식 API를 통해 작동하도록 설계한다.

## 필수 기능

- Rust Plugin SDK
- Plugin Manifest
- Plugin Lifecycle
- Typed Events
- Event Cancellation
- Command Registration
- Permission API
- Player API
- World API
- Entity API
- Inventory API
- Scheduler API
- Configuration
- Plugin Data Storage
- Logging and Metrics

## 플러그인 실행 방식

우선 **Rust → WebAssembly/WASI Component Model 기반의 플러그인 실행 구조**를 검토한다.

이 방식은 안전한 호스트 경계를 제공하기 위한 첫 번째 후보이며, 실제 버전과 실행 비용을 검증한다.

Rust 네이티브 동적 라이브러리 방식은 성능과 ABI 안정성, crash isolation 문제를 비교한 뒤 선택적으로 지원한다.

Rust 컴파일러 버전이 바뀌어도 임의의 Rust 내부 ABI가 안정적이라고 가정하지 않는다.

## Region-Aware Plugin API

Folia와 유사한 실행 소유권 규칙을 제공한다.

- Region Scheduler
- Entity Scheduler
- Global Scheduler
- Async Scheduler

비동기 플러그인 작업은 직접 월드를 수정하지 못하고, 해당 Region 소유자에게 명령을 제출해야 한다.

지역이 바뀐 엔티티의 작업은 새로운 소유자를 따라가야 한다.

플러그인마다 실행 시간, 메모리, 호출량 등의 한도를 적용할 수 있도록 설계한다.

공식 SDK를 사용한 실제 예제 플러그인을 만들어 검증한다.

---

# 6. World Engine

게임 로직과 네트워크 프로토콜을 분리한다.

필수 구현 대상:

- Block registry
- Item registry
- Entity registry
- Chunk and subchunk
- Block states
- Collision
- Fluid updates
- Redstone
- Lighting
- Inventory transactions
- Crafting
- Commands
- Effects
- Damage and combat
- Scheduled ticks
- World generation
- Dimension system
- World persistence

Bedrock의 동작을 기준으로 기능별 호환성 테스트를 작성한다.

Java와 Bedrock에서 동일한 작업을 수행했을 때 하나의 권위 월드 상태로 결과가 일치해야 한다.

월드 데이터는 프로토콜별 ID에 종속되지 않는 내부 형식으로 관리한다.

---

# 7. World Generation Optimization

C2ME, Moonrise, Lithium, Starlight, FerriteCore 등 공개 프로젝트의 최적화 원리와 구조를 조사한다.

특히 다음을 구현 후보로 삼는다.

- Parallel chunk generation
- Dependency-aware DAG scheduler
- Chunk request deduplication
- Prioritized world generation
- Cancellable generation tasks
- Asynchronous chunk loading
- Parallel lighting where safe
- Palette compression
- Cache-friendly block storage
- SIMD noise generation
- Reusable generation buffers
- Snapshot-based persistence

청크 생성 작업이 주변 청크를 수정할 수 있는 경우, 읽기·쓰기 범위와 의존성을 관리한다.

월드 생성 시드 결과의 결정성을 보존한다.

최적화 코드가 특정 월드 생성 결과를 바꾸는 경우 이를 명확히 구분한다.

완료되지 않은 청크 데이터를 실제 플레이에 잘못 노출하지 않는다.

---

# 8. Packet-Only Bedrock LOD

Bedrock 클라이언트에 Distant Horizons와 유사한 원거리 지형 표현을 제공한다.

실제 게임 엔티티를 서버 월드에 생성하지 않는 방식이다.

서버는 지형 데이터를 LOD 타일로 요약하고, 클라이언트에는 표현용 actor를 패킷으로 생성·갱신·제거한다.

이를 일반적인 게임 엔티티 시스템과 분리한다.

## 설계 요구사항

- Shared LOD Tile Cache
- Multiple LOD Levels
- Per-player Presentation Actors
- Distance-based LOD
- Geometry templates
- Height and surface classification
- Dirty tile tracking
- Packet delta updates
- Per-player visibility
- Dimension cleanup
- Cache invalidation
- Client render budget

같은 지역을 보는 플레이어끼리 지형 요약 결과를 공유한다.

플레이어마다 별도의 월드 데이터 전체를 생성하지 않는다.

서버의 AI, 충돌, 물리 및 일반 엔티티 틱에 표시 전용 actor를 등록하지 않는다.

클라이언트의 렌더링 비용은 별도로 존재한다는 점을 고려한다.

베드락 리소스팩의 표현 한계를 검증하고, 임의의 메시를 서버가 제한 없이 보낼 수 있다고 가정하지 않는다.

저사양 기기에서 원경 품질을 조절할 수 있도록 한다.

---

# 9. Latest Bedrock Camera Compatibility

카메라 기능은 주요 호환성 요구사항이다.

다음 기능을 조사하고 지원한다.

- Camera Presets
- Camera Set / Clear
- Easing
- Position and Rotation
- Facing
- Camera Fade
- FOV
- Target Tracking
- Entity Attachment
- Camera Detachment
- Spline Animations
- Input Restriction
- Camera Reset
- Dimension Transition Handling

최신 안정 버전에 추가된 기능을 지속적으로 확인한다.

카메라가 움직여도 플레이어 시뮬레이션과 카메라 기준 표시 범위를 정확하게 분리한다.

카메라 종료, 사망, 재접속, 차원 이동에서 상태가 정상적으로 복원되는지 검증한다.

Java Companion에서도 동일한 연출을 재현하는 것을 목표로 한다.

---

# 10. Low-Level Performance Engineering

Sculith는 높은 성능을 핵심 정체성으로 삼는다.

CPU와 RAM 사용량을 줄이고, 여러 코어를 효율적으로 활용할 수 있도록 설계한다.

## 기본 설계에 포함할 최적화

- Cache-friendly memory layout
- Dense integer identifiers
- Struct-of-Arrays where beneficial
- Hot/cold data separation
- Packed bit storage
- Uniform chunk sections
- Copy-on-write snapshots
- Reusable buffers
- Bounded queues
- Byte-based memory budgets
- Single-flight task deduplication
- Delta-based packet updates
- Network interest management
- Batched processing
- Reduced lock contention
- Minimized hot-path allocations
- Shared immutable data
- Efficient spatial indexing

## 측정 후 적용할 최적화

- SIMD
- CPU-specific intrinsics
- Profile-Guided Optimization
- Link-Time Optimization
- NUMA-aware memory allocation
- CPU affinity
- Optimized allocators
- Batched network syscalls
- io_uring
- GPU compute
- Specialized network backends

최적화는 실제 성능을 개선하는 경우에만 활성화한다.

잘못된 최적화로 게임 기능이 변경되거나 데이터가 손상되어서는 안 된다.

성능 향상률은 실제 벤치마크로 증명한다.

비교 대상은 Dragonfly, Pumpkin 및 적절한 기존 서버 구현이다.

비교 조건에서 구현 기능, 월드, 인원, 시뮬레이션 범위가 동일하도록 한다.

---

# 11. REA 활용

사용자가 지정한 REA 역공학 분석 플러그인을 연구 워크플로에 포함한다.

작업 시작 시 현재 Codex 환경에서 실제로 사용할 수 있는 REA 플러그인 또는 MCP 도구를 검색한다.

사용 가능하다면 프로토콜, 카메라, 렌더링, 콘텐츠 및 버전 호환 분석에 활용한다.

정확한 도구 이름과 명령을 확인하지 못했다면 가짜 명령을 만들지 않는다.

검증된 공식 문서, 공개 코드와 합법적으로 확보한 분석 자료를 활용하고, 분석 결과의 출처와 대상 버전을 기록한다.

REA 연결 문제로 독립적인 구현 작업 전체를 중단하지 않는다.

---

# 12. 참고할 프로젝트

다음 프로젝트를 조사하고, Sculith에 적용할 가치가 있는 설계와 구현 방식을 평가한다.

| Project | Research Focus |
|---|---|
| Dragonfly | Bedrock-native gameplay, server API |
| Pumpkin | Rust server architecture, protocols, threading |
| Folia | Regionized multithreaded ticking |
| C2ME | Parallel chunk generation |
| Moonrise | Chunk management and scheduling |
| Lithium | Simulation optimization |
| FerriteCore | Memory reduction |
| Starlight / ScalableLux | Lighting optimization |
| Geyser | Java–Bedrock translation differences |
| ViaBedrock | Java client and Bedrock server interoperability |
| ViaBedrockUtility | Java-side Bedrock content rendering |
| bedrock-rs | Rust Bedrock protocol libraries |

**영감을 얻는 것과 코드를 복사하는 것은 구분한다.**

각 프로젝트의 라이선스를 확인한다.

GPL 코드 등을 무심코 가져와 Sculith의 라이선스 정책과 충돌시키지 않는다.

다른 프로젝트의 기능 설명만으로 해당 기능이 완전히 동작한다고 가정하지 않는다.

---

# 13. Repository Architecture

하나의 저장소와 Cargo Workspace를 사용한다.

목표 구조:

```text
sculith/
├── AGENTS.md
├── Cargo.toml
├── Cargo.lock
├── README.md
├── LICENSE
├── crates/
│   ├── sculith-core/
│   ├── sculith-world/
│   ├── sculith-runtime/
│   ├── sculith-network/
│   ├── sculith-bedrock/
│   ├── sculith-java/
│   ├── sculith-content/
│   ├── sculith-storage/
│   ├── sculith-presentation/
│   ├── sculith-plugin-api/
│   ├── sculith-plugin-host/
│   └── sculith-server/
├── companion/
│   └── java-fabric/
├── examples/
│   └── rust-plugin/
├── tools/
├── tests/
├── benches/
├── docs/
└── .github/
```

현재 저장소에 이미 구현된 구조가 있다면 이를 분석하고, 필요할 때만 점진적으로 변경한다.

처음부터 빈 crate를 과도하게 만들지 않는다.

실제 구현이 없는 모듈을 완성된 기능으로 표시하지 않는다.

모듈 사이에 순환 의존성을 만들지 않는다.

---

# 14. Development Milestones

2026년 12월 31일까지 설치하고 실행할 수 있는 공개 알파를 목표로 한다.

전체 목표를 단기간에 완성할 수 없다는 이유로 핵심 요구사항을 삭제하지 않는다.

대신 기능별로 구현·검증 단계를 나눈다.

## Milestone 0: Existing Work Audit

- 기존 저장소 조사
- 현재 작업 변경 내용 확인
- Rust 개발 환경 준비
- 빌드·테스트 복구
- 지원 버전 확인
- 구현 상태 보고

## Milestone 1: Bedrock Native Server

- 최신 목표 Bedrock 연결
- 인증과 리소스팩 협상
- 월드 진입
- 이동
- 기본 블록 상호작용
- 정상적인 세션 종료

## Milestone 2: Java Crossplay

- 최신 근처 Java 연결
- 동일한 월드 접속
- 플레이어 상호 표시
- 블록·엔티티 상태 동기화
- 인벤토리 기본 정합성

## Milestone 3: Plugin SDK

- Rust Plugin SDK
- Event hooks
- Commands
- Scheduler API
- 실제 예제 플러그인
- 플러그인 로딩과 오류 처리

## Milestone 4: Regionized Multithreading

- Region ownership
- Concurrent region ticking
- Region scheduler
- Cross-region events
- Entity migration
- Correctness tests

## Milestone 5: Content Compatibility

- Bedrock resource pack handling
- ContentIR
- Java resource conversion
- Java Companion
- Custom entity and animation example
- Camera example

## Milestone 6: Performance and LOD

- Parallel chunk generation
- Memory optimization
- Packet-only LOD
- Interest management
- Network optimization
- Performance benchmarks

## Milestone 7: Public Alpha

- 설치 가능한 서버 실행 파일
- Windows와 Linux 실행 검증
- 기본 설정 파일
- 사용 설명서
- 실제 Minecraft 접속 검증
- 테스트 결과 공개
- 미구현 기능 목록
- 알려진 문제
- 재현 가능한 성능 측정

마일스톤 순서는 의존성과 실제 구현 상태에 따라 조정할 수 있지만, Bedrock 기반이라는 핵심 목표를 바꾸지 않는다.

---

# 15. 품질과 안전성 규칙

다음 원칙을 반드시 준수한다.

1. 안정적인 Rust를 우선 사용한다.
2. 코어의 unsafe 사용을 최소화한다.
3. unsafe 및 FFI 코드는 별도 검증 경계에 둔다.
4. 게임의 권위 상태를 클라이언트에 맡기지 않는다.
5. 패킷 길이와 입력 데이터를 검증한다.
6. 인증과 권한 검사를 생략하지 않는다.
7. 플레이어 요청을 믿고 인벤토리를 직접 덮어쓰지 않는다.
8. 메모리 사용량과 큐 크기에 상한을 둔다.
9. 실제 월드 데이터를 삭제하지 않는다.
10. 새 기능마다 자동 테스트를 추가한다.
11. 버그를 숨기기 위해 기능을 조용히 비활성화하지 않는다.
12. 성능 향상과 기능 정확성을 별도로 측정한다.
13. 단위 테스트 통과와 실제 게임 접속 성공을 구분한다.
14. 구현되지 않은 기능을 지원한다고 표시하지 않는다.
15. 실패한 테스트와 남은 문제를 기록한다.

## CI

다음 검증을 구축한다.

- `cargo fmt --check`
- `cargo clippy`
- `cargo test`
- 프로토콜 codec 테스트
- 청크 및 저장 round-trip 테스트
- 동시성 테스트
- 플러그인 테스트
- 콘텐츠 파싱 테스트
- 성능 회귀 테스트

가능한 경우 실제 Java 및 Bedrock 클라이언트와의 통합 테스트를 추가한다.

---

# 16. Codex 자율 작업 규칙

지금부터 너는 실제 개발을 수행한다.

계획서 작성에 과도하게 시간을 소비하지 않는다.

하지만 코어 구조를 검증하지 않은 채 대량의 상호 의존 코드를 생성하지 않는다.

반드시 다음 개발 루프를 사용한다.

**Inspect → Design → Implement → Test → Review → Benchmark → Document → Next Task**

### 구체적인 지침

- 현재 저장소의 실제 코드를 먼저 읽는다.
- 이미 완성된 기능은 다시 구현하지 않는다.
- 기존 API를 깨는 변경은 영향 범위를 먼저 확인한다.
- 하나의 기능을 작동 가능한 단위로 완성한다.
- 구현 후 반드시 가능한 테스트를 실행한다.
- 실제 Minecraft 접속이 필요한 경우 테스트 방법과 결과를 분리한다.
- 필요하다면 독립적인 코드 작업을 병렬화한다.
- 동일한 파일을 여러 작업자가 동시에 수정하지 않는다.
- 불필요한 질문 없이 다음 독립 작업으로 진행한다.
- 실패한 경우 원인을 조사하고 수정한다.
- 근거 없이 성공했다고 표시하지 않는다.
- 원격 푸시, 릴리스, 외부 계정 변경, 데이터 삭제 등 중요한 변경은 기존 권한을 확인한다.

정기적으로 다음 상태를 기록한다.

```text
Current Milestone:
Implemented:
Verified:
Failed:
Performance:
Known Limitations:
Next Action:
```

이 상태 기록은 실제 작업 결과를 반영해야 한다.

문서 갱신 자체를 기능 구현으로 집계하지 않는다.

---

# 17. 지금 즉시 수행할 작업

현재 Codex 작업 세션이 이미 실행 중이라면 해당 작업을 보존한다.

새로운 구조로 무작정 덮어쓰지 않는다.

다음 작업을 순서대로 수행한다.

1. 현재 저장소의 브랜치, 변경 파일, 진행 중인 작업을 확인한다.
2. 이 지침과 기존 구현의 차이를 분석한다.
3. Bedrock-native 아키텍처가 유지되고 있는지 확인한다.
4. Java가 별도의 서버가 아닌 동일한 월드에 연결되는지 검증한다.
5. Regionized Multithreading과 Rust Plugin API의 설계 경계를 확인한다.
6. 최신 프로토콜과 카메라 지원 상태를 확인한다.
7. 현재 가장 중요한 미완성 기능을 선정한다.
8. 해당 기능을 실제로 구현한다.
9. 빌드·테스트·정합성을 확인한다.
10. 다음 구현 작업으로 진행한다.

**우선순위는 항상 실제 Bedrock 서버 구동 → Java 동시 접속 → 동일 월드 및 게임 시스템 → 플러그인 → 콘텐츠 고충실도 → 병렬화와 최적화 확장이다.**

단, 병렬화·메모리 소유권·이벤트 경계는 처음부터 고려한다.

이 프로젝트의 최종 목표는 다음과 같다.

**하나의 Sculith 서버를 실행하면 Minecraft Bedrock과 Java 사용자가 동시에 접속하고, 동일한 Bedrock 기반 월드와 콘텐츠를 경험하며, 서버는 Rust의 메모리 안전성을 기반으로 여러 CPU 코어를 효율적으로 활용한다.**

단순한 프로토콜 프록시가 아니라 완전한 Minecraft 서버 엔진을 개발한다.

**SCULITH**
**Fast. Light. Safe.**
