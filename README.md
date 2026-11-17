# Sculith

**Fast. Light. Safe.**

A Bedrock-native Minecraft server written in Rust. Java and Bedrock connect to
one server and share the same world, game rules and Rust plugins.

[한국어](README.ko.md) · [Documentation](docs/README.md) · [Contributing](CONTRIBUTING.md)

Sculith is under development. Current gameplay is a local creative builder;
public hosting, survival and complete Bedrock content compatibility are unfinished.
Development targets Bedrock **1.26.63 / protocol 2225** and stock Java **26.3 / 777**.
[Compatibility and test status](docs/development/status.md).

## Build and run

Requires the pinned Rust toolchain and a C compiler/linker. Windows MSVC builds
also require CMake for the crypto backend.

```sh
git clone https://github.com/SKYRPG8957/Sculith.git
cd Sculith
cargo build --locked
cargo run --locked -- run --config examples/configs/crossplay.toml
```

Connect on the same PC:

| Client | Address |
| --- | --- |
| Bedrock | `127.0.0.1`, port `19132` |
| Java 26.3 | `127.0.0.1:25565` |

New players start with an empty inventory. Select the supported blocks from the
creative inventory, or middle-click a supported block in the world.

Use `list` to see connected players, `save` to checkpoint and `stop` to save and
shut down.

The example saves changed blocks to `world/region.sculith` every 60 seconds and
again on Ctrl+C. After a forced stop, restart restores the last completed save.
Changes since that save and player data are not persistent.
[Save settings](docs/guides/hosting.md#world-saving).
The previous preview downloads have been withdrawn; no new binaries are published.
[Windows setup](docs/guides/windows.md).
[Network hosting and ports](docs/guides/hosting.md).

## Features

- Shared chunks, player visibility, movement, creative building and chat.
- Counted 36-slot creative inventory with Stone, Dirt, Grass and Bedrock.
- Rust plugin world queries and atomic block edits through the region owner.
- Optional [plugin IPC monitoring and profiling](docs/guides/profiling.md).
- Rust plugin SDK with game events, commands, private messages, forms, sounds
  and static models. [Plugin guide](docs/guides/plugins.md).
- One Bedrock resource-pack source with selected Java texture, sound and
  static-model conversion. Arbitrary JSON UI, Molang and Script API are unfinished.
- Versioned region snapshots and bounded world-owner queues.

## Project layout

| Path | Purpose |
| --- | --- |
| `crates/` | Engine, protocols, resource conversion and plugin SDK |
| `examples/configs/` | Runnable server configurations |
| `examples/plugins/` | Rust plugins using the public SDK |
| `examples/pack/` | Original shared resource pack |
| `tests/interop/` | Independent Java, Bedrock and WebRTC clients |
| `scripts/` | Development tools and protocol generators |
| `packaging/` | Platform launch templates |
| `docs/` | Guides, internals and development records |

[Architecture and crate boundaries](docs/internals/architecture.md).

## Development

```sh
python3 scripts/check_architecture.py
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
python3 tests/interop/smoke.py
```

See [Contributing](CONTRIBUTING.md) for protocol tests and review conventions.

## License

[MIT](LICENSE). Commercial use is permitted. Third-party components are listed in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

We study [Dragonfly](https://github.com/df-mc/dragonfly),
[Endstone](https://github.com/EndstoneMC/endstone),
[Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) and
[Geyser](https://github.com/GeyserMC/Geyser) while keeping Sculith's implementation
and licensing independent. Minecraft game assets are not bundled.

Not affiliated with Mojang or Microsoft.
