# Sculith

Rust Minecraft server for Bedrock clients with optional Java connectivity. The original foundation is credited in [UPSTREAM.md](UPSTREAM.md).

The target remains one Bedrock-based simulation shared by both editions, region
ownership and parallel simulation, Rust plugins, and translated resources for
unmodified Java clients. Performance work targets tick latency, chunk throughput,
memory use and network cost, measured under the same world and client workload.

This branch is the engine migration. It is not a production release. Java 26.3
and Bedrock 1.26.63 (protocol 2225) are the initial targets.

## Build

```sh
cargo build --release --locked -p sculith
```

Run `sculith` (`sculith.exe` on Windows) from the server directory. A fresh server
creates `server.properties` for common settings and `sculith.toml` for advanced
options. Open `Settings.cmd` on Windows or run `sculith --init-server-properties`
to create the basic file from an existing configuration. Stop with `stop`.

Edit `server-name` and `server-port` in `server.properties` for Bedrock, which
uses NetherNet on TCP and UDP. Set `java-enabled=true` for Java access on
`java-port`, which defaults to TCP 25565. Authentication is on by default.
Properties present in the file take precedence over their TOML equivalents.
`sculith --check` prints the effective listeners and world settings.
Existing TOML-only installations can keep using `sculith.toml`.
Plugins use the upstream Rust/Wasm API.

Place a 64×64 PNG named `server-icon.png` in the server directory to set the
Java server list icon.

Shared resources and their Rust plugin APIs are opt-in. See the
[resource guide](docs/sculith/resources.md) for sounds, forms and static models.
Protocol plugins can use the [packet events](docs/sculith/packets.md).

## Worlds

To create a flat world, set `level-name` to a new folder name and
`level-type=FLAT` in `server.properties`. With TOML, use
`default_level_name` and `generator = "flat"` in the `[world]` table.
Sculith records the generator and seed in the world save for later starts.

Sculith supports Anvil, Linear and Pump storage. To bring in saved Bedrock
1.26.63 Overworld terrain, stop the source server and choose a new destination.
Run the import from the Sculith server directory.

```sh
sculith --import-bedrock-terrain "/path/to/bedrock-world" world
```

Start Sculith normally to open the imported `world` from Java and Bedrock. The
import writes a separate Anvil world with mapped blocks and biomes, the world
seed and spawn. Supported chest and barrel inventories retain slots, counts,
potion variants and enchantments. The source retains entities, player records
and unsupported block-entity data. For a different destination, set
`level-name` in `server.properties` to that world folder.
See [the fork notes](docs/sculith/fork.md) for format limits.

## License

The server is GPL-3.0. Commercial hosting and distribution are allowed under its
terms. Distributed modified binaries need the corresponding source. Plugin SDK
crates keep their own MIT OR Apache-2.0 licenses.

See [upstream attribution](UPSTREAM.md) and [LICENSE](LICENSE). The previous MIT
Sculith engine remains in its original history; it has not been relicensed.
