# World profiler layout and mode costs — 2026-11-17 KST

This is a bounded continuation of the measurement/low-risk optimization phases.
The engine remains unfinished. No package, release, tag or executable artifact is
published. Earlier [world instrumentation](performance-2026-11-17-mailbox.md) and
[plugin results](performance-2026-11-17-framing.md) remain separate measurements.

## Analysis and implemented changes

The world profiler previously copied a generation and three Instant values through
every timed request, did not enforce separation between producer controls and owner counters,
used an atomic read/modify/write for a counter with one writer, and locked the timing
history before ordinary queue admission in enabled modes.

The runtime now uses one immutable monotonic epoch and a 24-byte optional stamp
containing a nonzero generation and two nanosecond offsets. Window-relative output
and the existing phase units/schema are preserved. Lifetime counters have their own
64-byte-aligned storage; the owner keeps its private saturating counts and publishes
with atomic stores. The exclusive mutable count borrow follows the one-owner task.
No producer updates those counts.

Admission reads atomic generation/deadline settings and never takes the history
mutex. The owner verifies the stamp generation under that mutex before recording.
Capture changes can omit/discard timing samples while game requests continue.
Conditional expiry cannot disable a newer capture. Counter publication for timed
work remains ordered with history recording. This is metadata optimization, not
parallel region computation or a CPU-cache-miss profile.

Existing game ownership, 32-slot queue/128-event bounds, cancellation, lease cleanup,
validation, permissions, SDK v1–v11 messages, Minecraft protocols and storage remain
unchanged. Lease Drop stays untimed and nonblocking. No dependency, crate or unsafe
code is added. This does not introduce entity ECS, RMT, LOD or a job scheduler.

Changed files across this stage:

- `crates/sculith-runtime/examples/mailbox_bench.rs`
- `crates/sculith-runtime/src/lib.rs`
- `crates/sculith-runtime/src/profiling.rs`
- `crates/sculith-runtime/src/profiling_tests.rs`
- `docs/development/performance-2026-11-17-profiler-layout.md`
- `docs/development/status.md`
- `docs/guides/world-profiling.md`
- `scripts/bench/analyze_runtime_modes.py`
- `scripts/bench/runtime_mailbox.py`
- `scripts/bench/test_analyze_runtime_modes.py`

## Compiled footprint

A pinned-toolchain Linux x86_64 layout diagnostic in the existing Off-path test
measures the actual internal types:

| Type | Before, bytes | After, bytes |
| --- | ---: | ---: |
| Timing stamp / optional stamp | 56 / 56 | 24 / 24 |
| Command envelope | 176 | 144 |
| Shared profiler metadata | 2728 | 2816 |

The owner also has a private 16-byte count pair. A full set of 32 envelope payloads
uses 1,024 fewer bytes, excluding channel headers, producer futures, allocator/page
rounding and dynamic history. The 88-byte shared-metadata increase is retained in
the report. These are internal layout measurements, not an ABI promise, allocation
count or process-RAM reduction. A 24-byte stamp budget and control/counter separation
are regression checked; history remains bounded to 128 recent/512 detailed records.

## Matched studies and throughput observations

Before: `022040ed137a194cfbff26b343764988f8eadfe7`, with the preceding production runtime and the new
mode-aware harness. After: `702c71a8062ec70b15baf7c273db246c50b4ed74`.
Both are committed and clean, with identical harness/driver/helper hashes and mode
orders. Rust 1.100.0, release/thin-LTO, Linux x86_64 AMD EPYC 9V45, five visible logical
CPUs, a four-CPU cgroup quota and 32 GiB memory limit are unchanged. No other local
build/GUI/test workloads ran during measurement; the shared host remains uncontrolled.

Each implementation has five repetitions of four cases in each of three modes:
60 measured phases, each with 1,000,000 actual owner requests after 256 warmups.
One owner/two Tokio workers serve one or 32 requesting tasks over sixteen resident
chunks at revision zero. These are immutable snapshot/capture requests, not players,
game simulation, network traffic or disk I/O. An untimed completion barrier verifies
finished requests and mode-specific bounded history; no dropped work is counted as
speed. Latency bookkeeping is included in timing; startup/shutdown, result sorting
and the completion/metadata checks are outside wall/CPU timing.

Modes rotate from a seed-7 permutation per repetition. This balances mode position
and permits paired enabled/Off ratios. Implementations are separate before/after
cohorts, so mode interleaving does not establish a causal attribution or confidence
interval for the implementation patch. Table values are medians of per-run results.

| Mode / case | 1,000,000 requests, ms | Wall change | p95, µs | p99, µs |
| --- | ---: | ---: | ---: | ---: |
| off / capture-one | 1497.863 → 1519.371 | +1.44% | 4.927 → 5.018 | 5.969 → 5.999 |
| off / capture-thirty-two | 533.506 → 523.844 | -1.81% | 26.239 → 25.868 | 34.882 → 36.905 |
| off / snapshot-one | 1094.307 → 1081.163 | -1.20% | 4.617 → 4.607 | 5.819 → 5.959 |
| off / snapshot-thirty-two | 264.141 → 264.871 | +0.28% | 14.812 → 14.842 | 19.123 → 21.403 |
| monitor / capture-one | 1725.450 → 1724.879 | -0.03% | 4.878 → 4.838 | 6.329 → 6.139 |
| monitor / capture-thirty-two | 593.973 → 600.917 | +1.17% | 28.233 → 28.813 | 41.593 → 47.731 |
| monitor / snapshot-one | 1472.588 → 1418.853 | -3.65% | 5.048 → 4.917 | 6.139 → 6.780 |
| monitor / snapshot-thirty-two | 361.906 → 346.322 | -4.31% | 17.442 → 17.016 | 28.312 → 30.216 |
| profile / capture-one | 1722.830 → 1750.126 | +1.58% | 4.877 → 4.857 | 5.999 → 6.340 |
| profile / capture-thirty-two | 600.506 → 582.487 | -3.00% | 27.962 → 27.682 | 38.488 → 38.538 |
| profile / snapshot-one | 1435.787 → 1417.274 | -1.29% | 4.927 → 4.997 | 6.049 → 6.099 |
| profile / snapshot-thirty-two | 362.502 → 345.019 | -4.82% | 17.425 → 16.956 | 24.206 → 25.448 |

Snapshot work in Monitor is 3.6–4.3% faster; Profile snapshot work is 1.3–4.8% faster
in this cohort. Default Off ranges from 1.8% faster to 1.4% slower, with no consistent
speed gain. Capture results remain mixed, including 1.2%/1.6% increases in some
Monitor/Profile cases. Tail increases are retained in the table. No comparison
exceeds the default 20% wall/30% tail guard, which is not a production acceptance SLO.

The intermediate compact-layout-only candidate `77c56aa` did not show a consistent
throughput improvement; it includes a 4.2% Profile snapshot-one slowdown and observed
PSS increases up to 37.7%. Its raw measurements are retained. The final candidate
also changes the counter/producer paths; these cohorts do not isolate each change's
individual contribution. No cache-miss or hardware-counter improvement is claimed.

## Enabled mode costs and CPU/memory

Each cell below is the median paired wall-time change against Off in the same
repetition, before → after. Ratios of whole-study medians are not substituted:

| Case | Monitor / Off | Profile / Off |
| --- | ---: | ---: |
| capture-one | +14.8% → +15.3% | +15.2% → +15.2% |
| capture-thirty-two | +13.1% → +14.3% | +12.4% → +10.3% |
| snapshot-one | +33.9% → +29.0% | +31.2% → +29.7% |
| snapshot-thirty-two | +35.4% → +27.7% | +37.2% → +27.5% |

Enabled measurement still costs roughly **10–30%** in these very short owner kernels.
It is not negligible, and cannot be applied as a universal percentage to a full game
server. Further reduction and real workload profiles remain necessary. Off's cost
relative to the original uninstrumented runtime was measured in the preceding report;
this harness changed, so those older raw results are not silently compared here.

Process CPU uses 10 ms user+system ticks for all benchmark/owner/requester threads.
Memory values are medians of sampled per-run peaks, in KiB:

| Mode / case | Process CPU, s | Observed RSS, KiB | Observed PSS, KiB |
| --- | ---: | ---: | ---: |
| off / capture-one | 1.76 → 1.77 | 25580 → 25736 | 24867 → 25023 |
| off / capture-thirty-two | 0.99 → 0.97 | 25980 → 25760 | 25267 → 25047 |
| off / snapshot-one | 1.27 → 1.27 | 9948 → 9976 | 9235 → 9263 |
| off / snapshot-thirty-two | 0.50 → 0.50 | 9016 → 9428 | 8303 → 8715 |
| monitor / capture-one | 2.00 → 1.99 | 20768 → 21660 | 20055 → 20947 |
| monitor / capture-thirty-two | 1.10 → 1.13 | 20876 → 22316 | 20163 → 21603 |
| monitor / snapshot-one | 1.72 → 1.65 | 10008 → 10412 | 9295 → 9699 |
| monitor / snapshot-thirty-two | 0.67 → 0.64 | 9800 → 9672 | 9087 → 8959 |
| profile / capture-one | 1.98 → 2.01 | 23080 → 25164 | 22367 → 24451 |
| profile / capture-thirty-two | 1.12 → 1.10 | 23080 → 25164 | 22367 → 24451 |
| profile / snapshot-one | 1.68 → 1.65 | 10096 → 10036 | 9383 → 9323 |
| profile / snapshot-thirty-two | 0.68 → 0.64 | 9616 → 9912 | 8903 → 9199 |

Snapshot CPU falls 4.1–4.5% in Monitor and 1.8–5.9% in Profile. Other CPU results are
mixed; Off varies from -2.0% to +0.6%. Process PSS also varies, including increases
up to 9.3%; no process-memory reduction is established. The sampler observes at
50 ms and includes one million latency values, allocator behavior and the untimed
result snapshot. It can miss short peaks. Allocation counts/bytes, fragmentation,
cache misses, CPU call trees and flame graphs remain unmeasured. These are not TPS,
player-capacity or multi-region scaling results.

## Correctness and compatibility

Local source passes 265 project tests (two release-only benchmarks ignored),
workspace format/check/Clippy with warnings denied, ownership doctests, debug/release
builds, architecture checks and eight Python benchmark-contract tests. Complete
workflows on exact runtime source `702c71a` pass:
[Linux](https://github.com/SKYRPG8957/Sculith/actions/runs/43989241779) and
[native Windows](https://github.com/SKYRPG8957/Sculith/actions/runs/43989241702),
including Windows static-CRT release builds and active mixed-edition profile checks.
These do not run the official Windows game client.

Clock tests
cover nanosecond phase sums, capture-relative origins, stale generations, exhaustion
and saturation. Admission succeeds while the history lock is held; stale expiry
cannot disable a new profile/monitor. A concurrent test reconfigures 1,500 times
while 4,000 world reads finish, then verifies final-capture counts and history.

Existing full-queue/cancelled-caller/lease-drop tests pass. Real Rust fixtures and
independent Java/native sockets pass simultaneous captures, epoch expiry/shutdown,
private data omission, no-clobber output and game/world/recovery/content checks.
No protocol authentication or data validation is bypassed for measurement.

An unmodified Java 26.3 game on exact final runtime source loads the 11,911-byte pack,
displays the shared model and all four form controls, edits/submits with actual
mouse/keyboard inputs and receives `LiveName / false / 1 / 5`. Both profilers are
active; the world capture has 46 timed/47 lifetime handlers without input/player
payloads. Owned display/game/server processes exit. Capture directory:
`/tmp/sculith-compact-stock-java/`; server SHA-256
`e6cccd3c5523531d3c741d7c8f53c00c1487a90ecf2a9f00633da4c9cf63d976`;
submitted image SHA-256 `5bfd2961ec4faa21c8626e608614c0aa12092c838b77f4afd0459e5aa2921fdf`.
This is Linux offline-loopback game evidence. Official GDK gameplay, successful
official online identities and Internet/NAT remain unverified. The headless audio
backend does not establish audible playback.

## Reproduction and raw provenance

Use the [mode study commands](../guides/world-profiling.md#repeated-owner-benchmark)
with the same source, compiler, build settings and hardware. Analysis rejects dirty
source, changed workloads/hashes, missing/duplicate mode cells, unfinished work,
expired captures or unbounded history. CI runs functional/contract checks without
imposing resource thresholds across unrelated runners or uploading artifacts.

Local raw records, not committed binaries or release artifacts:

- `/tmp/sculith-compact-before.json` — SHA-256 `21629798a7fb06d93a5c6427ce3bf16905ace8a23109f65c45135cc8bb4cd4a0`.
- `/tmp/sculith-compact-after.json` — SHA-256 `00ecc71271bc0bac20095fd8d5296eb0c74f71f0b97f7b82a3aef8cf94e86131`.
- `/tmp/sculith-compact-comparison.json` — SHA-256 `072e743e9806777b818f483fc108bf3e2a79adabacdee0a4bb0231af035c9e33`.
- `/tmp/sculith-compact-published.json` — SHA-256 `5757ace1850017c8fb72628b60cfaaee99ee067d18bbf5c1f9d92985a5ac4a4c`.
- `/tmp/sculith-compact-published-comparison.json` — SHA-256 `961e4aa63b53793d2c27666f680a64a8f5e416b412c19b8e6f017703a3692344`.
- Final `harness_sha256`: `e38b9ba6bdfbf6dd2f3b7e11a53066d470b0732d2f1c89f5116a9936a8a2f835`.
- Final `driver_sha256`: `9409a86f8318d6b0754fc7bffb74bec0d4b20fd3e1079c8d948711b7909af614`.
- Final `resource_helper_sha256`: `fed23735ed7cd810bdf13e532ed42cca267be799a901cbc59fd2127216459ae2`.
- Final `benchmark_sha256`: `e17ebfac08b545167d169bd192247d312374fa62a4e9fcb33e0543244aba64bf`.

## Next priorities and limits

1. Measure Java/native chunk encoding/loading and real owner workloads separately;
   use these data to select the next optimization rather than extrapolating tiny
   metadata kernels. Measure allocator behavior and CPU hotspots directly.
2. Retain the known default/capture/tail/memory regressions and enabled-mode cost.
   Profiling is an opt-in diagnostic, not a zero-cost replacement for CPU sampling.
   Plugin-mode overhead studies remain separate from these world-mode results.
3. General entities/game rules, spatial interest storage and deterministic ownership
   must precede ECS/region migration/work stealing. Simulation LOD, packet-only
   terrain LOD and adaptive background control remain unimplemented.
4. Periodic region snapshots exist; WAL/unflushed durability/player persistence,
   broader gameplay/content and official account/GDK/Internet verification remain
   required by the active overall goal. No complete-engine/crossplay claim is made.
