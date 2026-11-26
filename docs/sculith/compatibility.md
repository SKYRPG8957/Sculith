# Compatibility

The tested targets are unmodified Java 26.3 and Bedrock 1.26.63 (protocol
2225). Connected players share the server's world, entities and game rules.

| Area | Java | Bedrock | Current behavior |
| --- | --- | --- | --- |
| Joining, moving and building | Supported | Supported | Both editions have joined the same world and received each other's position and block edits. |
| World clocks and rules | Shared | Shared | Named worlds keep independent deadlines, time, weather and supported rules. Transfers and reconnect restore their respective world and saved position. |
| Ticking areas | Shared | Shared | Operators manage the same persistent areas with `/tickingarea`. The area keeps its dimension's columns loaded and simulating. |
| Region execution | Partial | Partial | Active regions advance eligible block, fluid, entity and player work on their own deadlines. Complex mob AI and several player interactions still run in world phases. |
| Gameplay rules | Partial | Partial | Covered Bedrock rules include natural healing, ordinary melee damage and selected mob attributes. Additional mob behavior and item interactions remain open. |
| Shared resource packs | Projected | Native | Mapped PNGs, fonts, sound events and static models have Java entries. Bedrock receives the authored resource pack. |
| Shared forms | Projected | Native | Java dialogs and Bedrock forms return the same supported choices and input values. Java pack fonts and sprites represent supported custom artwork. |
| Raw Bedrock UI JSON | Unavailable | Native | Existing Bedrock UI files travel in the resource pack. Java dialogs support the shared form API. |
| Distant terrain | Unavailable | Limited | The optional pack shows 8-, 16- and 32-block terrain summaries beyond the sent chunk view on supported Bedrock connections. |
| Bedrock world import | Shared | Shared | A world folder or `.mcworld` export can create a separate Anvil world with ticking areas and supported terrain and block, entity and player data. |
| Bedrock world export | Limited | Limited | A Bedrock world template can receive selected inventory edits, dry block edits, supported world state and ticking areas. Standalone export remains open. |

An unmodified Java 26.3 client on Linux displayed the shared dialogs and
converted assets. Independent Bedrock protocol clients checked packet delivery,
pack bytes, forms and gameplay updates. Official Bedrock Dedicated Server
fixtures supply reference behavior in specific cases. Windows GDK rendering
and public-account login remain unverified. [Verification and storage limits](fork.md).

Native Rust plugins should be built against the deployed server revision.
The stable Wasm v0.1 host gives typed fields for the packets described by its
API. The optional v0.2 host also decodes all 37 serverbound and 78 clientbound
Bedrock packet IDs with implemented codecs. [Packet events](packets.md).

The supported shapes and limits for fonts, sounds, form layouts and static
models are documented with their configuration and plugin methods. [Shared resources](resources.md).

## Terrain diagnostics

`RUST_LOG=info,sculith::terrain=trace` records shared loading and generation
times, and reports preparation, encoding and queue admission for current
clients by edition. The encoding events report cache hits. Bedrock also
reports its initial cache lookup and the time spent waiting for transport
credit and the encoding worker. The queue events count admitted payload bytes
before connection compression and transport framing.
