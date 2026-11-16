# 성능 계측과 플러그인 전달 최적화 — 2026-11-17 KST

현재 구현된 플러그인 IPC 경로에 계측을 추가하고, 수신자별 깊은 이벤트 복사를
제거했다. 같은 조건의 반복 측정에서 32개 플러그인 중 1개가 구독하는 큰 이벤트
5,000회 전달 시간이 중앙값 **766.339 → 267.575 ms(-65.1%)**, 전부 구독할 때는
**3,141.371 → 2,728.644 ms(-13.1%)**였다. 현재 게임 전체 처리량이나 동시접속
인원으로 환산할 수 없는 IPC 전용 결과다.

작업 범위는 Phase 1의 플러그인/디스패처 기준 측정과 계측, Phase 2의 이벤트 전달
복사 제거다. ECS·RMT·LOD 전체를 구현한 결과가 아니다. 원본 요구사항은
[성능 지침](performance-directive.md), 실행 방법은 [프로파일러 안내](../guides/profiling.md)에 있다.

## 코드 분석에서 확인한 구조와 병목

| 대상 | 현재 구현과 분석 결과 |
| --- | --- |
| 상태 변경·소유권 | `sculith-world::Region`은 단일 가변 소유권과 COW 스냅샷을 사용한다. `sculith-game::GameState`가 게임 결정을 수행하고 runtime의 제한된 mailbox로 입력받는다. 별도 에디션 시뮬레이션은 없다. |
| Tick·게임 실행 | main의 기본 20 Hz는 플러그인 이벤트 시계다. 완전한 AI·물리·레드스톤 시뮬레이션 Tick으로 측정하지 않았다. |
| Tokio | 네트워크·IPC 대기는 비동기이며 플러그인 응답은 동시에 기다린다. 액션 적용 순서는 설정 순서다. 단일 지역의 게임 계산을 병렬화하지 않는다. |
| 청크·저장 | 제한된 평지 생성기, 팔레트 섹션과 공유 스냅샷이 있다. 변경된 지역은 별도 작업자 하나가 주기적으로 저장한다. 증분 WAL·무손실 복구는 없다. |
| 복사·할당 | 기존 디스패처는 구독하지 않은 프로세스까지 `Event::clone()`을 실행했다. 64개 블록 변경처럼 문자열/Vec를 가진 이벤트에서 측정 가능한 CPU 비용이 발생했다. |
| 이벤트 큐 | 제한된 방송 큐와 지역 명령 큐가 있다. 초과 시 해당 구독/연결을 명시적으로 종료한다. 이번 변경은 큐 상한이나 실패 정책을 변경하지 않는다. |
| 플러그인 IPC | 신뢰된 Rust 실행 파일과 JSON-lines 프로토콜이다. 버전 1–11 협상, 요청 ID, 크기 제한, 시간 제한, 개인 응답 검사를 보존했다. JSON 인코딩과 worker의 디코딩 비용은 남아 있다. |
| 패킷 경로 | Java·native 어댑터는 공통 커밋을 각자의 규격으로 표현한다. 연결별 암호화/상태를 공유하는 캐시는 도입하지 않았다. 이번 계측은 Minecraft 네트워크 바이트 계측이 아니다. |
| 관심 영역 | 로드된 청크와 초기화된 플레이어의 가시성/증분 갱신이 있다. 일반 엔티티 ECS/BVH나 시뮬레이션 LOD는 없다. |
| 검증 | 기존 커널 벤치마크는 있었지만 반복 fanout의 프로세스 CPU/메모리 기준값은 없었다. 실제 Rust worker 기반 측정기와 호환 조건 검사기를 추가했다. |

## 구현

- OFF는 최소 호출·완료된 IPC 프레임 바이트·실패/타임아웃 카운터를 유지한다. 추가
  요청별 계측 시계와 이력 할당을 생략한다. 기존 timeout 자체의 시계는 유지된다.
- MONITOR는 최근 128개 요청의 정확한 nearest-rank p50/p95/p99와 인코딩/전체 wall
  시간을 수집한다. PROFILE은 요청한 1–300초 동안 최대 512개 상세 샘플을 보관하고
  덮어쓴 샘플 수를 보고한다. CPU 샘플링·콜스택으로 오인하지 않도록 범위를 표시한다.
- `run --profile-plugins N --profile-output new.json`은 GUI 없이 보고서를 만든다.
  새 파일만 생성하며, 별도 작업자 하나로 쓰고 종료 시 합류한다. 보고서 실패가 게임을
  중단하지 않는다. 내용·주소·계정 토큰·입력 문자열을 기록하지 않는다.
- 최종 디스패처는 원본 이벤트를 불변 참조로 직렬화한다. 요청 envelope는 `Serialize`
  만 요구하고 `Clone`을 요구하지 않는다. 소유자를 통한 SDK 이벤트 생성과 JSON 프레임
  버퍼 생성은 남아 있다. 네트워크 전체가 zero-copy라고 주장하지 않는다.
- 기존 SDK의 소유된 요청과 바이트 단위로 같은 JSON을 생성하는 회귀 검사를 추가했다.
  동시에 응답을 기다린 뒤 기존 순서로 액션을 적용하며 개인 수신자 필터를 보존한다.

## 측정 조건

- 기준 소스: `09574e59dae196ba501b24c61909a4a9939cc65c` — 계측이 있지만 기존 복사 경로.
- 최종 소스: `789053efb85a5ca3b5a18aa8d22bc39cb6bcadad` — 참조 직렬화 경로.
- 두 측정 모두 커밋된 깨끗한 작업 트리, 동일 fixture/측정기 해시, 동일 OS·CPU 조건.
  둘 다 최소 계측을 포함한다. 계측 추가 전 `b398c7e` 대비 OFF 계측 자체의 비용은
  별도 측정하지 않았으며, 원본 엔진 전체에 대한 개선율로 해석하지 않는다.
- AMD EPYC 9V45 호스트, 컨테이너 CPU quota **4 CPU**, 메모리 상한 **32 GiB**.
  호스트의 물리 코어 수를 벤치마크에 할당된 코어 수로 사용하지 않았다.
- `rustc 1.100.0 (0ef1bc386 2026-11-09)`, release, thin LTO.
- 각 조건 128회 워밍업 후 5,000회 전달, **5회 반복**의 중앙값. 1·8·32개 실제 Rust
  자식 프로세스이며 숫자는 플레이어 수가 아니다. 각 프로세스의 전달 횟수와 실패 0을 확인한다.
- 이벤트 값은 타이머 전에 준비한다. 프로세스 시작/종료 시간은 제외한다. CPU는 Linux
  `/proc`의 프로세스별 user+system tick 차이이며 해상도는 **10 ms**다.
- RSS/PSS는 50 ms 주기의 관측 최고값이다. 실제 high-water mark/할당 횟수가 아니다.
  RSS 합계는 공유 매핑을 중복 계산하며 PSS 합계는 이를 배분한다. 준비된 입력 데이터도
  벤치마크 메모리에 포함된다. 짧은 조건은 메모리 관측이 부족할 수 있다.
- 공유 클라우드 환경이므로 빈도/스케줄링 변동의 영향을 배제한 통계적 인과 검정은 아니다.
  다른 구동기와의 성능 비교나 서버 TPS 수치는 측정하지 않았다.

## 전달 지연 결과

시간은 5,000회 전체 wall 중앙값, p95/p99는 각 반복의 개별 호출 지연 분위수를
구한 후 중앙값을 사용한다. 큰 이벤트는 64개의 공통 블록 변경을 포함한다.
`world-none`은 직접 디스패치 API의 무구독 조건이며 일반 게임 처리량이 아니다.

| 조건 | 이전 전체 ms | 최종 전체 ms | 변화 | p95 µs, 이전 → 최종 | p99 µs, 이전 → 최종 |
| --- | ---: | ---: | ---: | ---: | ---: |
| `tick-one-of-thirty-two` | 135.972 | 119.884 | -11.8% | 37.106 → 31.757 | 65.388 → 61.813 |
| `world-all-thirty-two` | 3141.371 | 2728.644 | -13.1% | 819.123 → 726.803 | 1821.951 → 1506.256 |
| `world-none-of-thirty-two` | 675.802 | 35.028 | -94.8% | 166.661 → 6.469 | 213.131 → 7.572 |
| `world-one-of-eight` | 392.419 | 262.950 | -33.0% | 99.170 → 70.216 | 139.911 → 107.322 |
| `world-one-of-one` | 273.716 | 262.784 | -4.0% | 69.424 → 69.494 | 101.183 → 102.475 |
| `world-one-of-thirty-two` | 766.339 | 267.575 | -65.1% | 190.407 → 68.372 | 257.818 → 96.255 |

단독 구독 조건의 p95는 **+0.1%**, p99는 **+1.3%**로 소폭 나빠졌다. 전체 시간은
4.0% 줄었지만 모든 분위수가 개선된 것은 아니다. 비교기의 기본 wall 20%/tail 30%
회귀 문턱을 넘은 조건은 없었다. 이 문턱이 운영 지연 SLO나 통계적 유의성 기준은 아니다.

중간 수정 `48ddd07`은 비구독 대상 복사만 건너뛰었다. 그때 단독 구독의 전체 시간은
273.716 → 291.841 ms(**+6.6%**)로 나빠졌다. 그 결과를 버리지 않고 보존했고,
선택된 수신자도 원본을 참조하도록 최종 변경한 뒤 같은 조건으로 다시 측정했다.

## CPU·메모리

CPU 시간은 벤치마크 호스트와 실제 worker 프로세스를 분리했다. wall time에서
플러그인 CPU 사용량을 추정하지 않는다. 아래 값은 10 ms tick 해상도의 중앙값이다.

| 조건 | 호스트 CPU 초, 이전 → 최종 | workers CPU 초, 이전 → 최종 |
| --- | ---: | ---: |
| `tick-one-of-thirty-two` | 0.09 → 0.09 | 0.03 → 0.02 |
| `world-all-thirty-two` | 2.69 → 2.20 | 6.56 → 6.18 |
| `world-none-of-thirty-two` | 0.68 → 0.03 | 0.00 → 0.00 |
| `world-one-of-eight` | 0.25 → 0.11 | 0.14 → 0.14 |
| `world-one-of-one` | 0.11 → 0.10 | 0.13 → 0.14 |
| `world-one-of-thirty-two` | 0.76 → 0.12 | 0.15 → 0.13 |

주요 1/32 조건의 호스트 CPU는 **0.76 → 0.12초(-84.2%)**였다. worker 실행 파일은
동일하며 worker CPU 차이를 플러그인 코드 자체의 최적화로 해석하지 않았다.

| 1/32 조건의 관측 메모리 중앙값 | 이전 KiB | 최종 KiB |
| --- | ---: | ---: |
| 호스트 RSS | 44,284 | 44,052 |
| 전체 RSS 합계 | 89,712 | 89,540 |
| 호스트 PSS | 43,467 | 43,235 |
| 전체 PSS 합계 | 49,350 | 49,106 |

전체 PSS 감소는 약 **0.5%**다. 큰 상주 메모리 절감이 입증됐다고 보고하지 않는다.
일부 다른 조건의 관측 메모리는 소폭 증가했다. 실제 heap 할당 횟수와 장시간 단편화는
이번 측정에 포함하지 않았다. MONITOR/PROFILE의 별도 오버헤드 비교도 아직 수행하지 않았다.

## 병렬성·아직 없는 기술

기존 여러 플러그인 프로세스의 대기 동시성은 유지했다. 게임 Region 계산의 병렬
속도 향상은 **미측정/미구현**이다. 자식 프로세스의 CPU 병렬 실행을 RMT로 세지 않았다.

| 항목 | 현재 상태와 보류 이유 |
| --- | --- |
| 전체 엔티티 ECS·SoA·generational ID | 일반 몹/투사체/아이템 시뮬레이션이 없어 실제 대량 순회 동등성 검증 대상이 부족하다. 기존 최대 64플레이어 상태를 ECS로 재작성하지 않았다. |
| RMT·이동·분할/병합 | 현재 한 지역 소유자다. 다중 지역 라우팅, 경계 작업과 소유권 이전을 먼저 구현/검증해야 한다. Tokio 작업 추가만으로 완료 처리하지 않았다. |
| Work stealing·우선순위 실행기 | CPU 집약적 지역 작업과 의존성/취소 계약이 아직 없다. 이번 보고서 작업자는 제한된 단일 저장 작업이며 게임 Job System이 아니다. |
| Simulation LOD | 일반 AI/경로 탐색·유체·레드스톤 규칙이 없어 정확성을 유지하는 생략 범위를 검증할 수 없다. 필수 Tick을 임의로 생략하지 않았다. |
| Packet-only LOD | 타일 캐시·멀티레벨 지형·예산 조절이 없다. 공식 native 렌더러 검증과 표현 예산이 선행돼야 한다. |
| SIMD·arena/slab·버퍼 풀 | 실제 효과가 확인되지 않은 경로에 적용하지 않았다. 안전한 Rust를 유지했고 allocator 계측용 unsafe도 추가하지 않았다. |
| 증분 저장·WAL | 주기적 원자 스냅샷만 있다. 마지막 완료 저장 이후 변경/플레이어 데이터의 무손실 복구를 보장하지 않는다. 저장 중 강제 종료의 추가 fault injection이 필요하다. |
| 전체 Sculith Profiler | IPC wall 계측만 제공한다. CPU 샘플링/Flame Graph/전체 call tree/지역 queue·네트워크·할당 계측은 없다. |
| Adaptive controller | 필수 게임 계산을 조절하지 않았다. 지역/배경 작업 지연 데이터와 제어 대상이 준비된 뒤 히스테리시스 등을 검증해야 한다. |
| 장기 성능 게이트 | 동일 조건 비교기와 CI 기능/수명주기/프로토콜 검사는 있다. 다른 사양 GitHub runner에 절대 시간 문턱을 적용하지 않았다. 전용 고정 환경의 장기 기준 운영은 후속 작업이다. |

미구현 기능을 전제로 한 대량 엔티티·레드스톤/유체·다중 지역·LOD 고부하 시나리오는
따라서 보류했다. 현재 벤치마크를 그런 기능의 성능 수치로 대체하지 않았다.

## 검증 결과

- `cargo fmt --all --check`, `cargo check --workspace --locked`, workspace/all-targets
  Clippy `-D warnings`, **249 프로젝트 테스트**, 소유권 compile-fail doctest 통과.
  release 전용 벤치마크 2개는 기존 정책대로 기본 테스트에서 제외했다.
- `cargo build --workspace --release --locked` 통과. 비교기 계약 Python 테스트 3개 통과.
- 실제 Rust worker의 제한된 profile/자동 만료/중단 캡처/기존 파일 보존/내용 비수집 검사 통과.
  Linux의 실제 `--monitor-plugins` 실행에서도 30초 콘솔 통계와 계속 가능한 TCP 상태 조회를 확인했다.
- Java/native 공통 입력·단순 폼, 인벤토리, 월드 일괄 변경, 자동 저장 복구, 기존 v8/v9
  연결 관측, 사운드·정적 모델·팩 프록시 검사 통과. 패킷 등록은 Java 260/native 244개다.
- 최종 소스의 실제 순정 Java 26.3에서 공통 팩·정적 모델을 유지하며 네 입력 컨트롤을
  마우스/키보드로 조작하고 `LiveName / false / 1 / 5`의 정확한 개인 응답을 확인했다.
  클라이언트 모드나 그래픽 세션의 게임 패킷 주입은 없다. 소유한 세 프로세스는 종료했다.
- Linux [43963744073](https://github.com/SKYRPG8957/Sculith/actions/runs/43963744073)는
  전체 workflow를 완료했다. Windows
  [43963744227](https://github.com/SKYRPG8957/Sculith/actions/runs/43963744227)도 전체
  workflow를 완료했다. static-CRT release 실행 파일의 profiler/공통 UI/기존
  프로토콜 검사와 후처리까지 통과했다. 프로토콜 프로브는 공식 GDK 렌더링 검증과 다르다.

실계정 정상 입장, 공식 Windows GDK 화면/입력, Internet/NAT, 음향 출력 및 모든
게임 동작을 검증했다고 주장하지 않는다. 프리뷰/릴리스/빌드 artifact는 게시하지 않는다.

## 실제 변경 파일

성능 코드 구간 `b398c7e..789053e`의 전체 파일 목록이다. 마무리 문서는
이 보고서와 `README.md`/`README.ko.md`의 안내 링크를 추가했다. UI 프로토콜 수정 자체는 선행
`b398c7e`이고 이후 변경은 계측/전달 경로와 검증이다.

```text
.github/workflows/ci.yml
.github/workflows/windows.yml
AGENTS.md
Cargo.lock
crates/sculith-plugin-host/examples/dispatch_bench.rs
crates/sculith-plugin-host/src/lib.rs
crates/sculith-plugin-host/src/profiling.rs
crates/sculith-plugin-host/src/tests.rs
crates/sculith-server/src/main.rs
crates/sculith-server/src/profiling.rs
docs/development/performance-directive.md
docs/development/status.md
docs/development/verification/custom-forms.md
docs/guides/forms.md
docs/guides/profiling.md
scripts/bench/compare_plugin_dispatch.py
scripts/bench/plugin_dispatch.py
scripts/bench/test_compare_plugin_dispatch.py
tests/fixtures/profile-plugin/Cargo.toml
tests/fixtures/profile-plugin/src/main.rs
tests/interop/profile_smoke.py
```

핵심 변경은 `sculith-plugin-host/src/lib.rs`의 참조 envelope/선택 전달,
`profiling.rs`의 제한된 계측, `sculith-server/src/profiling.rs`의 CLI·보고서,
`dispatch_bench.rs`와 `scripts/bench`의 실제 프로세스 측정/동일 조건 비교다.
`Cargo.lock`은 로컬 테스트 fixture 1개 항목만 추가했다. 새 엔진 crate는 없다.
MIT 라이선스와 `unsafe_code = forbid` 정책을 유지했다.

## 재현·자료

[실행 명령](../guides/profiling.md#repeated-dispatch-benchmark)으로 각 소스를 동일
조건에서 빌드/측정한다. 원시 JSON은 작업 공간의 `/tmp/sculith-profile-*.json`에
보존하며 GitHub에 업로드하지 않았다. 파일 SHA-256:

- `sculith-profile-before.json`: `3ff980ca0d8f39aad86a5bace9ae9d81b3a3130d1bf89615aca827f351abc257`.
- `sculith-profile-after.json`: `206e295e61af1972aead4b9619b3a3ce2bc9c0bfeef3d1612fdde560a98fb6af`.
- `sculith-profile-borrowed.json`: `4771689a29ab59f5b58d486d4f51ecd1931962d80150521922f0f8328d41fa53`.
- `sculith-profile-borrowed-comparison.json`: `b0dd9ddfb7a99e0c38584dd2894b223b67757c719edd2d8ba71591df0e2a4af9`.

최종 실제-game 서버 SHA-256:
`5314088175a05ca2788540706ebadbd5a3475a5cc95e5c9437f48302a618fbfe`.
공통 팩 SHA-1 `4ec0f94603861aeceb633e2384b12d7cc3213135`(11,911 bytes),
stock JAR SHA-1 `e877b6a07acd633fb3bb475002175cec036e7b87`.
최종 개인 응답/모델 화면 SHA-256:
`e6baaa283a4c0e10dade19aa1c8d0cab4bab6f60b18f38f2aebfe5fe4ad71260`.
캡처·클라이언트 자산·실행 파일은 저장소 밖에 남긴다.

## 남은 병목과 우선순위

1. 공개 이벤트의 JSON 직렬화는 선택된 worker마다 여전히 실행된다. 공통 인코딩 재사용과
   제한된 프레임/응답 버퍼 재사용을 따로 측정한다. IPC를 근거 없이 교체하지 않는다.
2. 지역 mailbox 대기·청크 인코딩/생성·저장 flush를 계측하고 실제 혼합 접속 부하에서
   p95/p99를 측정한다. 현재 IPC 예제로 플레이어 수용량을 추정하지 않는다.
3. 일반 엔티티/지역 경계 계약과 저장 fault injection을 구현한 뒤 ECS/RMT/Job System의
   대안 구조를 같은 게임 결과와 CPU 작업으로 비교한다. 이후 LOD/적응 제어를 평가한다.
