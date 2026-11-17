# World mailbox profiling

World profiling separates delay before queue admission, delay inside the bounded
mailbox and the owner's command handling. It works independently of plugins and
a management UI. It does not measure CPU samples or a game simulation tick.

```sh
sculith run --config sculith.toml --monitor-world
sculith run --config sculith.toml --profile-world 10 --world-profile-output world.json
```

Monitor prints command totals, occupied channel capacity and recent queue/service
p95 every 30 seconds. Profile captures for 1–300 seconds and writes a new JSON file.
The parent directory must exist; existing files are never overwritten. The server
continues after capture. For simultaneous plugin and world captures, choose separate
files:

```sh
sculith run --config sculith.toml --profile-world 10 --world-profile-output world.json --profile-plugins 10 --profile-output plugins.json
```

The report uses `schema_version: 1`, `scope: world_mailbox_wall_time` and the region
ID. Commands are classified by fixed operation names. Player identities, plugin
names, commands, chat, coordinates, form values and account data are excluded.

## Measurements

Each timed command records monotonic wall durations in nanoseconds:

- `admission_ns`: waiting for a mailbox permit after the admission timer begins.
- `queue_ns`: sending the envelope through receiving it in the owner.
- `service_ns`: the owner branch, including its post-receive dead-lease check,
  game decisions, publications and reply send, before statistics recording.
- `total_ns`: the sum of those phases. It excludes the caller's later wakeup and
  response handling, network encoding/sending, disk writes and work outside this
  command branch. OS preemption can increase all wall durations.

`completed_commands` counts lifetime ended handlers, including `aborted_commands`.
An abort is early return/unwind from the owner branch; ordinary game rejections
and a dropped reply receiver still count as normally ended handlers. Pre-admission
cancellation/closed-channel failures do not create samples. Already queued work
keeps its existing execution/lease validation behavior after caller cancellation.
Best-effort retirement in `PlayerLease::drop` remains nonblocking and untimed;
its handled commands still contribute to lifetime counts. Dead-lease maintenance
outside a received command is not separately timed.

`totals` and per-operation aggregates cover the current timing window, not all
lifetime commands. Recent component-wise p50/p95/p99 use exact nearest-rank
quantiles over at most 128 completed timed commands. Detailed mode retains the
last 512 samples globally for the owner and reports overwritten samples. Monitor
retains no detailed samples. Do not treat a component-wise percentile as the
phases of one particular request.

The mailbox still has 32 slots. `occupied_capacity` includes sender reservations
and excludes the command in service; it is not an exact waiting-message count.
Samples also record the number waiting immediately after receive.
`maximum_remaining_queue_depth` is a sampled observation, not a continuous queue
high-water mark. A snapshot is metadata observed concurrently with the owner,
not a world checkpoint or a barrier for unfinished work.

## Capture lifecycle and cost

Default Off keeps a lifetime handler/abort count but performs no profiler clock
reads, operation classification or history allocation per request. The owner updates
its private counts and publishes them with atomic stores. Producers read capture
generation/deadline settings atomically and do not acquire the history mutex.
The owner records enabled timings under that mutex; game state remains owned by
its existing task. A profile snapshot copies bounded records under the same lock,
then computes quantiles outside it. Profiling adds work and can contend; its cost
must be measured rather than assumed negligible.

Admission inside the capture window selects a request for timing. Its completion
can happen after the deadline. Once the deadline passes, new admission timestamps
stop; the report contains work completed before its snapshot. Reconfiguring a
capture discards old-generation samples while preserving game work. Requests
racing reconfiguration may have no timing stamp; their admission and game execution
continue normally. Expiry uses a generation check so an old producer cannot disable
a newer capture. History is released when the CLI captures the report and returns
to Off.

The coordinator polls exports on its existing plugin clock; a slow callback can
delay export, although admission timestamps respect the requested deadline. This
clock is not a full world simulation tick. One optional blocking writer saves the
file outside the owner/coordinator and is joined on shutdown. Write errors are
logged without stopping the world. Early shutdown uses `finished_reason: shutdown`;
a completed duration uses `duration`.

Rust embedders can call `WorldHandle::monitor`, `start_profile` and
`profile_snapshot`. This is one owner per region handle, not an implementation
of region scheduling, multi-region simulation or adaptive load control.

## Repeated owner benchmark

The Linux resource driver needs `/proc`:

```sh
cargo build --release --locked -p sculith-runtime --example mailbox_bench
python3 scripts/bench/runtime_mailbox.py --iterations 1000000 --repetitions 5 --output before.json
# Repeat after an isolated implementation change using after.json.
python3 scripts/bench/compare_runtime_mailbox.py before.json after.json --output comparison.json
```

The harness uses real bounded request/reply channels with one owner,
two Tokio workers, one or 32 requesting tasks and sixteen resident chunks. It
measures immutable chunk snapshots and region captures without gameplay edits,
networking, plugins or disk I/O. Requesting tasks are not simulated players or
parallel regions. Startup/shutdown are outside timing; request scheduling and
latency storage are included. RSS/PSS samples include that latency storage and
are interval observations, not allocation counts or exact peaks. An untimed
completion barrier and metadata snapshot verify that all measured work ended and
the selected mode kept its documented history limits.

For a balanced mode study, repeat the same build/measurement before and after a
change using separate result files:

```sh
python3 scripts/bench/runtime_mailbox.py --iterations 1000000 --repetitions 5 --mode all --order-seed 7 --output before-modes.json
# Repeat after the change using after-modes.json.
python3 scripts/bench/analyze_runtime_modes.py before-modes.json after-modes.json --output modes-comparison.json
```

Each repetition rotates Off/Monitor/Profile after a seeded permutation, giving
balanced positions across repetitions. The analysis pairs each enabled-mode run
with Off in the same repetition and separately compares implementations within
identical modes. It rejects missing/duplicate cells, expired captures or incomplete
work. These observations are not statistical confidence intervals or game TPS.

Comparison rejects changed hardware/toolchains, dirty source, different harness,
driver or sampling-helper hashes, requesters and workload. Five repetitions are
the default; at least three are required. No benchmark binaries or reports are
uploaded by CI. Functional profiling and comparison-contract checks run on Linux
and Windows; resource timing thresholds require matching hardware.

[Measured default cost and verification limits](../development/performance-2026-11-17-mailbox.md).

[Compact timing records, mode costs and remaining regressions](../development/performance-2026-11-17-profiler-layout.md).
