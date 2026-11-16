# Bounded plugin framing — 2026-11-17 KST

This completes another measured part of Phase 2. It does not complete the engine
or the optimization directive. The broader architecture analysis and earlier
copy-removal results remain in [the preceding report](performance-2026-11-17.md).
No release, tag, executable or package is published.

## Bottleneck and implementation

After removing event clones, the host still serialized the same event separately
for each selected worker and allocated fresh request/reply vectors on each call.
Sparse subscriptions also created asynchronous placeholders for unselected workers.

The host now selects subscribers and private owners first. No recipient skips IPC;
one recipient serializes directly and bypasses fan-out futures. Several recipients
share one immutable encoded body but retain independent request-ID envelopes.
Only selected workers enter the concurrent wait; results still apply in configured
order. Game callbacks select their canonical owner before encoding.

Request/reply buffers grow lazily and retain capacity for subsequent calls. Every
append is bounded before growth, including the request-ID envelope and newline.
Shared body encoding uses the same bounded writer. Success and failure clear lengths;
responses own their decoded values. The buffers do not survive host teardown.

Safe Rust, the existing SDK/API v1–v11 negotiation, compact JSON-lines wire bytes,
64 KiB frame limit, checked IDs, timeout policy, action validation, quotas, owner
mailboxes, authentication and Minecraft protocol adapters remain unchanged.
No dependencies or crates are added. Capacity and actual message-serialization
attempt counters are additive fields in the existing profile schema.

Changed files across this phase:

- `crates/sculith-plugin-host/src/framing.rs`: bounded reusable storage and direct/shared envelopes.
- `crates/sculith-plugin-host/src/lib.rs`: selected dispatch, independent IDs, buffer lifecycle and profile snapshots.
- `crates/sculith-plugin-host/src/profiling.rs`: serialization attempts and retained-capacity metrics.
- `crates/sculith-plugin-host/src/tests.rs`: wire, boundaries, ownership, ID, timeout and failure regressions.
- `tests/interop/profile_smoke.py`: real Rust fan-out and profile lifecycle on both CI platforms.
- `docs/guides/profiling.md`: metric attribution and memory bounds.
- `docs/development/status.md`: current scope and limits.
- `docs/development/performance-2026-11-17-framing.md`: this measurement record.

## Repeatable comparison

Before: `d6f8e5492411ba3853f2b4dfb0dc211a380aee01`.
After: `2e2067b076d718510aec5b978f656e33852a46d6`.
Both were committed and clean during measurement. Same pinned Rust 1.100.0,
release/thin-LTO, AMD EPYC 9V45 host, five visible logical CPUs with a four-CPU
cgroup quota and 32 GiB memory limit. No other local builds/GUI probes ran during
these measurements; the shared cloud host is not a controlled performance lab.

The unchanged harness uses real Rust child processes, 128 warmups, 5,000 measured
dispatches and five repetitions per case. Large events contain 64 block changes.
The table reports medians of whole-run wall time and per-run latency quantiles.
Startup, shutdown and input construction are outside timing; dropping prepared
inputs, IPC, callback decoding and action handling are inside the dispatch loop.

| Case | 5,000 dispatches, ms | Wall change | p95, µs | p99, µs |
| --- | ---: | ---: | ---: | ---: |
| tick-one-of-thirty-two | 134.096 → 104.639 | -22.0% | 32.559 → 25.278 | 52.589 → 55.864 |
| world-all-thirty-two | 2638.546 → 2215.382 | -16.0% | 685.030 → 615.166 | 1493.267 → 1317.281 |
| world-none-of-thirty-two | 35.227 → 11.845 | -66.4% | 6.420 → 1.533 | 8.392 → 1.923 |
| world-one-of-eight | 267.617 → 284.788 | +6.4% | 72.509 → 79.280 | 109.726 → 117.968 |
| world-one-of-one | 279.770 → 289.737 | +3.6% | 72.940 → 77.036 | 112.569 → 120.842 |
| world-one-of-thirty-two | 248.934 → 254.443 | +2.2% | 67.151 → 64.808 | 105.288 → 99.570 |

The comparator reports no regression above its default 20% wall/30% tail limits.
Those limits are a guard, not evidence that all paths improved. Large single-recipient
cases remain **2.2–6.4% slower** in whole-run wall time; 1/1 and 1/8 tails also rise.
Tick p99 rises 6.2% despite lower median wall/p95. These observations are retained.
The five samples do not establish statistical confidence or a production SLO.

The first candidate `40d38df` regressed 1/32 wall time by 30.1% and 1/1 p99 by 51.5%,
failing the guard. Filtering futures (`d570a61`) reduced the measured 1/32 wall
regression to 11.0%; the direct single-recipient path reduced it to the final 2.2%.
The candidates and failed results are not hidden or averaged into the final cohort.
Individual code effects are not isolated by these sequential cloud runs.

## CPU, memory and concurrency

For 32 subscribed workers, median **host process CPU** falls from **2.06 to 1.16 s
(43.7%)** per 5,000 dispatches. Worker process CPU sums are 6.39 → 5.93 s; their
binary and received bytes are identical, so this is not a worker-code improvement.
For a single worker, host CPU instead rises 0.10 → 0.13 s while worker CPU is
0.14 → 0.13 s. CPU counters have 10 ms resolution and differ from wall/wait time.

For 32 subscribed workers, observed host RSS is 44,332 → 44,452 KiB and total
PSS is 51,373 → 51,477 KiB, about **0.2% higher**. Total RSS is 93,584 → 93,684 KiB.
For 1/32, host RSS is 44,048 → 44,120 KiB and total PSS is 49,090 → 49,185 KiB.
Memory therefore stays approximately flat in this workload; no RAM reduction is
claimed. The driver samples at 50 ms, includes prepared input values and is not
an exact high-water or allocator trace. The no-recipient cases are too short for
three memory observations and remain unreported rather than filled with zeros.

After warmup plus 5,000 full-fan-out dispatches, actual counters show 5,128 shared
body encodes and 32 worker handshakes. Reused frame storage totals 310,454 bytes
plus a 6,144-byte shared buffer in that sample. Each request/reply buffer can retain
up to 64 KiB, so 32 loaded workers have a worst-case 4 MiB plus 64 KiB shared-body
capacity, excluding pipe buffers, decoded values, metadata and allocator overhead.
This removes fresh host frame-vector creation; it is not zero-allocation IPC.
Allocation counts/bytes, fragmentation and cache misses have not been measured.

Concurrent IPC waits and separate Rust processes remain implemented. This is not
parallel world computation: no region scaling, RMT speedup, game TPS, player
capacity or entity throughput is established by this benchmark.

## Profiling modes

Additional five-repetition runs use the exact same final binary, fixture, workload,
hardware and clean source, intentionally changing only Off/Monitor/Profile.

| Case / metric | Off | Monitor | Profile |
| --- | ---: | ---: | ---: |
| 32 subscribed: wall ms | 2215.382 | 2170.352 | 2177.242 |
| 32 subscribed: host CPU s | 1.16 | 1.06 | 1.19 |
| 1/32 large: wall ms | 254.443 | 266.821 | 259.411 |
| no subscriber: wall ms | 11.845 | 12.350 | 12.114 |

These sequential cohorts have visible scheduler variance; lower observed times
in an instrumented mode do not imply negative or zero profiling overhead. The
1/32 large-event observations rise 4.9%/2.0%, and no-recipient time rises 4.3%/2.3%.
A reliable general overhead percentage still needs interleaved controlled runs.
Off retains no timing history; Monitor has 5,128 timed calls and no detailed
samples; Profile retains 512 dispatcher samples and reports 4,616 overwritten,
with each worker independently bounded at 512. CPU flame graphs and an allocator
profiler remain separate work.

## Correctness and compatibility evidence

Local Linux runtime source passes 253 project tests (two release-only benchmarks ignored),
workspace format/check/Clippy with warnings denied, ownership doctests, architecture
checks, workspace debug/release builds and three benchmark-comparator contract tests.
Tests compare direct/shared requests byte-for-byte with SDK requests for Unicode,
escaping and maximum IDs; enforce exact 64 KiB boundaries, fragmented/truncated
replies, mismatched IDs, timeouts and non-wrapping exhaustion. An oversized shared
event writes nothing, disables selected workers and leaves other workers untouched.
Retained replies stay valid after later buffer reuse.

Full workflows on the exact final runtime commit succeed:
[Linux Rust checks](https://github.com/SKYRPG8957/Sculith/actions/runs/43973769378)
and [native Windows checks](https://github.com/SKYRPG8957/Sculith/actions/runs/43973769389).
Windows includes its static-CRT release build and independent plugin/protocol
probes; it does not run the official Windows game client.

Real Rust worker probes pass shared encoding and report expiry/shutdown/no-clobber
checks in debug and release. Independent Java/native tests pass choice/input forms,
private callbacks, models, exact sound assets, world reads/CAS edits, autosave and
forced restart, counted inventory, packet/connection hooks including retained v8,
and required pack delivery through an independent HTTP proxy. All 30 before/after
benchmark phases preserve every worker's delivery count and complete-frame byte totals.

A fresh **unmodified Java 26.3** profile on exact final source joins by address,
accepts/downloads the 11,911-byte pack, displays its original static model and all
four input controls, submits real mouse/key edits and receives the exact private
result `LiveName / false / 1 / 5`. No game packets are injected. Local capture:
`/tmp/sculith-framing-final-stock-java/`; server SHA-256
`4c47766533ff7ff11d6fffae62c394a00d0c92368f793467a01dbd154ef0eb76`;
submitted screenshot SHA-256
`26f9604beb10b2eae2a24be542e0ddaf880651f9050f8aa1c9343afebdbc100d`.
The screenshot is inspected and all owned processes exit. This is Linux offline
loopback evidence, not official Windows GDK/real-account/NAT verification. The
headless audio backend does not establish audible playback.

## Reproduction and artifacts

Use the unchanged [operator benchmark commands](../guides/profiling.md#repeated-dispatch-benchmark).
Build the standalone fixture and harness in that order; a workspace release build
can feature-unify the fixture differently. Compare matching binary hashes.
For mode studies add `--mode monitor` / `--mode profile`; the strict before/after
comparator intentionally rejects mismatched modes rather than treating them as
an implementation comparison. Raw results remain outside Git, not release artifacts:

- `/tmp/sculith-framing-before.json` — SHA-256 `23422986d64ff85a226b32b1d8b1099024f7badebeeda8448b5cca15cc89f592`.
- `/tmp/sculith-framing-after.json` — SHA-256 `df8ed7ef2945406c0edc28eaacbf97f0ad94991615d38fc6c0e8c813bde31efe`.
- `/tmp/sculith-framing-filtered.json` — SHA-256 `34d3d79e8b4284ce111397c64e39e76c96bf7cdc4dcb5a7b7502480fac31fe8f`.
- `/tmp/sculith-framing-direct.json` — SHA-256 `2d7708699e784ed7d8a732146211d41cd62a8ff99710c015388c47b00fb125a9`.
- `/tmp/sculith-framing-direct-comparison.json` — SHA-256 `9ebab1e9fb4b07597ede89c15bb2feceb18b5e0dee943bcf68c75841e195c8ef`.
- `/tmp/sculith-framing-monitor.json` — SHA-256 `7eaa6ba79b33f97d3c162b8140e9de6d57a9ce276474dbfb0cf10ac5aa0d93ca`.
- `/tmp/sculith-framing-profile.json` — SHA-256 `54f6f5d4adb26b378b170464bfcfa7ee8014d5989992d9b231be7c76a6ec5feb`.
- Final `fixture_sha256`: `1b095436b168797e497b279ef770ffcccc0cd21a24dd44bdef735f011b625f1c`.
- Final `harness_sha256`: `92fcf82ef398fb3eed170f06b82061ddcb9bc83a47446feca5b929242197f401`.
- Final `driver_sha256`: `fed23735ed7cd810bdf13e532ed42cca267be799a901cbc59fd2127216459ae2`.
- Final `benchmark_sha256`: `caff5fcc0c10e396a6ab70d013d56552a1a60841f13b0340fde9da3a8fe40be9`.

## Remaining work and priorities

1. Measure queue wait/owner work and Java/native chunk encoding separately before
   selecting the next bottleneck; retain the distinction between plugin Tick and
   a real game simulation clock.
2. Measure allocation counts and profiler overhead under controlled/interleaved
   workloads. Revisit the remaining single-recipient wall/tail regressions.
3. Extend real entity/world behavior and spatial interest indexing before choosing
   ECS storage, region split/migration or work stealing. One region remains the
   correctness boundary; Tokio I/O tasks alone do not satisfy RMT.
4. Simulation LOD, packet-only terrain LOD and adaptive background budgets remain
   unimplemented. They need correct simulation/ownership, measured jobs and verified
   stock-client presentation rather than empty abstractions or dropped game ticks.
   Recovery already has periodic snapshots; unflushed durability, WAL and player
   persistence remain separate work. Official online/GDK/Internet scenarios and
   broader gameplay/content remain required by the active overall goal.
