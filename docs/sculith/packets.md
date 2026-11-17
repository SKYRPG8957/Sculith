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
