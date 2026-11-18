# Packet events

`PacketReceivedEvent` and `PacketSentEvent` expose play packets on both editions
through their ID and raw payload. Blocking handlers can replace those fields or
cancel delivery. Outgoing edits now apply to current clients as well as clients
admitted by a protocol translator. An unchanged result retains the original
encoded buffer. Injected packets are not echoed back to current-client observers.

Native handlers and the experimental Wasm v0.2 host also support extra outgoing
packets. These are queued after the original, or on their own when it is cancelled.
Stable Wasm v0.1 retains its existing record and supports ID, payload and
cancellation changes without a component ABI change.

## Bedrock packet IDs

Bedrock packet definitions use generated constants from
`assets/bedrock/packets.json`, pinned to gophertunnel's protocol 2225 catalogue.
The name lookup includes retained legacy IDs. Names describe known wire IDs;
they do not indicate gameplay or typed decoder coverage. Known packets without
a gameplay handler still reach raw packet observers and are logged at debug level.

The catalogue corrected seven stale IDs: both stonecutter directions,
matchmaking state and cancellation, signed audio registration and playback,
and block passengers. Signed audio now includes the fourth registration string
and trailing playback options; matchmaking options use the two optional fields
in protocol 2225. Neither matchmaking nor signed audio services are implemented
by these codecs.

The public packet records changed in ABI 10. Rebuild native plugins against
the same revision as the server. The experimental v0.2 packet records are
regenerated with the codecs.

`ActorPickRequest` (35) uses a fixed signed little-endian 64-bit entity ID.
Creative players can now pick a nearby tracked mob to create or select its
mapped spawn egg. It shares the block-pick inventory selection and updates.
Unknown IDs, IDs outside the engine's entity ID range, and distant targets are
ignored. Entity data copying and targets without a spawn egg remain unsupported.
The experimental v0.2 typed packet view includes this request; native Rust and
stable v0.1 observers retain their raw packet interfaces and existing ABI.

## Pick selection events

Block and mob picks now use one inventory operation for both editions, following
the native first-empty-hotbar policy. If a pick would change the selected slot,
`PlayerItemHeldEvent` fires before changing items or selection. Cancellation
leaves the inventory unchanged and resynchronizes the client.

Callbacks run without an inventory guard. The operation checks its plan and
selected slot again before committing, so callback changes are retained rather
than overwritten. The native plugin ABI remains 10 and the Wasm interfaces are
unchanged. The native fixture checks cancellation, callback reads and inventory
mutation with independent Java and Bedrock clients.

## Creative inventory requests

The server checks the player's game mode and every creative catalogue ID before
applying any action in that request. Only Creative mode can create items this
way. IDs start at 1; 0 and IDs beyond the advertised catalogue are rejected.
The creative repetition byte does not multiply the result: a creation supplies
one stack at the item's normal limit. Other crafting requests retain their own
repetition handling.

Both editions use `InventoryCreativeEvent` before writing the inventory.
Native creation requests first calculate player-slot, cursor and drop changes.
Each changed player slot uses its Java player-screen index in the event;
native cursor changes and outside-inventory drops use `-1`. If any event is
cancelled, the request applies none of its planned changes or drops.

Callbacks run without screen or inventory guards. Before applying changes, the
server checks the affected stacks, screen, world and game mode again. Plugin
changes are retained if a plan becomes stale. Inventory and cursor corrections
follow rejected native requests; Java also sends the actual slot after a veto
or callback mutation. Equipment notifications and world drops run after unlocking.
Native creation supports the player's inventory, equipment and cursor. Recipe
and container-specific operations continue through their specialized handlers;
mixing those operations into a creative creation request is rejected.

## Player inventory requests

Native take, place, swap, drop and destroy requests against player-owned slots
and the cursor use the same snapshot and commit checks as creative creation.
Counts, matching components, stack limits, slot permissions and game mode are
validated before any changes. A later invalid action also rejects preceding
swaps and world drops. A legal transfer from a slot to itself changes nothing.
Spectators cannot move stacks; only Creative mode can destroy them. Standalone
consume requests need a crafting context and are rejected on this path.

Ordinary requests fire the existing `InventoryInteractEvent`. Planned drops,
including creative-output drops, fire `PlayerDropItemEvent` before the commit.
Callbacks can read the screen and inventory without retaining their locks;
cancellation leaves the plan unapplied and callback changes are retained.
Actual inventory, cursor and equipment follow both successful and rejected
requests. Recipes and specialized screens retain their separate handlers.

Planned ordinary UI requests first fire `InventoryClickEvent`, then the general
interaction event. A click veto skips the interaction event and leaves the plan
unapplied. The click carries the original cursor and the first physical slot
touched by the request, using current screen indices. Cursor-only operations use
the outside slot `-999` and no clicked item. Bedrock requests do not identify a
mouse button, so their click type is `Unknown` and hotbar button is `-1`.
Legacy hotbar drops have no UI click event; creative creation retains its
`InventoryCreativeEvent` path. Specialized container clicks and drag gestures
remain separate work.

Creative destruction also fires `InventoryCreativeEvent` after the click and
interaction callbacks, once per affected target with its final planned stack.
Slot numbers use the player's canonical inventory layout; the cursor uses `-1`.
An empty result reports air with count zero. A veto leaves preceding moves and
destruction unapplied; callback changes survive stale-plan rejection.

## Storage requests

Ordinary stack requests in a generic storage screen use the same plan. The
server binds the packet to its open screen and world, validates storage indices
and the rest of the request, then runs the click, interaction and drop
callbacks before applying changes. A veto or an invalid later action leaves
the request unapplied. The response corrects the active storage window along
with the player's inventory and cursor, including changes made by callbacks.

When the chunk unloads, the scheduler saves the current block entity data with
that chunk. On reload, the next viewer receives the saved contents.

This covers individual requests. Simultaneous transactions by separate
viewers of the same container still need shared inventory ownership.

## Item use events

Both editions dispatch `PlayerInteractEvent` before preparing timed item use or
swapping equipment. Cancellation skips those effects and the item-use callback.
The handler also rechecks the used stack, selected slot, game mode and world
after callbacks; a changed snapshot rejects the old action and corrects the
owner from current inventory and health. It does not clear use state written by
a callback. Java retains its consume and fishing callbacks before interaction.

These checks cover native click-air and Java use-item handlers. Native block and
entity interaction handlers retain their separate paths.

## Inventory close

Closing returns the cursor stack through the shared screen handler. The native
close reply then uses the same owner inventory synchronization as stack requests,
including the actual cursor in UI window `124`. It captures inventory, cursor
and equipment after close callbacks, so callback-written stacks are not replaced
with an assumed empty cursor.

## Legacy inventory transactions

Legacy slot groups request synchronization. The server returns the actual slots
without moving or dropping their contents. Queries allow two groups with two
slots each. Invalid containers or indices reject the transaction and resynchronize
the owner.

Normal transactions accept a verified main-inventory/world drop pair. The old
stack, remaining stack and dropped type/count must agree with the server's
snapshot. The existing inventory plan runs interaction and drop callbacks before
applying changes; only a copy of the server stack enters the world.

Item-use, entity-use and release journals describe client predictions and are
never applied as inventory writes. The declared hotbar and held type, aux value
and count are checked against the server; client NBT and runtime state do not
replace server data. Selection changes honor `PlayerItemHeldEvent` and recheck
the stack, world and game mode after callbacks. Nearby rejected block predictions
are corrected from the world, while air use does not read the supplied block
position.

Offhand air use and release are supported. Offhand block/entity transactions and
the legacy item-use break subtype are rejected; native breaking continues through
`PlayerAction` and input-carried block actions. Modern stack network-ID prediction
journals and specialized container transactions remain separate work.

## Entity tracking callbacks

Tracking releases its entity-map and viewer-list locks before sending packets
or calling player filters. A packet observer may remove an entity during spawn
or motion delivery without waiting on the tracker that invoked it. The pending
spawn or motion can still be cancelled through `PacketSentEvent`. Removed
entities are checked before pairing and non-player updates.
The native ABI and Wasm interfaces are unchanged.

## Before player creation

Native Rust plugins can subscribe to `JavaConnectionPacketReceivedEvent`,
`JavaConnectionPacketSentEvent`, `BedrockConnectionPacketReceivedEvent` and
`BedrockConnectionPacketSentEvent`. The experimental v0.2 component API exposes
these events too; its host remains disabled by default. Stable v0.1 components
do not have connection-event variants.

Java events identify the connection, version and handshake/status/login/config
state. Bedrock events identify the connection and negotiated protocol, which is
zero before the first network-settings request is admitted. Incoming payloads
are in the client's format; outgoing payloads are in the server's format.
Once a Bedrock player exists, its packets use the player packet events.

The initial Bedrock network-settings request now reaches the connection event
before version admission, and current clients' login traffic uses the same hooks.
Default version admission still applies to packets a handler leaves unchanged.

## Verification

The stable Rust Wasm fixture changes an outgoing packet's ID and body, changes
another body, cancels a marked packet and preserves a final sentinel. Independent
Java and Bedrock decoders check the result and order. The native fixture observes
Java handshake/status/login/config, replaces a status reply with a cancellation
and extra packet, and rewrites a synthetic Bedrock version proposal before
admission. Both regressions fail against the previous server.

See the [Wasm fixture](../../tests/interop/plugin/README.md) and
[native fixture](../../tests/interop/connections/README.md) for reproduction.
These checks use copied worlds and loopback authentication settings. They cover
packet delivery, not Windows rendering or online authentication. Typed packet
views and Bedrock gameplay decoders still cover a subset of the protocol; raw
events retain the unconverted payloads.
