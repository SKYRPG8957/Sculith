# Plugin IPC profiling

Profiling works without a management GUI. Default **OFF** keeps only request,
complete-frame byte, message encoding and failure counters. It adds no clock reads per request
or history allocation. No game/network packet format changes.

For rolling timing statistics (**MONITOR**):

```sh
sculith run --config sculith.toml --monitor-plugins
```

Every 30 seconds the console reports each plugin's calls, failures and recent
p95 wall time in nanoseconds. Each plugin retains at most 128 latency values;
there is no detailed request history in this mode.

For a bounded detailed capture (**PROFILE**):

```sh
sculith run --config sculith.toml --profile-plugins 10 --profile-output profile.json
```

The duration is 1–300 seconds. The parent directory must exist and the output
must be a new file. The server continues running after capture; the report writer
runs outside the coordinator and region owner. An existing output is never
replaced. If shutdown interrupts a capture, `finished_reason` is `shutdown`;
a completed duration reports `duration`. Output errors are logged without stopping
the game. The optional writer is joined during shutdown.

The JSON document has `schema_version: 1` and `scope: plugin_ipc_wall_time`.
`dispatcher` measures the whole host dispatch, including concurrent requests and
action application. Each `plugins[].ipc` record measures request encoding plus
pipe write/flush, reply wait and JSON decoding, excluding action application.
Wall time includes OS scheduling/waiting; it is **not plugin CPU time**.

- `calls`, failures, timeouts and complete-frame bytes are lifetime counters,
  including the initial handshake. Partial failed writes/replies are not byte
  traffic counters. They are not Minecraft network bandwidth measurements.
- `timed_calls`, `encode_ns`, `wall_ns` and `maximum_ns` cover the selected timing
  window. Do not divide them by lifetime `calls`.
- Recent p50/p95/p99 use exact nearest-rank selection over the last 128 timed calls.
- Detailed mode retains the last 512 samples per plugin and dispatcher. Each sample
  records request ID, hook, timing, byte counts and failure flags; overwritten
  sample counts are explicit. An in-flight request started inside the window can
  finish after its deadline. History storage is released after report capture.
- `message_encodes` counts actual attempts to serialize a full message body.
  It is a lifetime counter in each aggregate, and per-request in detailed samples.
  An event with several selected recipients is encoded once in the dispatcher;
  workers copy that body into their own request-ID envelopes. A single recipient
  serializes directly in its worker; no recipient performs no serialization.
  Handshakes count in the worker. Failed serialization attempts are included.
- Dispatcher `encode_ns` measures shared body serialization. Worker `encode_ns`
  measures its frame construction, including direct serialization when needed.
  Shared body time is outside the worker interval; do not sum concurrent wall times.
- Request/reply buffers retain capacity on reuse, growing lazily up to 64 KiB each.
  The shared event buffer has the same bound. Snapshot capacity fields expose
  this retained storage, not total process RAM or allocation counts. Up to 32
  loaded workers can retain 4 MiB plus 64 KiB for the shared body, excluding
  pipe buffers, decoded values, metadata and the allocator. Lengths are cleared
  after requests, including failures; capacity is retained until host teardown.
- No event `Clone` bound is required. Direct/shared framing is checked against
  the retained SDK wire bytes, including Unicode and maximum request IDs.
- Payloads, commands, input text, account tokens and addresses are not captured.
  Plugin names and worker PIDs identify local processes for separate OS analysis.

Embedded hosts can call `Plugins::monitor`, `start_profile` and `profile_snapshot`
without the CLI. The snapshot uses the same versioned metrics schema. No remote
metrics listener or Manager dependency is introduced.

This is the initial plugin/dispatcher instrument, not a CPU sampling profiler.
Region queue/tick/encoding metrics, allocator tracing, flame graphs, global call
trees and adaptive scheduling remain unimplemented. The server's plugin Tick
clock is not a complete game simulation tick.

## Repeated dispatch benchmark

```sh
cargo build --release --locked -p sculith-profile-fixture
cargo build --release --locked -p sculith-plugin-host --example dispatch_bench
python3 scripts/bench/plugin_dispatch.py --iterations 5000 --repetitions 5 --output before.json
# Repeat the same command after an isolated implementation change, using after.json.
python3 scripts/bench/compare_plugin_dispatch.py before.json after.json --output comparison.json
```

The resource driver requires Linux `/proc`. It runs real Rust workers with 1, 8
and 32 processes, warms 128 dispatches and tests one/all/no subscribed workers.
It records latency quantiles, host/worker process CPU ticks and periodic RSS/PSS.
Prepared event values are outside the timed loop but remain part of benchmark
memory. These are IPC measurements, not game TPS, player capacity or RMT scaling.
Short cases can finish between memory samples; missing measurements stay missing.

Comparison rejects different hardware, toolchains, fixture/harness hashes or
workloads and requires at least three repetitions. Wall/tail thresholds are
configurable. A shared cloud runner is not a controlled long-term performance lab;
timing thresholds are not imposed against unrelated GitHub-hosted hardware.
Build, lifecycle, wire equivalence, delivery counts and bounded-capture checks
run in CI. No benchmark files or binaries are uploaded.

[Event-copy measurements](../development/performance-2026-11-17.md) and
[shared framing measurements and limits](../development/performance-2026-11-17-framing.md).
