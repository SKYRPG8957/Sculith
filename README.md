# Sculith

**Fast. Light. Safe.**

Rust Minecraft server for Java and Bedrock clients, based on [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin).

The target remains one Bedrock-based simulation shared by both editions, region
ownership and parallel simulation, Rust plugins, and translated resources for
unmodified Java clients. Performance work targets tick latency, chunk throughput,
memory use and network cost, measured under the same world and client workload.

This branch is the engine migration. It is not a production release. Java 26.3
and Bedrock 1.26.63 (protocol 2225) are the initial targets.

## Build

```sh
cargo build --release --locked -p pumpkin --bin sculith
```

Run `sculith` (`sculith.exe` on Windows) from the server directory. The first
start creates `pumpkin.toml`. Existing Pumpkin configuration names and crate
names are retained to keep upstream updates manageable. Stop with the console
command `stop`.

Java uses TCP 25565. Bedrock uses NetherNet on TCP/UDP 19132. Authentication is
on by default. Plugins use the upstream Rust/Wasm API; Sculith's previous
prototype plugins need to be ported.

Shared resources and their Rust plugin APIs are opt-in. See the
[resource guide](docs/sculith/resources.md) for sounds, forms and static models.
Protocol plugins can use the [packet events](docs/sculith/packets.md).

## Worlds

Pumpkin's Anvil, Linear and Pump storage backends are retained. Back up a world
before opening it. The Java 26.3 Anvil import test and remaining gaps are recorded
in [the fork notes](docs/sculith/fork.md). Bedrock LevelDB worlds are not supported
yet. Joining from Bedrock does not add LevelDB world support.

## License

The server is GPL-3.0. Commercial hosting and distribution are allowed under its
terms. Distributed modified binaries need the corresponding source. Plugin SDK
crates keep their own MIT OR Apache-2.0 licenses.

See [upstream attribution](UPSTREAM.md) and [LICENSE](LICENSE). The previous MIT
Sculith engine remains in its original history; it has not been relicensed.
