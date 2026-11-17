# Java cursor distribution — 2026-11-17 KST

Source: `6387910ba917d0d5edffd0efd18721d3841b673a`.
Baseline: `7edca2b87d8192a6033335e57e628c42254e4e25`.

Protocol-777 left and right drags now use owner-held cursor state. Start and
selection keep a bounded 36-slot mask without changing items; completion applies
one atomic candidate. Left drag divides the original count, retaining remainders;
right drag places one per selected slot. Matching full stacks participate in the
division but receive nothing. Incompatible slots are excluded and duplicate
selection adds no weight. A single target follows the ordinary pickup operation.

Closing menu 0, stale drag state IDs, changed modes/cursor identity, permission
loss, rate rejection and interrupted gestures cancel the selection. A rejected
completion preserves item counts and allocation state and discards the gesture.
Server grants/replacements also invalidate it. Normal stale single clicks retain
their prior owner-based behavior. Accepted start/add packets do not send full
contents over the stock screen's preview; completion/rejection resynchronizes.

## Reference and checks

The official Java 26.3 `AbstractContainerMenu.doClick`, quick-craft helpers and
container-close codec were inspected with `javap`. Server tool JAR SHA-1:
`33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c`. The button's low two bits encode the
stage; bits 2–3 encode the type. Supported sequences are 0/1/2 (left) and 4/5/6
(right), with slot -999 at start/end. Target slots map only to owned inventory
and hotbar. Close carries a VarInt container ID. Reference outputs remain in
`/tmp/sculith-collect-reference.txt`, `/tmp/sculith-drag-close-reference.txt` and
`/tmp/sculith-drag-buffer-reference.txt`; no implementation is copied.

315 project tests pass, with six manual checks ignored by default. Clippy,
ownership doctests, architecture checks and debug/release builds pass. Added
checks cover full-grid distribution, single targets, full/incompatible slots,
duplicate selection, remainders, source identity, cancellation, work limits and
rollback after a late ID-allocation failure. Runtime hooks publish one final
inventory batch and the resulting hand update; selection phases publish neither
and the region revision stays unchanged.

The independent Java/native socket probe passes in debug and release. It checks
forged hashes, stale phases, close cancellation and mode changes. The native
peer sees the Java hand become empty, then Grass21 after a three-target even
drag and Grass22 after a one-item drag. Real Rust inventory/equipment hooks and
Control classification for Java close ID19 pass. Plugin grant/query regression
checks pass. Registration counts and SDK version are unchanged.

## Stock creative screen

A fresh unmodified Java 26.3 profile connects to the exact-source debug server
without mods, server plugins or packs. Actual mouse input takes Grass64 from
the vanilla catalogue and drags it across two selected hotbar slots. The screen
shows Grass32 in each. The world profile records **nine creative-slot calls and
zero inventory-click calls**, consistent with the creative screen's bytecode.
This is a regression check for the existing creative-slot path; the wire probe
above exercises the new owner drag handler.

Artifacts are under `/tmp/sculith-drag-stock-java/`. Server SHA-256:
`c705c49b099590024fc4281bab60b0a52cfc18ce1bf80a472225380ad5b30e00`;
client JAR SHA-1: `e877b6a07acd633fb3bb475002175cec036e7b87`.
The `distributed.png` SHA-256 is
`67b2b55be3c954aba586457ffabfe0cd0ebc8f1d6f9e20c3561655ee26df406e`.
The helper reached its 240-second guard before a later observer setup; it cleaned
its Xvfb/server/client (exit 0/0/130). That observer then got connection refused,
so no separate peer assertion is claimed for the graphical run. This does not
verify a stock game's server-driven QUICK_CRAFT screen or official Windows GDK.

## Layout and kernel measurements

Pinned Rust 1.100.0, Linux x86_64, release mode. Before/after use distinct target
directories, with compilation and GUI/socket probes finished before timing.
Measurements run serially with `--test-threads=1`, five consecutive runs each.
Shared-target exploratory results were discarded after an old executable was
identified by its missing drag test and 360-byte layout; overlapped GUI/socket
measurements were also excluded. Those logs remain outside the product.

| Case | Wall milliseconds, five runs | Median |
| --- | --- | --- |
| Baseline, 20,000 × 32-action swaps | 16.199, 16.526, 16.108, 13.897, 12.276 | 16.108 |
| Candidate, 20,000 × 32-action swaps | 12.467, 14.275, 13.818, 25.917, 12.464 | 13.818 |
| Baseline, 20,000 × 64-action swaps | 27.998, 28.241, 28.252, 27.752, 21.865 | 27.998 |
| Candidate, 20,000 × 64-action swaps | 23.362, 23.219, 24.199, 55.616, 23.113 | 23.362 |
| Candidate, 10,000 × three-target gestures | 3.151, 3.006, 3.033, 4.662, 3.074 | 3.074 |
| Candidate, 10,000 × 36-target gestures | 16.564, 15.982, 16.476, 19.857, 16.488 | 16.488 |

The inventory object grows from **360 to 384 bytes**: the optional gesture is
24 bytes with no heap selection set. Existing replay payload stays at 40,960
action-capacity bytes for the saturated 32/64-action cases, with at most 64
requests/2,048 actions. The existing 40-unit/second, 64-unit-burst client budget
charges every phase; completion covers at most 36 transfers/37 changed slots.

Swap timing includes request construction/application/caching. Gesture timing
includes a fresh inventory, grant/pickup setup and start/add/finish operations.
Both exclude transport, mailbox, plugin IPC, rendering and world simulation.
Observed timing varies substantially; no speedup, process-RAM or server-TPS
claim follows from these samples. Raw accepted logs are
`/tmp/sculith-drag-isolated-{baseline,candidate}-{1..5}.log`, with summaries in
`/tmp/sculith-drag-measurements.json`.

```sh
cargo test --release --locked -p sculith-game --target-dir /tmp/sculith-drag-after-build measurement -- --ignored --nocapture --test-threads=1
python3 tests/interop/inventory_smoke.py --client target/gophertunnel-probe
```

Middle-button creative fill, drop entities, crafting, armour/offhand and general
containers remain unsupported. Player inventory recovery after reconnect/restart
is still pending; this does not complete the production-server objective.
