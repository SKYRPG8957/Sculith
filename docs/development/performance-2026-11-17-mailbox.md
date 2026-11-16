# World mailbox profiling — 2026-11-17 KST

This extends Phase 1 instrumentation to the authoritative world owner. It provides
measurements for choosing the next optimization; it is not a throughput improvement
or completion of the engine. The [architecture analysis](performance-2026-11-17.md)
and [plugin framing results](performance-2026-11-17-framing.md) remain separate.

## Analysis and implemented behavior

The existing one-region owner serializes game decisions through a 32-slot Tokio
mailbox. Callers wait for admission and then a oneshot reply, but previous plugin
IPC measurements could not distinguish those waits from actual owner work. The
owner also performs lease cleanup and publishes committed results; network encoding,
HTTP/media tasks and snapshot disk writes run elsewhere.

The runtime now times admission, queue residence and command-branch service with
monotonic clocks when explicitly enabled. It reports fixed operation labels, region
ID, aggregate/recent latency, sampled queue occupancy and bounded detailed records.
The default Off path skips profiler clocks, classification and history allocations;
a saturating lifetime handler/abort counter remains active. Retirement in lease Drop
keeps its nonblocking queue attempt and does not acquire the profiler lock.

One metadata mutex protects enabled measurements, not game state. Copying records
for a snapshot is bounded; quantile sorting and file serialization happen outside
that lock and outside the game owner. Existing single ownership, 32/128 queue bounds,
FIFO requests, checks/permissions, quotas, command replies, private hooks and SDK
v1–v11 wire contracts are preserved. No game logic, save format or network packet
format changes. The runtime reuses the workspace's existing serde dependency for
its snapshot interface; no third-party package, crate or unsafe code is added.

The server exposes `--monitor-world`, `--profile-world 1..300` and
`--world-profile-output`. World/plugin captures can run simultaneously with separate
new files. One optional world writer runs on the blocking pool and is joined on
shutdown. Expiry stops new timestamps, exports completed samples and releases
history; interrupted capture records shutdown. Payload/account data is excluded.
See the [operator guide](../guides/world-profiling.md) for metric attribution,
incomplete in-flight work and best-effort retirement exclusions.

## Changed files

- `.github/workflows/ci.yml`
- `.github/workflows/windows.yml`
- `Cargo.lock`
- `crates/sculith-runtime/Cargo.toml`
- `crates/sculith-runtime/examples/mailbox_bench.rs`
- `crates/sculith-runtime/src/lib.rs`
- `crates/sculith-runtime/src/profiling.rs`
- `crates/sculith-runtime/src/profiling_tests.rs`
- `crates/sculith-runtime/src/tests.rs`
- `crates/sculith-server/src/main.rs`
- `crates/sculith-server/src/profiling.rs`
- `crates/sculith-server/src/world_profiling.rs`
- `docs/development/performance-2026-11-17-mailbox.md`
- `docs/development/status.md`
- `docs/guides/profiling.md`
- `docs/guides/world-profiling.md`
- `scripts/bench/compare_runtime_mailbox.py`
- `scripts/bench/runtime_mailbox.py`
- `scripts/bench/test_compare_runtime_mailbox.py`
- `tests/interop/world_api_smoke.py`
- `tests/interop/world_profile_smoke.py`

## Repeatable Off-path comparison

Baseline: `10ca2a83e1c1f2adf06dcdba55b0b555338d4b20` (measurement harness, previous runtime).
Instrumented source: `6710b3b5bdb67284ae87e04c566086581a8b3193`.
Both sources were committed and clean. The harness, driver and resource-sampling
helper hashes match. Same Rust 1.100.0, release/thin-LTO, Linux x86_64 AMD EPYC 9V45,
five visible logical CPUs, four-CPU cgroup quota and 32 GiB memory limit. No other
local build/GUI/test jobs ran during these measurements. Shared-host variance is
not controlled; these are five repeated medians, not confidence intervals or SLOs.

Each case warms 256 owner requests, then makes **1,000,000 real channel requests**
per repetition. One owner/two Tokio workers serve one or 32 requester tasks against
sixteen resident chunks at revision zero. Requests assert actual immutable chunk
snapshots or region captures. There are no child workers, event subscribers,
players, game mutations, network traffic or disk saves in the timed workload.
A short 20,000-call sanity run was too brief for useful resource observations;
it is not used in this comparison.

| Case | 1,000,000 requests, ms | Wall change | p95, µs | p99, µs |
| --- | ---: | ---: | ---: | ---: |
| capture-one | 1442.725 → 1530.930 | +6.1% | 5.188 → 4.988 | 6.540 → 6.019 |
| capture-thirty-two | 496.405 → 518.567 | +4.5% | 25.198 → 26.099 | 45.248 → 46.059 |
| snapshot-one | 1086.299 → 1113.648 | +2.5% | 4.667 → 4.637 | 5.799 → 5.819 |
| snapshot-thirty-two | 260.982 → 275.538 | +5.6% | 14.301 → 15.053 | 24.127 → 25.167 |

This is a measured **2.5–6.1% Off-path wall-time cost**, not an optimization speedup.
The default resource guard reports no increase above 20% wall/30% tail limits;
that guard is not a production acceptance SLO. Capture-one p95/p99 fall while its
whole-run time rises; no causal tail improvement is claimed. Enabled Monitor/Profile
throughput overhead has not been isolated and is a remaining measurement task.

## CPU and memory

CPU is process user+system time, including the owner, requesters and benchmark
latency bookkeeping, with 10 ms resolution. It is distinct from the new wall-time
service metric. Memory is the median of sampled per-run RSS/PSS peaks, in KiB:

| Case | Process CPU, s | Observed RSS, KiB | Observed PSS, KiB |
| --- | ---: | ---: | ---: |
| capture-one | 1.70 → 1.78 | 25600 → 25636 | 24887 → 24923 |
| capture-thirty-two | 0.92 → 0.97 | 25720 → 25732 | 25007 → 25019 |
| snapshot-one | 1.27 → 1.29 | 10636 → 10376 | 9923 → 9663 |
| snapshot-thirty-two | 0.49 → 0.51 | 9964 → 9804 | 9251 → 9091 |

CPU rises about 1.6–5.4% in these cohorts. Capture-case PSS rises 12–36 KiB;
snapshot-case PSS falls 160–260 KiB. Latency-vector allocation and shared-host/page
behavior are part of process memory, so this does not establish a memory reduction.
The 50 ms samples are not exact high-water marks or allocation counts. The benchmark
stores one million latency values and includes their request scheduling/storage;
startup/shutdown and result sorting are outside wall/CPU timing. It is not a game
TPS, capacity, network throughput or parallel simulation measurement.

Off history has zero capacity. Enabled state has fixed operation aggregates, at
most 128 recent phase records and at most 512 detailed records per owner. Allocation
counts/bytes, fragmentation, cache misses and CPU call trees remain unmeasured.
There is still one authoritative region task; concurrent requesters and independent
IPC workers do not establish region parallelism, RMT or work-stealing performance.

## Verification

Local Linux source passes **260 project tests**, two ignored release-only
benchmarks, warnings-denied workspace Clippy, format/check, ownership doctests,
workspace debug/release builds, architecture checks and five Python comparison
contract tests. Complete workflows on the exact instrumented runtime source pass:
[Linux](https://github.com/SKYRPG8957/Sculith/actions/runs/43980707015) and
[native Windows](https://github.com/SKYRPG8957/Sculith/actions/runs/43980707032),
including Windows static-CRT release builds and the new mixed-edition profile probe.
These are protocol/build checks, not official Windows game rendering.

Queue tests hold the owner unstarted, fill all 32 slots, delay a
33rd request and verify distinct admission/queue durations. Cancelled pre-admission
work is absent; admitted work with a dropped receiver still executes as before.
A held telemetry lock cannot block lease Drop. Other checks cover closed owners,
generation changes, automatic expiry, exact recent quantiles, ring overwrites,
history release, handler aborts vs ordinary game rejection and CLI argument groups.

Real Rust fixtures and independent Java/native sockets pass active world profiling
while joining, reading and atomically editing blocks. Simultaneous plugin reports,
private-data omission, continued work after expiry, interrupted capture, no-clobber
output and separate-path checks pass in debug and release. Existing world/CAS/quota,
forced recovery, inventory, forms, models, sound, packet/connection (including v8),
pack proxy and Java/RakNet listener checks also pass. A 30-second Monitor CLI run
prints queue/service p95, then accepts another world read and stops normally.

An exact-source **unmodified Java 26.3** game joins a fresh profile by address,
loads the 11,911-byte shared pack/model and submits all four input controls using
real mouse/keyboard input with both profilers active. Its private result is
`LiveName / false / 1 / 5`. The world report records 46 timed handlers/47 lifetime
handlers and includes model, sound, movement and custom-form submission labels,
with no input/player payloads. Owned game/server/display processes are stopped.
Capture directory: `/tmp/sculith-world-metrics-stock-java/`;
server SHA-256 `779d36a366efe12e88f02edc3816eaeea9bd6befd3d8d8e92ac575a92d1079cf`;
submitted image SHA-256 `f4539570ecc84a783c21e0538d2d383eac837b1c84ac43682169821fd6d69a0a`.
This is Linux offline-loopback evidence. Official GDK gameplay, successful official
online identities and Internet/NAT deployment remain unverified; the headless
audio backend does not establish audible playback.

## Reproduction and provenance

Use the [owner benchmark commands](../guides/world-profiling.md#repeated-owner-benchmark)
with matching source/hardware/toolchain/build settings. The comparator rejects
uncommitted source, changed helper/harness/workload and insufficient repetitions.
It reuses common resource-comparison rules while checking runtime requester counts.
CI runs functional/contract checks without timing unrelated hosted runners or
uploading artifacts. No release/package/tag is created.

Raw local records, not committed build artifacts:

- `/tmp/sculith-world-metrics-before-long.json` — SHA-256 `a27411c36b6dfb132713512d6b4bf127780e260ab3072c53fd54c1b9ea3595d1`.
- `/tmp/sculith-world-metrics-after.json` — SHA-256 `3a97c6e13c115283238e15900c0badba2baa6f081e3012e87cf25d1dd28ccf7c`.
- `/tmp/sculith-world-metrics-comparison.json` — SHA-256 `8345be9b1d457425ec7c7d5ffd15100af7e42bdfe6fa253b902816101b0cc4ac`.
- Instrumented `harness_sha256`: `3bb4c1def29a3433fa792f8b89cbf8605a0254678b333cc02b3e31cd8d6c32ff`.
- Instrumented `driver_sha256`: `b16d66635c89c8787b32355965c28fbda0f1ac32788a982fdfca8c8d61e3927c`.
- Instrumented `resource_helper_sha256`: `fed23735ed7cd810bdf13e532ed42cca267be799a901cbc59fd2127216459ae2`.
- Instrumented `benchmark_sha256`: `d363a4229dc9ca3e382ed178c728970ab6fa5a68f0fac4cb44f9997dc433139f`.

## Remaining work

1. Reduce the measured default metadata/envelope cost and isolate enabled-mode
   overhead under interleaved workloads before calling Monitor low cost.
2. Use real profiles to choose chunk encoding/loading or owner hot paths; add
   separate encoding/storage/CPU/allocator instruments instead of inferring them
   from mailbox wall time. Lease maintenance outside requests remains a gap.
3. Extend real entity/game behavior and spatial interest storage before choosing
   ECS, region migration/splitting and work stealing. Keep owner correctness and
   deterministic game rules as the boundary for parallel computation.
4. Simulation LOD, packet-only terrain LOD, adaptive background budgets and CPU
   flame graphs remain unimplemented. Periodic snapshots exist; WAL, unflushed
   durability, persistent players, broader gameplay/content and official online/
   GDK/Internet scenarios remain required by the active overall goal.
