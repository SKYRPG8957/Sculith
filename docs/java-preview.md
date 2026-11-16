# Java 26.3 shared-world development preview

Current source implements protocol **777** against the same `WorldHandle` and
single region owner as Bedrock. This is an explicitly offline, loopback-only
development path. Unmodified stock Java entry, rendering, keyboard movement and
mouse building have been verified on Linux. [Actual-client evidence](verification/2026-11-16-java26.3.md).
[Windows Preview 11](https://github.com/SKYRPG8957/Sculith/releases/tag/v0.1.0-preview.11)
includes the actual-client wire corrections, enables both local paths in new
configurations and bundles a Rust gameplay plugin. Its Korean/emoji welcome was
also [displayed in the unmodified Java game](verification/2026-11-16-plugin-api-v2.md).
The earlier Preview 8's Java path fails with the official game.

## Run from source

```sh
cargo build --workspace --locked
cargo run --locked -- run --config examples/crossplay-preview.toml
```

On the same machine, enter `127.0.0.1:25565` in Java 26.3 multiplayer.
Bedrock uses `127.0.0.1`, port `19132`. No client mod, proxy or separate
Minecraft server process is used. A Java runtime is needed by the Java game,
not by Sculith itself.

Existing configurations do not automatically enable Java sessions. The opt-in is:

```toml
[java]
sessions = true
require_authentication = false
login_timeout_secs = 30
```

The Java listener must be enabled and bound to a loopback address. Public/LAN
bindings and online Java authentication are rejected in this preview. The source
default remains discovery-only. Offline profile identifiers are conventional
Java name-derived UUIDs; client-proposed UUIDs do not establish account identity.

## Implemented and tested

- Login/configuration with the matching negotiated vanilla core, 32 registries,
  773 tags, creative abilities, clock, three server-owned starter slots and 25
  chunks from owner snapshots.
- Protocol-777 palettes, fluid counts, heightmaps and byte-mask lighting. Every
  one of the 2,457,600 spawn voxels is decoded by an independent Python client.
- Activation requires both teleport confirmation and PlayerLoaded. Pending
  game input cannot mutate the owner. Initialized sessions count toward capacity.
- Movement uses the existing owner speed/collision/bounds checks. Main-hand
  placement and creative breaking use its permission, reach, sight and item checks.
- Prediction sequences reject replay; corrections gate subsequent movement and
  building until acknowledged. Client inventory/creative-stack claims reset to
  the server's 41 inventory slots rather than becoming authoritative.
- Committed changes reach both editions. A real-socket regression joins one
  Java, one RakNet and one NetherNet client to one owner and checks bidirectional
  changes, the final snapshot/revision and Java disconnect cleanup.
- Bounded input frames, connection counts, packet/byte rates, writes, startup,
  teleport and keepalive deadlines. Framing retains partially read state when
  a socket read loses a `select!` race to another event.

```sh
python3 scripts/java_smoke.py
cargo test --locked -p sculith-server java_and_both_bedrock_transports_share_bidirectional_commits_in_one_owner
```

These commands are headless wire/owner checks. The separately recorded official
Linux game test rendered the world and exchanged mouse-driven changes with the
independent Bedrock client. Windows CI runs the independent probe against the
portable release executable; official Windows game rendering remains unverified.

## Limits

Only Java 26.3 is implemented. No online Java account authentication, encryption
or compression is provided by this local path. The view radius is fixed at two
chunks and clipped to the existing region. Only Air, Bedrock, Stone, Dirt and
Grass are mapped. Sky lighting is direct vertical lighting; lateral propagation
is pending. Other players/entities are not yet displayed.

General inventory transactions, chat, survival, gravity, crafting and shared
models/sounds/UI remain unfinished. Current source supports optional
[automatic local texture-pack delivery](content-delivery.md), included in Preview 11.
The known vanilla core supplies registry definitions, not Bedrock pack conversion.
Both editions use the same orderly-shutdown region persistence; periodic saves
and persistent player data are pending.

## Reproduce version facts

Primary numeric IDs come from the official **26.3** data reports. The official
server report tool JAR has SHA-1
`33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c`, available through the public
[version manifest](https://piston-meta.mojang.com/mc/game/version_manifest_v2.json).
Development extraction requires Java 25 or newer; the server product does not.

```sh
java -DbundlerMainClass=net.minecraft.data.Main -jar server.jar --reports
python3 scripts/generate_java_protocol_facts.py --server-jar server.jar --reports generated/reports --output crates/sculith-server/src/protocol/java/protocol777.json
```

The generator checks the pinned tool checksum. It stores only names, tag
memberships, packet IDs and the five block/item IDs. Registry entries are sent
without element definitions only after the client selects `minecraft/core/26.3`.
Block state Bedrock is **88** in this version; an older version's 85 is incorrect.

Packet layouts and synchronized registry names were cross-checked against the
MIT [MCProtocolLib source at 65efb9da](https://github.com/GeyserMC/MCProtocolLib/tree/65efb9dac372709f49f4395ae4361642bc012328).
No reference server, Minecraft implementation or game assets are distributed.
