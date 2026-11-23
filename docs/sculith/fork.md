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

Bedrock mouse, classic touch and analog controls decode into the engine's
`PlayerInput` flags. Java inputs use the same engine entry point. Sneaking
and minecart controls read those flags. The native plugin input field and Wasm
input event use the established input mask encoding.

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

## World scheduling

Each loaded world has a monotonic tick deadline and at most one outstanding
simulation job. The existing simulation pool executes world jobs and their
parallel work. A busy world skips expired deadlines while other ready worlds
can process packets, run simulation and synchronize their own clocks.

Conditional plugin block edits compare the expected block in its loaded chunk.
Rust and Wasm plugins can also queue a conditional player inventory edit with
`schedule_inventory_edit`. Before writing the slot, the owner checks the active
connection, intervening teleports or respawns, and the item type, count and
components. A plugin can poll the receipt from a callback or a later tick.
A current-protocol Bedrock connection's region can apply a queued storage-slot
edit when the player's inventory is closed and the ordered input queue is idle.
The region queues the slot packet with the write. Both owners use one ordered
list of requests for the connection and complete each receipt once. Hotbar,
equipment, open screens, observed packets and coordinated operation use the
world path. Per-tick inventory component updates also check the original stack
before replacing it.
On its personal inventory screen, a current Bedrock client can also finish a
request with up to eight `Take`, `Place` and `Swap` actions on its region. Each
slot must be in ordinary storage or the cursor. The region checks each stack ID
against its staged state as it processes the actions in order. A prediction can
reference a slot already touched within that request. The region prepares the
reply and inventory refresh for the final state, checks that it still owns the
player, and writes the changes together under the locks a player save uses.
The ordered input queue keeps later packets behind the request. Predictions
from other requests, other screens or slot types, drag state, and observed
inventory or packet events use the world dispatcher.
Transfers reserve the source and destination before changing a player's membership.
Java and Bedrock logins prepare the player before queuing registration on the target
world. Registration checks the live connection and membership while holding that owner.
It resolves a new player's starting position from the world's loaded spawn terrain.
If the chunk is missing, the connection loads it and retries registration. Both
protocols then use the player's authoritative position for their first spawn.
While that spawn is being sent, the session can process network input. Player
ticks and contact with other actors begin after the spawn setup completes.
The world then validates the registered connection before publishing its roster
entry and pairing its actor. A dimension transfer queued during connection
setup waits for this publication and, on Bedrock, the spawn status packet.
Pending transfers are discarded if the connection closes.
Closed connections detach and clean up their inventory on the owning world. Leave
callbacks run between those steps. Saving uses the resulting player snapshot, and
the account can load again when the disconnect save finishes.
Automatic inventory checkpoints capture the worlds that contributed to the
batch. Shutdown drains world jobs before kicking players and saving.

Console commands, server-wide tick hooks, datapack functions and game tests
use a coordinated boundary. Frozen worlds still process network packets.
Stepping and sprinting advance every participating world by the requested
number of ticks. Explicit checkpoints coordinate the worlds for capture.
Periodic player saves capture ready worlds and retry busy owners at later
boundaries. The captures share the existing ordered asynchronous writer.

During independent ticks, players can run built-in `/seed` and `/time` on
the owner of their current world. Time changes update that world's clock and
publish it to its Bedrock and Java clients. Admission checks the registered
command tree and uses the same permission checks and parser as other commands.
Plugin-extended routes, command or permission hooks, packet send hooks and
commands queued behind coordinated work use the coordinated boundary.

Each dimension saves its elapsed ticks, time of day, fractional tick, clock
rate and pause state in `.sculith-world-clock.json` beside its terrain. The
level writer publishes the snapshot during autosaves and explicit saves.
Shutdown writes the world's final clock after its pending writes have drained.
Existing worlds without a snapshot start from the matching Anvil clock when
present. Invalid or unsupported snapshots prevent opening the world.

The active-chunk tracker groups connected chunks into regions for scheduled
block and fluid work. Overlapping regions reuse owner IDs, with the largest
overlaps taking priority.

On ordinary ticks, each active region advances scheduled block and fluid
timers and chunk inhabited time on its own deadline. Loaded chunks with
timers outside active regions share a deadline for timer work. Inhabited
time contributes to local difficulty. Each chunk carries its remaining
delays when it changes owners. After a merge or split, new jobs inherit the
latest relevant deadline and wait only for jobs whose chunk extents overlap.
Jobs with unchanged chunk membership keep their existing write authority.
A late owner drops missed starts and cannot accumulate a queue of catch-up
ticks.

A timer that expires stays with its chunk until the world takes it for the
block or fluid callback phase. While it waits, the same timer cannot be
scheduled again and a save stores it with zero remaining delay. This uses
the existing chunk formats. The world sorts each callback type by priority.

An active region can commit a chunk's ready batch of built-in redstone
lamp turn-offs and target block pulse resets on its own deadline. Each
decision uses locked block palettes in tick priority order. Lamp checks
inspect adjacent blocks and possible strong power through solid neighbours.
Target resets require that adjacent block and fluid callbacks have no work
and that no plugin observes block physics. Reads can reach adjacent chunks
in the region's dispatched extent. Their loaded entries and block palettes
stay pinned through the commit, which rechecks the published extent. A batch
requiring an unavailable chunk, a callback on the world owner or more than
32 ready ticks stays queued for the world phase. Both save formats capture
the committed properties and timer removals together. The shared lighting
engine follows changes to light properties, and both editions read the
authoritative state.

The region also executes built-in water ticks for isolated vertical flow.
Canonical source and falling water can fill air directly below them. The
falling block and both follow-up timers commit together, and consecutive
ready ticks see earlier decisions in that batch. The same ownership guard
applies to the neighboring reads. Horizontal flow, other fluid interactions
and observable flow or physics events use the world callbacks.

On a regional deadline the owner can also apply queued Rust plugin block
edits. The owner checks the expected state and its current extent under the
palette lock. Edits can notify clients when their geometry, light and other
chunk bookkeeping stay compatible and the adjacent shape checks have no
callbacks to run. Requests stay ordered within a region. A deferred edit or
an edit claimed by the world holds that region's subsequent requests while
other regions can process theirs. The world applies remaining requests and
runs edits during coordinated ticks and freezes. Wasm receipts use the same
API and report success, staleness or unavailable worlds as before.

Region jobs can also publish their committed block changes to Java and Bedrock
watchers before the world finishes its tick. Delivery checks the current owner,
the loaded block state and each connection's chunk sender. Packets go through
the existing FIFO, including Bedrock's separate water layer. Each ready
connection can receive an update while another viewer waits for its terrain.
The world reconciles deferred changes with the authoritative state. A viewer
that already got that state in the same connection epoch skips the duplicate.
Packet observers and translated clients use the world publication path. The
Java chunk sender checks terrain revisions before reusing or queuing an encoding.

Loaded campfires in active regions advance their cooking or cooling counters
on the region clock while the actor and block state are pinned. The region
validates a ready slot again before exchanging its ingredient for one live item
entity. The drop starts its lifetime and pickup delay on the next deadline of
its region when plugins do not observe item despawns or merges. The world pairs
the new item with clients and publishes the changed
block actor at its next publication step. Coordinated ticks and cooking or
entity-spawn listeners keep their existing world-owned execution. A save or
entity chunk retirement can defer a regional completion until its next deadline.

Dropped items in active loaded chunks take their lifetime and pickup delay
steps from the region owner. The world publishes the assignment from the
entity's current chunk. A movement between owners updates position and
assignment together so the destination region can take over before the next
world tick. A merge or split reassigns the already scheduled items when the
new chunk ownership is published. A regional job checks each item's current
assignment and residency before touching its counters. The item keeps its most
recent deadline across the handoff so two close deadlines cannot advance it twice.
An isolated airborne item can also move on the region clock while its swept
bounds touch only air in loaded chunks of the same owner. The owner pins the
terrain and spatial indices through the update, including when the item crosses
a chunk boundary. The separate motion deadline prevents a repeated step.
An expired unmounted drop can be removed from the live registry by its region.
The commit excludes concurrent actor interactions and shares the entity-save
guard. A region can publish the removal to current-protocol Bedrock and Java
viewers once their spawn has been queued. Java flushes once per viewer for
the region's expiry batch. The tracker retains connections
that still need the world's translated or plugin-observed publication.
Movement outside the region, ground and fluid contact, pickup and merging
are handled by the world. Native library loading disables regional item motion
and expiry because native plugins can write live actor fields. Merge and despawn event
handlers use the world clock, as do coordinated steps and sprints. Saved items
retain their remaining timers and last position.

Experience orbs use the same region assignments and motion deadlines. An orb
with no follow target can advance a short path through loaded air or land on
inert full blocks if its region owns the sweep and the player index has no
collector in range. The step applies gravity, collision, bounce, drag and age
together. Block callbacks, partial collision shapes, fluids, nearby players
and coordinated ticks use world processing. An expired orb can
leave the registry and notify both editions on its region, using the shared
drop removal path. Orb records keep their age and experience value across
chunk retirement and restart. A restored orb starts on the next regional
deadline.

An eligible item or orb that moves inside its paired chunk can also publish
its pose on the region. Current-protocol viewers must already have the actor
and the chunk. A partial send requires a full pose before deltas resume.
The world handles pairing at chunk boundaries, translated clients and
packet observers through its usual tracker.

Freeze pauses timer advancement and lets the first resumed world tick
refresh membership after player movement. Coordinated steps and sprints
use the region tick numbers at the world boundary and advance each chunk
once per requested world tick. Merging inherits the highest coordinated
tick number and splitting passes that number to each successor.

An active region can consume the front of a native Bedrock player's input queue
for latency, chunk cache and teleport acknowledgments. A teleport is confirmed
against the wire position and the pending ID recorded after queuing the packet.
The region consumes stale coordinates while that teleport is pending. A resend
and packets with receive or send hooks use the world input path. A ready
connection is routed by its current chunk and checked again when the regional
job claims a packet. A merge
or split forwards pending connections to their current owners. The world and
region share the same ordered drain for that connection. Stationary grounded
inputs can finish on the region once movement, landing and footstep effects
are settled. A change of look also requires queuing the updates for every
paired viewer.
The region records the client's current prediction and can then process the
next packet within the same budget. A finishing world step serves any viewers
whose packets the region could not queue.
At the head of the queue,
the region can also decode one native Bedrock movement packet within its current
extent. The world rechecks that extent and the packet after receive hooks before
using the decoded input. During the ordered world move, it can commit the position
with the swept chunk extent, destination biome and player spatial index pinned.
The check uses the final target after the movement hook. A path outside the
current extent or a busy chunk or index uses the ordinary position setter.
Crossings between owners, updated payloads and translated clients go through
the ordinary world decoder. Movement effects that need shared work, other
scheduled callbacks, entity AI and entity chunk residency use world phases.

The region can prepare a native Bedrock direct melee request when the player
and target are in its current extent. The request stays at the head of the
connection queue. The world rechecks the extent and original packet before
using the decoded request through the existing inventory and attack handlers.
Target identity, reach, held item, damage, and plugin events are resolved on
the world owner. Cross-region targets, translated packets, and receive or send
packet observers use the usual world decoder.

A native Bedrock input can also commit a short level move and its look on
the region before the world processes the connection. The pose, controls and
grounded state must stay stable. For positional movement, the extent must own
each swept chunk. The region pins the terrain and spatial indices and checks
for an air path, a full dry floor for a walk, and clearance from nearby
actors. A turn in place updates the rotation without a terrain collision
check. Input with outstanding effects or delivery stays at the head of the
connection's queue for the world to finish. A move within the paired chunk
or an eligible level step into the next owned chunk can reach current-protocol
viewers from the region once their actor spawn and destination terrain have
been queued.
Java receives an absolute position, including when the world finishes delivery
to remaining viewers. An intervening teleport supersedes the pending move.
The world resynchronizes existing viewers that already got the regional move.
If a teleport changes the chunk, those viewers must also have its terrain.

An ordinary level step, walking or sprinting, can also finish on its region.
When it crosses a chunk boundary within that region, the player's published
view and delivered terrain must already cover the destination. The region
shifts the simulation ticket to the destination chunk and keeps that view.
All paired viewers must have their packets queued and the movement must
require only local accounting. The region records the movement statistic,
delta, and activity, and marks the player's changed position for normal actor
pairing. Sprint exhaustion uses the same calculation as world movement and
shares the player's guard with the world food tick. A step that triggers
sound or vibration, a location enchantment, or an exhaustion hook stays on the
ordered world path for those effects.

When a native Bedrock player moves between active regions, the destination
receives that connection's ready work at the position change. Previously
claimed work and the handoff use the same per-connection guard. The player
carries the ordered packets and timer deadlines across the move, so the
destination can process them while the previous region is waiting.

Once a native Bedrock player has spawned, its region can dispatch ready
chunks from the existing sender. A view becomes eligible after the publisher
packet is queued and the sender has installed the new view. Regional and
world dispatch share the sender's quota, deadline, and delivery tokens.
Encoding and network credit stay in the existing workers. A view change,
initial spawn, coordinated step, or chunk/packet hook uses world dispatch.

After a native Bedrock player gathers an experience orb, the current region
can advance that player's pickup delay. Pending work follows the player across
region changes. Deadlines prevent an overlapping world or region step from
advancing the same delay again. Orb contact, mending, and awarding experience
run in the world phase. Coordinated steps and players without an eligible
region use world processing.

A current-protocol Bedrock player's region can spend accumulated exhaustion
to reduce saturation or food. The connection retains the same deadline stamp
across movement, and both world and regional drains use the same food rule.
Food writes and player save snapshots share the abilities lock. The world
advances healing and starvation, and publishes the latest food attributes
after a regional change. Peaceful regeneration, healing or starvation in
progress, packet observers and coordinated ticks use the world processing path.

Finite status effects on current-protocol Bedrock players can count down on
the player's region while they have no periodic effects or removals to process.
The active effects lock protects the remaining durations and the shared
deadline stamp. Pending work and the stamp follow the connection across
region changes. The world handles periodic effects, expiration and their
attribute and packet updates. Coordinated ticks advance the same duration
state, which also supplies player saves.

The same passive countdown can run on the region that owns a living mob.
Its assignment changes with its chunk, and its remaining durations and deadline
travel with the actor. The old owner checks the current assignment and loaded
chunk before taking a step. The owner may remove a hidden passive effect after
its last tick and queue removal for its Java and Bedrock viewers. The active
effects lock and an edit permit order the removal with effect reapplication.
The world processes periodic effects, removals with attribute or metadata
changes, and removals that need packet observers or translation. Both paths
share the same deadline to prevent a second step after a handoff.

Each current-protocol Bedrock connection with an active item cooldown advances
it on the player's region. The same clock and cooldown entries follow the
connection when it changes owners. A repeated deadline cannot count twice.
During coordinated steps or without a region, the world advances the clock.
Item use and its plugin callbacks still run through the ordered input path.
Starting a cooldown sends the client's native category and duration packet.
Both Wasm cooldown setters update the server cooldown immediately. The 0.1
host can run packet observers during the send. The 0.2 host sends from a task
after its synchronous setter returns, and its packet observers can read the
typed category and duration.

The Wasm player setters for health, maximum health, absorption, saturation
and exhaustion also support packet observers registered by the calling plugin.
The 0.1 host services those observers before returning from the import.
The synchronous 0.2 imports record the change immediately and publish current
attributes from a store task through the normal packet callbacks.

Native Bedrock survival mining accrues elapsed ticks on the player's region
when that extent owns both the player and block. World steps and regional
steps share a deadline stamp. The world checks the live block, mining
speed and permissions before it publishes progress or breaks a block.
Replacement targets, a cancelled action, and a change of world invalidate
the old mining session.

Player movement registers the new view before entity chunk readers run.
Readers share one disk request for a chunk and route its ready position to the
current active extent. Each region claims up to four positions per deadline.
Duplicate notifications coalesce and a retry goes behind other ready positions.
A merge, split or newly active extent receives its outstanding positions from
the pending table. A world pass that claims that table clears the regional
queues before new arrivals can enter them. An active region can register ready
armor stands, dropped items, experience orbs and markers. It validates the
current owner, pinned watchers, cached chunk and entity UUID registrations
before consuming saved records.
Regions also restore persistent, unlinked cows, pigs, sheep and chickens
when their terrain chunk is loaded and all saved actors in the entity chunk
can be restored on the owner. The region rechecks the terrain entry and
watchers before registering the actors and consuming their records. The region
can also pair freshly restored livestock without equipment or effects. It
prepares the native Java and Bedrock spawn data before making the actor live,
then checks the pose and terrain delivery before putting those packets in the
viewer's queue. The world reuses that tracker and pairs remaining viewers.
Regions can prepare unlinked livestock
for world registration in other chunks. The world rechecks the saved type,
UUID and position, then applies the latest NBT. Concurrent regions can share
the entity snapshot permit while doing this work.
An eligible drop joins the current region's timer plan with its saved age
and pickup delay. Its first timer step uses the next regional deadline.
The timer assignment follows a subsequent move to another region. Item merge
and despawn observers keep timer execution on the world owner. Regions can
pair a restored item or experience orb with current-version Java and Bedrock
players once their client has the terrain chunk. The world reuses the tracker
and pairs any remaining viewers. Legacy clients, attachment restoration and
packet observers use world pairing.
Actors with passengers or team memberships use world processing. The world
retires unwatched inactive chunks using
the live positions of entities and their vehicles. The retirement step prepares
records before pinning the watcher slot, then checks membership and the cached
chunk again. A watcher that arrives during capture keeps the live actors.
Disk reads and retirement writes run asynchronously. Manual saves serialize
their entity snapshot against these residency changes.
Native plugins use ABI 36 for the living entity layout change.

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
players and loaded block-entity chunks at a server tick boundary. The scheduler
continues polling world deadlines while the checkpoint writer is busy. When both
groups have records, the server syncs one `.sculith-world-checkpoint` in the
world root before saving either group. Startup replays the checkpoint before
opening region caches or admitting players. A retry can finish an interrupted
checkpoint while the server is running. Offline conversion and storage export
require the checkpoint to be fully applied.

## Automatic inventory checkpoints

When terrain autosave and player saving are enabled, Java quick moves and
number-key swaps and Bedrock direct slot transfers between a player and a chest,
trapped chest, barrel, or hopper enter a shared checkpoint. A double chest includes
both of its live halves in the same checkpoint, including Bedrock direct transfers
between the halves. A transfer across storage boundaries requires the open
double chest's backing inventories to match the live pair. A replaced or
disconnected half closes the stale screen when the player attempts the transfer.

Java cursor pickup, placement, double-click collection, and drag completion
enter the same checkpoint when they change a storage container. Bedrock stack
requests can include the cursor in the player and container snapshot. Manual
saves capture the carried stack as well. Rejoining first returns the saved stack
to available inventory slots. Any remainder stays on the player's cursor and
follows them between inventory screens. A disconnected player keeps an overflow
stack in the saved cursor.

Hopper moves involving chests, trapped chests, barrels, and other hoppers use the
same journal. This includes pushes to other containers and pulls from them.
The transfer adds the hopper and the other container's terrain chunks, including
both live halves of a double chest. A group containing only block inventories
can be saved without a player session. Hopper transfers retry on later ticks
when the writer is busy.

The server captures touched players and terrain chunks at the end of the tick.
One group can hold 256 distinct player and chunk targets, counting halves in the
same chunk once. Clients get the resulting slots in memory, and the checkpoint
journal records when the group becomes durable.

The writer attempts a failed group again after one second and keeps the
captured data available for replay. Supported transfers that arrive while the
writer is busy receive the server's current slot contents. A process
interruption before publication leaves the earlier stored state, and startup
replays a published group before admitting players.

## Remaining work

Standalone Bedrock world export and conversion of entities, player data and
other block-entity contents remain open.

Automatic checkpoints for item entities, minecart inventories, and automation
involving other block types remain open.

Pumpkin is a Java-oriented engine with Bedrock support still in development.
Using this base does not establish Bedrock gameplay parity, arbitrary Bedrock pack
compatibility, authoritative movement and inventory validation,
production authentication coverage, the old Sculith plugin API, or production
load capacity. Those remain
Sculith work. No Windows GDK client test is claimed for this fork.
