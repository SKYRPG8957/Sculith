# Java 26.3 connections

Current source implements protocol **777** against the same `WorldHandle` and
single region owner as Bedrock. Online mode uses the official account-session
service and an encrypted TCP stream; offline mode is restricted to loopback.
Unmodified stock Java offline entry, rendering, keyboard movement and
mouse building have been verified on Linux. [Actual-client evidence](../development/verification/2026-11-16-java26.3.md).
Current source also provides nearby peers, counted creative inventory, public chat,
Rust plugin commands and selected shared textures/forms/sounds/static models.
Preview packages have been withdrawn; use a source build.

## Run from source

```sh
cargo build --locked
cargo run --locked -- run --config examples/configs/crossplay.toml
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

This offline listener must be enabled and bound to loopback. The source default
remains discovery-only. Offline profile identifiers are conventional Java
name-derived UUIDs; client-proposed UUIDs do not establish account identity.

For an online Java listener, keep authentication enabled:

```toml
[java]
sessions = true
require_authentication = true
login_timeout_secs = 30

[network.java]
enabled = true
bind = "0.0.0.0:25565"
```

The server generates an ephemeral RSA-2048 key, validates a fresh challenge and
uses AES-128/CFB8 for all subsequent frames. HTTPS `hasJoined` verification must
return the requested account name and UUID before world reservation. Authentication
workers are capped at 16, including work left running after a socket timeout.
The session service needs outbound HTTPS access. Authentication failure never
falls back to an offline profile. No Microsoft access token is sent to Sculith.

Local socket tests exercise encrypted world entry with a test session verifier,
invalid challenges/profiles and cancellation. An independent OpenSSL-backed client
also reaches the actual official session endpoint and receives an encrypted
rejection for an account that did not join. This does not establish successful
sign-in with a real Microsoft account. The earlier actual-game evidence remains
offline evidence. [Public pack delivery](packs.md) accepts an explicit HTTP(S)
advertised URL; authenticated NetherNet hosting has its own [media configuration](hosting.md).

## Implemented and tested

- Login/configuration with the matching negotiated vanilla core, 32 registries,
  773 tags, creative abilities, clock, an initially empty 36-slot inventory and 25
  chunks from owner snapshots.
- Protocol-777 palettes, fluid counts, heightmaps and byte-mask lighting. Every
  one of the 2,457,600 spawn voxels is decoded by an independent Python client.
- Activation requires both teleport confirmation and PlayerLoaded. Pending
  game input cannot mutate the owner. Initialized sessions count toward capacity.
- Movement uses the existing owner speed/collision/bounds checks. Main-hand
  placement and creative breaking use its permission, reach, sight and item checks.
- Prediction sequences reject replay; corrections gate subsequent movement and
  building until acknowledged. The owner validates four creative block items,
  counts 1–64 and player slots 9–44 before committing permitted replacements.
  Pickup/right-click splitting and hotbar swaps compute from owner state,
  without trusting hashed client predictions. Unsupported actions restore state.
- Stock creative screens submit resulting slot replacements and keep their carried
  stack locally. Successful edits acknowledge only the affected slot; resetting
  the entire menu would erase that unsent carried stack. Actual Java mouse/key
  input verified splitting Stone64, moving32 and swapping it with Dirt64.
- Public chat and plugin command input share owner text budgets. `/help` lists
  registered commands; `examples/configs/events.toml` provides the Rust `/hello` example.
- Initialized peer rosters, nearby avatars, movement and counted held items share
  owner decisions with Bedrock. Account skins, armour and offhand are pending.
- Committed changes reach both editions. A real-socket regression joins one
  Java, one RakNet and one NetherNet client to one owner and checks bidirectional
  changes, the final snapshot/revision and Java disconnect cleanup.
- Bounded input frames, connection counts, packet/byte rates, writes, startup,
  teleport and keepalive deadlines. Framing retains partially read state when
  a socket read loses a `select!` race to another event.

```sh
python3 tests/interop/java_smoke.py
cargo test --locked -p sculith-server java_and_both_bedrock_transports_share_bidirectional_commits_in_one_owner
```

These commands are headless wire/owner checks. The separately recorded official
Linux game test rendered the world and exchanged mouse-driven changes with the
independent Bedrock client. Windows CI runs the independent probe against the
portable release executable; official Windows game rendering remains unverified.

## Limits

Only Java 26.3 is implemented. Online authentication and encryption are present;
Java packet compression is pending. The view radius is fixed at two
chunks and clipped to the existing region. Only Air, Bedrock, Stone, Dirt and
Grass are mapped. Sky lighting is direct vertical lighting; lateral propagation
is pending. General entity simulation is not implemented.

General containers, drag/shift-click, drops, survival, gravity and crafting remain
unfinished. Current source supports selected [static models](models.md),
[sounds](sounds.md), [simple forms](forms.md) and
[automatic pack delivery](packs.md). Arbitrary Bedrock content
conversion is not implemented.
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
