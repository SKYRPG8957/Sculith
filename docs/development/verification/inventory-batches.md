# Native inventory batches — 2026-11-17 KST

Source: `aaca190b4061a25a2683b52b62db06f2bbfee116`.
Measurement baseline: `55d25da3a37275295f936b01ccae969b749ceba4`
(previous runtime plus the same ignored measurement harness).

## Reference and resulting behavior

REA 6.9.0 captured separate official BDS 1.26.63.1 processes on loopback,
using fresh private worlds and offline identities. The official binary SHA-256
is `dc9b3c566a014cf53930eb1b54c6b599a99a81cc32c68aba06a5d73a0fd63179`;
the original ZIP SHA-256 is
`b40b04c8f5a7c42a19ee685c22fc593e66116b9c22741b8c75489306b252ff62`.
The download source was
`https://www.minecraft.net/bedrockdedicatedserver/bin-linux/bedrock-server-1.26.63.1.zip`.
No binary, implementation or game asset is added to Sculith.

An external independent gophertunnel client pinned to `e2251a60f71d` submits
native protocol-2225 requests and records ordered incoming stack responses.
BDS accepts 1/2/36/64-action swap requests, with both negative prediction
aliases and positive stack references. Positive IDs move with stacks. In the
completed ordinary-slot transfer run:

| Operation | Source count / ID | Destination count / ID |
| --- | --- | --- |
| Initial | 36 / 2 | empty / 0 |
| Split one | 35 / 2 | 1 / 5 |
| Merge back | 36 / 2 | empty / 0 |
| Move whole stack | empty / 0 | 36 / 2 |
| Move back | 36 / 2 | empty / 0 |

Sculith previously replaced IDs on every occupied-slot change and rejected
requests larger than 32 actions. It now accepts up to 64 actions, retains the
source ID on partial removal, retains the destination ID on merge, moves the
original ID on a whole-stack transfer or swap, and allocates an ID when a split
creates a new stack. Java clicks, quick moves, collection, block picking and
plugin grants use these same owner rules. Counts, ownership, permission checks,
fresh decreasing request IDs and all-or-nothing commits remain enforced.

The first cursor-transfer reference attempt was rejected with destination/source
validation statuses 50/49; it does not establish BDS cursor compatibility. A
later ordinary-slot launch had no connected client, and another probe initially
omitted the required HTTP URL scheme. Those attempts were not used as positive
evidence. The successful reference uses ordinary slots; the Sculith cursor/grid
scenario below is independently tested against Sculith. This is runtime process
observation plus native wire analysis, not decompilation or GDK screen testing.

A follow-up reference sends `Interact(OpenInventory)` targeting the client's
own runtime entity ID. BDS replies with `ContainerOpen` (window 2, type 255,
entity -1), then accepts the cursor split and merge with the same 2/5 IDs in
the table above. The earlier zero-target request had not established this screen
context. Capture evidence is
`ev_ae8ad1eab00f9b1191ce49b1b7e7782f4f5196970b206afafeea5b39dd710bb2`;
`/tmp/sculith-native-inventory-reference/cursor/capture.json` has SHA-256
`1e25bfa1bdabaca5609047b16074401936cd5624fa649469f80263d421fc2a82`.
Its response log SHA-256 is
`997047071df5ea5388c3a32d6c31fa919a112afb45e025d22213eb8d6b77d661`.
This extends the protocol reference to the cursor after a valid open, without
establishing official GDK input/rendering. Sculith currently ignores inventory
open/close notifications; adding their bounded acknowledgements is the next
adapter task, before claiming a complete native screen lifecycle.

A further close/reopen run retains Grass35/ID2 in the ordinary slot and
Grass1/ID5 on the cursor. BDS acknowledges client close for window 2 (responses
with type bytes 255 and 247, both server-side=false), then opens window 3. A
subsequent merge using the original cursor ID5 succeeds, restoring Grass36/ID2.
Closing alone therefore did not return or destroy the cursor in this scenario.
An earlier helper incorrectly waited for such a return and timed out; the
corrected probe tests the retained cursor directly after reopening. Meaning of
the additional type-247 close response is not established here.

The completed reopen evidence is
`ev_57a7c37fc4802335e439c132134779b0a1a84a35d55cbd9d7fa03703db3bf2d8`.
`/tmp/sculith-native-inventory-reference/cursor/reopen-capture.json` SHA-256 is
`2dc1d81d1c9bf352c6ccb7cc411b80b4848a96ac2eff63caae5b64deda1338cd`;
the `reopen-probe.log` SHA-256 is
`3c36ab38cab2243ef5270d366dba68af30721b399c68e19e805ff02c410ad4d4`.

Completed reference evidence:

| Local capture | REA evidence ID | Capture SHA-256 |
| --- | --- | --- |
| Initial prediction swaps | `ev_ceaf6e70d3a3e25785af94a93b9a385365ac2272d4ad1ebc472db3ee75218b53` | `5ee1e3dd7c40ab05026f056912521fb8e3a4a54e63bfb996f6a653c0059e7e10` |
| Positive swaps and ordinary-slot split/merge/move | `ev_6fec5444d5a2709d98a0e6733af7d74e34a721623718d15373a3cc487611946c` | `936854efb4e549fa6681c95de52114c35add6fb15d42c7ef92410430b0dd97a0` |

Raw files remain outside the product at
`/tmp/sculith-native-inventory-reference/capture-run1.json` and
`/tmp/sculith-native-inventory-reference/slots/capture-run4.json`.
The final native response log SHA-256 is
`d9cc96c8b7dba893c66b02dac01e1c4968ee670c2da426c81a87dbe10201b820`.
Its helper reconstructs item kinds from counts for display, so the table uses
only the counts and IDs decoded directly from responses. REA reports the owned
process groups, terminals and temporary capture roots cleaned after normal exit.

## Bounds and verification

The owner charges one existing work unit per 1–32 actions and two per 33–64.
The bucket remains 40 units/second with a 64-unit burst: maximum accepted action
work stays at 1,280/second and 2,048 in a burst. Replays are charged as well.
Wire requests above 64 actions normalize to a bounded rejection without retaining
their actions. The existing packet, envelope, mailbox and transport byte limits
remain in effect; an accepted request can now contain up to 1,280 action bytes
instead of 640 on this target.

Replay storage is bounded by both 64 requests and 2,048 total actions, including
failed requests. Saturated 64-action histories therefore retain 32 requests;
histories of requests up to 32 actions keep their former 64-request capacity.
Evicted IDs remain stale, and aliases to their responses cannot resolve.

Local validation passes 302 project tests, five ignored tests,
ownership doctests, Clippy, architecture checks and debug/release builds. Added
checks cover stable positive IDs across compound swaps, a 36-action full-grid
split, a 35-action cursor merge, distinct live IDs, rollback after a late error,
allocator exhaustion during actual stack creation, mixed cache eviction and
fixed-clock work/refill limits. Runtime checks publish one committed 37-slot
inventory event and the resulting hand update; replay publishes neither and
the region revision stays unchanged.

The independent Java/native socket probe passes in debug and release. It verifies
2/36/64-action positive-ID swaps, 65-action rejection, full-grid split/merge,
replay, late failure, resynchronization and continued control traffic. Existing
Java pickup/swap/quick-move/collection, peer hand counts and real Rust plugin
observations still pass. Plugin grant/query and block-pick probes also pass.
The official Windows GDK's generation of these gestures remains unverified;
general containers, drag distribution, crafting and persistent player state
are not added by this change.

```sh
. ./scripts/dev-env.sh
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
python3 scripts/check_architecture.py
python3 tests/interop/inventory_smoke.py --client target/gophertunnel-probe
python3 tests/interop/plugin_inventory_smoke.py --client target/gophertunnel-probe
python3 tests/interop/block_pick_smoke.py --client target/gophertunnel-probe
cargo test --release --locked -p sculith-game inventory_batch_measurement -- --ignored --nocapture
```

## Windows check follow-up

Initial Windows run [44033528126](https://github.com/SKYRPG8957/Sculith/actions/runs/44033528126)
passes the new game/runtime tests but fails the pre-existing checkpoint test's
three-second observation deadline. That helper performed blocking snapshot reads
on its current-thread async runtime and discarded read errors and worker failures.
The log does not identify which revision/read delayed; no production storage
failure or inventory cause is established.

The test helper now observes files on a blocking worker, reports checkpoint-worker
exit immediately, retains the last read error/revision and uses a ten-second CI
readiness deadline. Exact revision ordering and final-save assertions are kept.
The production checkpoint worker, save interval and storage format are unchanged.
The four checkpoint tests, the full server test suite and Clippy pass locally.

At follow-up source `3f8487ead8b47e96a9a14908db55ad5c9156dd3f`,
[Linux main](https://github.com/SKYRPG8957/Sculith/actions/runs/44034331216)
and [Linux branch](https://github.com/SKYRPG8957/Sculith/actions/runs/44034331184)
workflows pass. Every build/test/interoperability step in the
[Windows/static-CRT run](https://github.com/SKYRPG8957/Sculith/actions/runs/44034331149)
also passes, including periodic/manual saves and forced-termination recovery;
its cache finalization is still running at this record. Neither run has an
uploaded build artifact, and the repository has no release or tag.

## Work/history measurement

Five release runs use the pinned Rust 1.100.0 toolchain on the same Linux x86_64
cloud machine. Each builds/applies 20,000 even swap batches using current-request
aliases after the first action. Timing covers request construction, owner
inventory application and replay caching; it excludes game-state rate limiting,
mailbox, plugin IPC, sockets, encoding, rendering and world simulation.

| Source / actions per request | Wall milliseconds, five runs | Median |
| --- | --- | --- |
| Baseline / 32 | 14.935, 16.559, 14.981, 14.616, 14.511 | 14.935 |
| Candidate / 32 | 11.796, 12.008, 12.530, 11.673, 12.420 | 12.008 |
| Candidate / 64 | 21.575, 22.308, 23.199, 22.249, 21.671 | 22.249 |

| Measured retained layout | Baseline / 32 | Candidate / 32 | Candidate / 64 |
| --- | --- | --- | --- |
| Inventory object bytes | 352 | 360 | 360 |
| Cached entries | 64 | 64 | 32 |
| Cached actions | 2,048 | 2,048 | 2,048 |
| Action vector capacity bytes | 40,960 | 40,960 | 40,960 |
| Response vector capacity bytes, two changed slots | 1,536 | 1,536 | 768 |

The extra eight owner bytes hold the action counter. These layout figures
exclude allocator overhead and deque capacity; response storage depends on
changed slots, bounded at 38 per response. They are not process RAM figures.
Raw logs are `/tmp/sculith-inventory-{baseline,candidate}-{1..5}.log`.
Stack-ID outputs intentionally differ between sources, so the timing samples
are a compatibility-change work check, not an equivalent-output optimization,
server throughput result or comparison with complete server implementations.
