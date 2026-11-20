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
| Direct Bedrock source guard | Exit 1, source-file hashes unchanged |

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

Current-client packet callbacks now apply returned ID and payload changes instead
of retaining an unconditionally read-only result. The stable Rust Wasm fixture
verifies replacement, cancellation and an untouched sentinel on both editions.
Bedrock's initial network-settings request and current-client login also reach
the connection hooks. A native Rust fixture checks Java's pre-player stages,
status cancellation plus an extra packet, and a synthetic Bedrock proposal
rewrite before admission. Both checks fail on the earlier server. See the
[packet event guide](packets.md) for API versions and remaining protocol limits.

## Bedrock world import

Run `sculith --inspect-bedrock-world SOURCE` on a stopped world to check what
can migrate before choosing a destination. It uses the same terrain and item
decoders as the importer and reports readable columns, storage, excluded
records and up to 16 blocking diagnostics. An inspection returns exit code 1
when the source has blocking issues.

`sculith --import-bedrock-terrain SOURCE DESTINATION` reads a stopped Bedrock
world through a temporary LevelDB copy. The command accepts storage version 10,
chunk version 42 and subchunk version 9 in the Overworld. It writes a new Anvil
world after all imported columns have converted. Unknown block and biome
palettes, unsupported generators and other dimensions fail the import.

Dragonfly v0.11.6 can save flat terrain with its generator layers unspecified.
Append `--new-chunks=normal` to either command to select Sculith's normal
generator for new chunks in this case. Saved terrain, seed and spawn still
come from the source. The option preserves complete generator metadata when
the source supplies it.

Biome storage is sampled at the origin of each four-block Java biome cell.
The import carries the seed and spawn into Sculith. New chunks use Sculith's
generator, with supported Bedrock flat layers mapped to a flat preset.
Ordinary single chests and barrels preserve the slots and counts of plain
items with a shared item mapping. This includes supported block items in their
default state. The command skips the entire contents of paired chests,
containers with unsupported metadata and containers with unsupported stacks
such as potions. It reports excluded database records and shows the first 16
skipped block entities with their coordinates and reasons.

The official Bedrock 1.26.63.1 server fixtures covered a flat column and 27
ordinary terrain columns. Java and Bedrock protocol clients read the same
imported block, and Java received the original flat strata in a newly generated
column. An unmodified Java 26.3 client joined the imported flat world.
Source file hashes matched after import and after a rejected conversion.

## Bedrock storage export

`sculith --export-bedrock-storage BEDROCK_TEMPLATE SCULITH_WORLD DESTINATION`
copies a stopped Bedrock world and transfers supported inventories from its
stopped Sculith import. Use a new destination outside both inputs. The inputs
must have the same seed and matching ordinary single chests or barrels at the
template positions.

The output uses the template's terrain, players and metadata. The transfer
covers plain mapped item IDs, slots and counts in the template's supported
containers. The command keeps unsupported template records and reports why it
preserved them. A missing or incompatible Sculith container blocks the export
before the destination is published.

## Region header recovery

Anvil and Pump write new inline chunk payloads into free sectors before
publishing the region header. A temporary `.sculith-journal` beside the region
records the old and new headers during publication. After an interrupted
process write, Sculith checks the journal against the region and restores the
complete new header on opening it. The journal is removed after recovery. A
damaged or mismatched journal produces a region read error and remains
available for inspection.

Synchronous region flushes sync the payloads before publishing the journal and
sync the region header before removing it. The recovery unit is one region
header. Read-only Anvil tools can use the recovered header in memory.

## Player save batches

`[player_data].save_player_cron_interval` in `sculith.toml` sets the interval between
periodic player snapshots. A manual save and a normal shutdown also capture
online players as one batch. On disconnect, the same writer saves the departing
player's inventory and other player NBT.

The writer syncs and publishes `.sculith-player-save` inside `world/players/data`
before replacing the affected `.dat` files. The server replays a published batch on
startup before admitting players. A damaged journal or failed replay prevents the
server from opening the world and leaves the journal available for inspection.

## Manual world checkpoints

`/save-all`, `/save-all flush`, and the management save request capture online
players and loaded block-entity chunks at a server tick boundary. When both
groups have records, the server syncs one `.sculith-world-checkpoint` in the
world root before saving either group. Startup replays the checkpoint before
opening region caches or admitting players. A retry can finish an interrupted
checkpoint while the server is running. Offline conversion and storage export
require the checkpoint to be fully applied.

## Automatic inventory checkpoints

When terrain autosave and player saving are enabled, Java quick moves and
number-key swaps and Bedrock direct slot transfers between a player and one
chest, trapped chest, or barrel enter a shared checkpoint. The server captures
the touched players and terrain chunks at the end of the tick. One group can
hold 256 distinct player and chunk targets. Clients get the resulting slots
in memory, and the checkpoint journal records when the group becomes durable.

The writer attempts a failed group again after one second and keeps the
captured data available for replay. Supported transfers that arrive while the
writer is busy receive the server's current slot contents. A process
interruption before publication leaves the earlier stored state, and startup
replays a published group before admitting players.

## Remaining work

Standalone Bedrock world export and conversion of entities, player data and
other block-entity contents remain open.

Automatic checkpoints for cursor gestures, double chests, and automated container
transfers remain open.

Pumpkin is a Java-oriented engine with Bedrock support still in development.
Using this base does not establish Bedrock gameplay parity, arbitrary Bedrock pack
compatibility, authoritative movement and inventory validation,
production authentication coverage, the old Sculith plugin API, or production
load capacity. Those remain
Sculith work. No Windows GDK client test is claimed for this fork.
