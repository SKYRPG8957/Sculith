# Pumpkin foundation

2026-11-17. Base: Pumpkin `87f8822ac36da62102350977be2f62f155a303dc`.

Sculith is moving to a Pumpkin fork to reuse its Rust game implementation,
world storage, generation, entities, inventories and Rust/Wasm plugin API.
The prior MIT implementation is preserved at Sculith `9ad4bf8`. Its original
history and license remain unchanged. The fork server remains GPL-3.0.

## Initial change

Upstream parsed an input-carried item-use payload with a separate structure
missing its hand byte. It also never dispatched `item_interaction` to gameplay.
The fork reuses the standalone item-use decoder and routes the decoded
transaction through the existing inventory handler. It preserves the bounded
input reader and the existing transaction fields and game rules.

The executable and discovery name are Sculith. Internal Pumpkin crate names,
Wasm interfaces, configuration names and upstream history are retained. Upstream
telemetry and update notifications default to off. CI builds and tests source;
it has no release, artifact-upload, container-push or automatic publication job.
The upstream Docker downloader and compose file are removed because they ran
upstream release binaries. This branch currently supports source builds.

## Verification

- Upstream Linux debug build passed on Rust 1.100.0.
- Official Java 26.3 created a flat Anvil test map and saved a diamond marker.
- The unmodified upstream server read that map. An independent gophertunnel
  client joined over NetherNet, picked the marker, placed it and broke it.
- The upstream input-carried placement test failed, with a truncated-payload
  error in the server log. The fork contains an independently encoded 2225
  regression fixture for this path.

The modified fork passed these Linux checks on Rust 1.100.0:

| Check | Result |
| --- | --- |
| Protocol unit tests | 173 passed |
| Configuration unit tests | 24 passed |
| Game/server unit tests | 803 passed |
| Shared resource converter unit tests | 14 passed |
| Plugin runtime unit tests | 8 passed |
| Experimental Wasm v0.2 host tests | 18 passed |
| World unit tests, including official 26.3 fixtures | 305 passed |
| Clippy for the changed packages and binary, all targets | Passed with warnings denied |
| Rust formatting, Go build and Go vet | Passed |
| NetherNet standalone and input-carried place/break | Passed |
| Negative predicted held-stack ID and input-carried slot journal | Passed |
| Saved marker read after a normal stop and restart | Passed |
| Official Java 26.3 reads the fork's saved map | Marker found; no ERROR log entries |
| Unsupported Bedrock source guard | Exit 1; source-file hashes unchanged |

The map test also exposed upstream storage errors: block palettes were written
with pre-26.3 `Name`/`Properties` keys, and saved game rules, clocks and generation
settings put `DataVersion` inside their payload. The fork writes current palette
keys and puts the version beside `data`. Clock loading retains compatibility
with the earlier Pumpkin layout.

Tests use a copied flat map with known marker blocks. They do not establish
full entity, block-entity or arbitrary-map conversion fidelity. The source map's
four region-file hashes remained unchanged. These are debug-build correctness
checks; they are not performance results or Windows GDK client verification.

Official readback was captured with REA 6.9.0. Versioned test fixtures are under
`crates/pumpkin-world/tests/fixtures/java26_3`; the native socket probe is under
`tests/interop/bedrock`. Reproduction commands are in that directory's README.

## Shared resource delivery

An opt-in shared source now compiles explicit Bedrock textures, sounds and static
cuboid models for Java 26.3. The converter comes from the former MIT Sculith
implementation and retains its MIT license. The server serves the Java archive
on its existing TCP port and transfers the native archive in Bedrock chunks.
Both archives are frozen before worlds open. See [configuration and limits](resources.md).

Independent Java and Bedrock clients passed archive/hash verification, native
spawn and all four building paths with the shared pack enabled. The Java check
also holds a fragmented acknowledgement across configuration keepalives and
checks that download completion alone cannot enter play. Altering source files
after startup did not alter either active archive.

A fresh, unmodified Java 26.3 client joined with the server address, remained at
the standard pack consent screen for about 90 seconds, accepted the download,
and rendered the converted checker texture and static item model. Its downloaded
archive matched the advertised SHA1. This was a Linux software-rendered client
with a null audio device; it does not verify sound playback or Bedrock rendering.
The independent native building probe disables protection only in its copied
test world; production spawn protection is retained.

Rust Wasm plugin custom-sound calls now dispatch to both editions using the shared
mapping. The interop fixture registers join and packet observers, plays and stops
the sound inside the join callback, and verifies both wire formats. This caught a
host reentry deadlock: the old synchronous sound imports could not service their
own nested packet event. Sound imports now use the existing runtime admission and
reentry paths. The corrected fixture completes both joins and a normal shutdown.
World-broadcast testing also exposed a race in the runtime itself: it could start
blocking host work before registering a route for its immediate callback. A
deterministic regression fails on the previous ordering. Host work now starts
only after callback admission is registered. The shutdown fixture also explicitly
joins its spawned Rust task, rather than treating it as a Wasmtime component task.
Protocol tests also cover independently encoded 2225 sound payloads, and generated
v0.2 packet mappings now include optional numeric fields needed by those packets.

Custom dialog replies now retain their complete network NBT and decode the
VarInt payload length in both configuration and play phases. Fixtures encoded
with the official Java codec cover cancellation, a short payload and a payload
requiring a two-byte length. The real TCP/Rust Wasm test receives all three with
the expected bytes. The decoder bounds size, nesting and traversal without
building an NBT tree.

The additive shared form API projects simple, modal and custom forms to both
editions. The Rust fixture receives all four response paths, including dismissal,
and opens the next form from each response callback while packet observers remain
enabled. Invalid choices, wrong IDs and replays do not produce extra callbacks.
Six session regressions cover ownership, validation, expiry and ID exhaustion.
The official Java 26.3 dialog codec also decoded all four captured definitions.
See the resource guide for layout differences and limits.

An unmodified Java 26.3 GUI client displayed each form. Clicking the simple button
and the modal's second choice, changing the custom form's checkbox and submitting
all input values, then dismissing the final form with Escape produced the four
expected Rust callbacks. The custom response retained the range, both option
indices and Unicode text. The client returned to the world after dismissal.
This verifies the vanilla Java projection; Bedrock rendering is still unverified.
The component built before the new method was added also loaded and passed the
previous join, sound and custom-dialog reply checks against the new host.

Shared static models now have one tracked server entity, exposed through the
Rust plugin API. The interop fixture covers creation, scaling, movement, removal,
late watchers on both editions, spawn cancellation, invalid inputs and a normal
restart. Bedrock receives an actor registry containing its mapped native name;
Java receives an item display carrying the canonical name. The vanilla registry
comes from Cloudburst's network data; custom registration follows Geyser's actor
identifier format.

The same saved model survived an official Java 26.3 server load and save, then
was restored for both editions by Sculith. Removing its mapping hid the model
without deleting its saved data; restoring the mapping recovered its original
UUID and transformation. This also fixes the inherited item display serializer,
which omitted its item stack. A new unmodified Java 26.3 client rendered the
model and its scale, position and removal updates. Bedrock rendering remains
unverified.

Removal testing caught another host reentry deadlock: the synchronous entity
removal import held the store while dispatching its own packet events. Both
hosts now release that borrow during removal; v0.1 plugin calls retain their
synchronous contract. The experimental v0.2 method is async.

Arbitrary Bedrock UI conversion, animated models and Molang remain unfinished.

## Remaining work

Bedrock LevelDB world import/export is not implemented. Java Anvil support does
not provide it. Existing worlds must be copied before testing, and unknown
blocks, entities and data must not be silently discarded.

Pumpkin is a Java-oriented engine with Bedrock support still in development.
Using this base does not establish Bedrock gameplay parity, arbitrary Bedrock pack
compatibility, authoritative movement and inventory validation,
production authentication coverage, the old Sculith plugin API, or production
load capacity. Those remain
Sculith work. No Windows GDK client test is claimed for this fork.
