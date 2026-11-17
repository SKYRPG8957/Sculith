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

The corrected public packet records require native plugin ABI 10. Rebuild
native plugins against the same revision as the server. Stable Wasm v0.1 is
unchanged; the experimental v0.2 packet records are regenerated with the codecs.

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
