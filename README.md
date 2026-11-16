<div align="center">

# Sculith

**Fast. Light. Safe.**

A Bedrock-native Minecraft server engine written in Rust.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.100.0-orange.svg)](rust-toolchain.toml)
[![Status](https://img.shields.io/badge/status-pre--alpha-yellow.svg)](docs/status.md)

[한국어](README.ko.md) · [Development status](docs/status.md) · [Roadmap](docs/roadmap.md) · [Contributing](CONTRIBUTING.md)

</div>

Sculith is being built around **one authoritative Bedrock-native world**, with Java and Bedrock protocol adapters sharing the same server state. Its long-term goals include Rust plugins, region-owned simulation, and a shared Bedrock content pipeline.

**This is an early, public development repository, not a production-ready release.** The default mode provides server discovery. Opt-in local Java and Bedrock adapters share a flat world and limited authoritative creative building. An unmodified Java 26.3 game has entered, rendered, moved and exchanged mouse-driven edits with an independent Bedrock client on Linux. [Screenshots and verification scope](docs/verification/2026-11-16-java26.3.md). Official Bedrock-game entry remains unverified.

## Compatibility

The checked-in release references are:

| Edition | Reference version | Protocol | Current implementation |
| --- | --- | --- | --- |
| Bedrock | 1.26.60 | 2225 | Discovery; RakNet and local NetherNet login, encryption, resource-pack negotiation/delivery, spawn and limited creative-builder preview |
| Java | 26.3 | 777 | Discovery; opt-in loopback login/configuration, flat-world chunks and limited creative building in the same owner |

These numbers identify development targets, **not verified gameplay support**. The gameplay support lists in [`protocol-targets.json`](protocol-targets.json) remain empty. Preview/Beta versions are separate targets, and RakNet tests do not establish NetherNet support.

## Implemented so far

- Java TCP and Bedrock UDP discovery in one Rust server process.
- Local Java 26.3 shared-world login, negotiated vanilla registries/tags, full spawn chunks, initialization, keepalive and authoritative movement/building. [Scope, source configuration and wire-fact provenance](docs/java-preview.md).
- Shared MOTD and initialized-session counts.
- Bounded RakNet session handling, compression, signature verification and encryption using pinned Rust libraries.
- Local NetherNet HTTP/WebRTC with signed answers and persistent operator keys; a real-socket headless test shares committed world changes with a RakNet client. See [transport scope and verification](docs/nethernet.md).
- An opt-in Bedrock flat-world spawn with 25 chunks, bounded view updates and session cleanup.
- Authoritative movement checks and limited creative block placement/breaking, with server-owned starter items and committed block deltas shared between sessions.
- A single region owner, revision-checked block commits, compact uniform/palette sections and immutable copy-on-write snapshots.
- Optional versioned, checksummed region snapshots loaded at startup and saved after sessions close on orderly shutdown. Periodic autosaving is pending.
- Trusted-executable Rust plugin SDK v2 with v1 compatibility, lifecycle/MOTD, initialized-player join/leave and committed-block hooks, plus owner-validated private messages in both editions. [Actual game evidence](docs/verification/2026-11-16-plugin-api-v2.md).
- Explicit Bedrock PNG-to-Java texture overrides, with a conversion report describing unsupported assets.
- Optional automatic delivery from one frozen Bedrock pack to native clients and a converted Java ZIP, with immutable hashes/cache IDs and required-pack acknowledgements. [Configuration and test scope](docs/content-delivery.md). The stock Java game renders the server-delivered checkerboard texture.
- Unit, codec, ownership, real-socket and executable smoke tests.

Multiplayer entity visibility, general inventory transactions, physics/gravity, public content hosting, and complete resource-pack conversion are still under development. WASI Component plugins, region-parallel ticking, public NetherNet, cameras, a Fabric Companion and packet-only LOD are planned work.

## Windows quick start

For local testing, download the `windows-x86_64.zip` from the [development preview](https://github.com/SKYRPG8957/Sculith/releases/tag/v0.1.0-preview.10), extract the entire archive, and double-click **`start.cmd`**. Use **`diagnose.cmd`** for connection-stage logs. No Rust, Python or Java installation is required by the server.

In Bedrock on the same PC, add **`127.0.0.1`**, port **`19132`**. Press **Ctrl+C** in the server window to save and stop. The launcher creates `sculith.toml` only when missing and preserves `world/region.sculith`.

Java **26.3** uses **`127.0.0.1:25565`** and shares the same owner and committed block changes. Existing configurations do not automatically enable Java sessions; see [the Java preview guide](docs/java-preview.md).

Preview 10 selects the local, offline-identity Java and Bedrock previews and includes the actual-game wire corrections and a Rust gameplay plugin. Official Linux Java entry/building and plugin-message rendering are verified; official Bedrock-game entry and Windows game rendering remain unverified. See [Windows setup and connection troubleshooting](docs/windows-quickstart.md). The preview release includes SHA-256 checksums, build metadata and dependency license texts.

The Windows GDK `1.26.63` Preview 6 retest completed login, then Sculith rejected an Interact/33 notification before initialization and disconnected with `InitialConnection-40`. Preview 7 corrected this; Preview 10 retains that handling. Both transport regression tests and independent startup/spawn/full-chunk probes pass on Windows; official Bedrock-game entry remains unverified.

## Build from source

Install [Rust](https://www.rust-lang.org/tools/install) and a native linker. The repository pins its Rust toolchain and dependency lockfile. Python 3 is needed for the executable smoke tests.

```sh
git clone https://github.com/SKYRPG8957/Sculith.git
cd Sculith
cargo build --workspace --locked
```

Linux x86_64 development checks and native Windows packaging checks are configured separately. Recorded results are in [development status](docs/status.md).

## Run

```sh
cargo run --locked -- check --config sculith.example.toml
cargo run --locked -- run --config sculith.example.toml
```

The defaults bind Java to `127.0.0.1:25565/TCP` and Bedrock to `127.0.0.1:19132/UDP`. They are local development listeners. Stop with Ctrl+C.

For an optional, loopback-only Bedrock protocol preview:

```sh
cargo run --locked -- run --config examples/bedrock-login-preview.toml
```

This test configuration accepts self-signed identities. It does **not** prove online Microsoft account authentication or official Bedrock-client compatibility. Online authentication is required by default when sessions are enabled; it needs outbound HTTPS access to the official Minecraft discovery/OIDC public-key services. Authentication failures never silently enable offline mode.

The preview supplies Stone, Dirt and Grass starter slots. The server checks movement timing/collision, building permission, reach, line of sight and held items before committing. This is a limited creative-builder implementation, not complete Minecraft movement or inventory behavior.

To exercise both adapters against the same owner from current source, use `examples/crossplay-preview.toml`. Java 26.3 connects to `127.0.0.1:25565`; Bedrock uses `127.0.0.1:19132`. Both are explicitly offline, same-machine previews. Java online authentication and other Java versions are not implemented.

The example stores its region in `world/region.sculith`. To enable persistence in another configuration, set `snapshot_path = "world/region.sculith"` under `[world]` (relative to the configuration file). Ctrl+C/SIGTERM triggers the final save; abrupt termination can lose changes since startup. Existing foreign or corrupt snapshots cause an error instead of being overwritten.

## Rust plugins

```sh
cargo build --workspace --locked
cargo run --locked -- run --config examples/with-plugin.toml
```

The original welcome example changes the common MOTD. The new gameplay example sends a private welcome after player initialization and observes accepted block changes; run it with `examples/gameplay-preview.toml`. Current plugins are trusted executable JSON-lines processes, not a sandbox. The official host design will evaluate the WASI Component Model, resource limits and region-aware APIs.

Public Preview 10 includes game API v2 and `plugins/sculith-gameplay-plugin.exe`, enabled in new Windows configurations. It sends each initialized player a private welcome and logs committed block changes. Existing configurations are preserved and do not automatically add plugins.

See the [plugin SDK guide](docs/plugins.md) and [example source](examples/welcome-plugin/src/main.rs).

## Content compatibility

Bedrock packs are intended to be the source assets. **Vanilla Java cannot directly execute arbitrary Bedrock UI JSON, geometry, Molang or Script API code.**

- **Java Vanilla Compatibility Mode** will use stock resource packs, display entities, dialogs and supported packets, with explicit fallbacks and unsupported-feature reports.
- **Java Enhanced Compatibility Mode** is planned around an optional Fabric-based Sculith Companion for higher-fidelity rendering, animation and UI.

Only explicit static PNG overrides are implemented today. The original checkerboard example can be converted into a Java `26.3` pack:

```sh
mkdir -p packs/generated
cargo run --locked -- pack convert \
  --input examples/pack/bedrock \
  --mapping examples/pack/textures.json \
  --output packs/generated/java-demo \
  --pack-format 97 --pack-format-minor 1
```

The output must be a new directory. Current source also supports [automatic local pack delivery](docs/content-delivery.md) with `examples/content-preview.toml`; public Preview 10 predates this addition. Sound/model/UI conversion remains unfinished.

## Development checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo build --workspace --locked
python3 scripts/smoke.py
python3 scripts/smoke.py --sessions
```

Protocol-client tests, real socket tests and official game-client tests are reported separately. Tests that use a headless protocol implementation do not establish successful rendering or gameplay in Minecraft.

An initial public alpha is targeted for **December 31, 2026**. Work is tracked by implemented and verified milestones in the [roadmap](docs/roadmap.md) and [status report](docs/status.md).

## References and licensing

We study [Dragonfly](https://github.com/df-mc/dragonfly) for Bedrock-native server boundaries, [Geyser](https://github.com/GeyserMC/Geyser) for edition-translation constraints, and [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) for Rust server organization. [bedrock-rs](https://github.com/bedrock-crustaceans/bedrock-rs) and [RakNet](https://github.com/bedrock-crustaceans/raknet) and [NetherNet](https://github.com/bedrock-crustaceans/nethernet) are pinned Apache-2.0 dependencies.

Sculith's source remains under the existing **MIT License**. References are not a claim that another project's implementation was copied or that its features are implemented here. See [architecture and versioned sources](docs/architecture.md) and [third-party notices](THIRD_PARTY_NOTICES.md). The example texture is original; Minecraft game assets are not bundled.

Sculith is an independent project and is not affiliated with Mojang or Microsoft.
