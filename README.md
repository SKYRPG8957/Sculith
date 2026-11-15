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

**This is an early, public development repository. It is not a playable crossplay server or a production-ready release.** The default mode provides server discovery; an opt-in Bedrock path exercises encrypted login, flat-world spawn and limited authoritative creative building with headless protocol clients. Official game-client entry and rendering have not yet been verified.

## Compatibility

The checked-in release references are:

| Edition | Reference version | Protocol | Current implementation |
| --- | --- | --- | --- |
| Bedrock | 1.26.60 | 2225 | Discovery; opt-in RakNet login, encryption, empty-pack negotiation, spawn and limited creative-builder preview |
| Java | 26.3 | 777 | Server-list status and ping; world login is not implemented |

These numbers identify development targets, **not verified gameplay support**. The gameplay support lists in [`protocol-targets.json`](protocol-targets.json) remain empty. Preview/Beta versions are separate targets, and RakNet tests do not establish NetherNet support.

## Implemented so far

- Java TCP and Bedrock UDP discovery in one Rust server process.
- Shared MOTD and initialized-session counts.
- Bounded RakNet session handling, compression, signature verification and encryption using pinned Rust libraries.
- An opt-in Bedrock flat-world spawn with 25 chunks, bounded view updates and session cleanup.
- Authoritative movement checks and limited creative block placement/breaking, with server-owned starter items and committed block deltas shared between sessions.
- A single region owner, revision-checked block commits, compact uniform/palette sections and immutable copy-on-write snapshots.
- Optional versioned, checksummed region snapshots loaded at startup and saved after sessions close on orderly shutdown. Periodic autosaving is pending.
- An initial trusted-executable Rust plugin SDK with lifecycle hooks and a shared MOTD action.
- Explicit Bedrock PNG-to-Java texture overrides, with a conversion report describing unsupported assets.
- Unit, codec, ownership, real-socket and executable smoke tests.

Java world entry, multiplayer entity visibility, general inventory transactions, physics/gravity, content distribution, and complete resource-pack conversion are still under development. WASI Component plugins, region-parallel ticking, NetherNet, cameras, a Fabric Companion and packet-only LOD are planned work.

## Windows quick start

For local testing, download the `windows-x86_64.zip` from the [development preview](https://github.com/SKYRPG8957/Sculith/releases/tag/v0.1.0-preview.2), extract the entire archive, and double-click **`start.cmd`**. Use **`diagnose.cmd`** for connection-stage logs. No Rust, Python or Java installation is required.

In Bedrock on the same PC, add **`127.0.0.1`**, port **`19132`**. Press **Ctrl+C** in the server window to save and stop. The launcher creates `sculith.toml` only when missing and preserves `world/region.sculith`.

This selects the local, offline-identity Bedrock preview. **Official game-client entry/rendering remain unverified; Java world entry is not implemented.** See [Windows setup and connection troubleshooting](docs/windows-quickstart.md). The preview release includes SHA-256 checksums, build metadata and dependency license texts.

The first reported Windows GDK `1.26.63` attempt failed with `Terracotta / InitialConnection-149`; the cause is still under investigation. Preview 2 improves diagnostics and does not claim to fix that failure.

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

The example stores its region in `world/region.sculith`. To enable persistence in another configuration, set `snapshot_path = "world/region.sculith"` under `[world]` (relative to the configuration file). Ctrl+C/SIGTERM triggers the final save; abrupt termination can lose changes since startup. Existing foreign or corrupt snapshots cause an error instead of being overwritten.

## Rust plugins

```sh
cargo build --workspace --locked
cargo run --locked -- run --config examples/with-plugin.toml
```

The example plugin changes the common MOTD seen by both editions. Current plugins are trusted executable processes communicating over versioned JSON-lines; they are not sandboxed. The official plugin-host design will evaluate the WASI Component Model, resource limits and region-aware APIs.

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

The output must be a new directory. Automatic pack delivery and sound/model/UI conversion are not implemented.

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

We study [Dragonfly](https://github.com/df-mc/dragonfly) for Bedrock-native server boundaries, [Geyser](https://github.com/GeyserMC/Geyser) for edition-translation constraints, and [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) for Rust server organization. [bedrock-rs](https://github.com/bedrock-crustaceans/bedrock-rs) and [RakNet](https://github.com/bedrock-crustaceans/raknet) are pinned Apache-2.0 dependencies.

Sculith's source remains under the existing **MIT License**. References are not a claim that another project's implementation was copied or that its features are implemented here. See [architecture and versioned sources](docs/architecture.md) and [third-party notices](THIRD_PARTY_NOTICES.md). The example texture is original; Minecraft game assets are not bundled.

Sculith is an independent project and is not affiliated with Mojang or Microsoft.
