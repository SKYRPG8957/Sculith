# Player visibility preview

One world owner commits initialized player membership and movement for both
adapters. A viewer gets an atomic membership snapshot and its bounded continuation;
there is no second edition-specific simulation. Pending logins are absent.
Private plugin messages, forms, sounds and models never enter the peer stream.

The stream holds at most 128 shared events. Each viewer tracks at most 63 other
players, with an online roster and entities only inside its loaded chunks. Leaving
loaded chunks removes the entity while preserving roster membership. Returning
spawns its latest owner position. Disconnect removes the entity before its roster
entry. A missed event backlog closes the session instead of continuing with stale
membership. Global player/presentation IDs remain distinct.

Java 26.3 uses player-info entries, the stock default avatar, player entities,
absolute position/rotation updates and head rotation. Names outside Java's ASCII
profile-name rules get a bounded `Scu_<id>` profile name and retain the original
display name in the roster. No signed account textures or chat sessions are forged.

Bedrock protocol 2225 gets native player-list/skin, player-entity and movement
packets. The placeholder is an original solid green RGBA image and simple cuboid
geometry. Skin trust describes that server-authored placeholder, not account
verification. The empty held item has an empty user-data buffer: a padded air
payload was rejected by the independent decoder when its minimal item registry
had no shield entry. Peer abilities reuse the same owner-derived base layer as
local abilities. Native wire player positions include the 1.62-block offset;
Java positions use feet coordinates.

Account skins, capes/persona, armour/offhand, arm swings, damage/combat, player collision,
general entity AI and persistent player state are outside this preview. Public
online hosting and official Windows Bedrock rendering remain unverified.

## Verification

[Actual unmodified Java screenshots, exact-source native Windows checks and hashes](../development/verification/2026-11-16-shared-peers.md).

```sh
. ./scripts/dev-env.sh
cargo test --workspace --all-targets --locked
cargo build --workspace --locked
# Build the pinned Go probe as documented in tests/interop/gophertunnel/README.md.
python3 tests/interop/peer_smoke.py --client target/gophertunnel-probe
```

The independent Python/Go check verifies initialized roster/skin/spawn ordering,
both movement directions, current positions for a late join, and ordered entity
and roster removal. Rust owner/projection tests cover the atomic observation
boundary, dead leases, pending/self/duplicate players, private-event separation
and leaving/re-entering loaded chunks. Other smoke checks continue validating
private model IDs instead of silently discarding every entity packet.

The optional Go `--peer-visual` companion joins an already running local server,
moves into view, reports actual Java movement and remains for 30 seconds. It skips
the flat-voxel assertion because an existing test world may contain edits. It is
a headless native protocol client; only the separate unmodified Java game renders.

Wire references: official Java 26.3 registry/report and client manifest (hashes in
the actual-game verification records); Apache-2.0 bedrock-rs
`5221fcae`, MIT gophertunnel `e2251a60f71d` and MIT Dragonfly
`0f9a3a573626` pinned in the lockfiles. No official client code or assets are
distributed. Existing MIT licensing is preserved.

## Authoritative held items — Preview 16

Peer spawn now includes the server's selected starter item. Changed hotbar slots
publish a dedicated owner event to the peer stream only. A repeated selection
produces no event; rejected or pending selections produce none. Hidden peers keep
the latest selection for their next visible spawn and late joins see the current
owner snapshot. Region revisions and plugin game hooks do not change.

Java uses packet 104, main-hand equipment ordinal zero and its normal optional
item stack codec. Native player spawn and MobEquipment both use the same selected
stack as the local inventory correction. Stone/Dirt/Grass counts remain 64 in the
existing creative preview. Client item/count claims cannot replace the canonical
slot contents. Selecting an empty starter slot explicitly clears the hand.

`python3 tests/interop/equipment_smoke.py --client target/gophertunnel-probe` verifies
initial hands, both directions, a forged item/count claim, late joins, empty hands
and cleanup with independent Python/Go codecs. Owner/projection tests cover event
isolation, no-op/invalid/pending selections and hidden-peer re-entry. This is a
bounded projection of the fixed hotbar, not general inventory or armour support.
