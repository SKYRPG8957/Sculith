# Independent Bedrock client

This development-only Go/Pion client uses the MIT-licensed gophertunnel at source revision
e2251a60f71d5faa9784d6a4bc817c117cb6af78. It connects to the Rust server over NetherNet,
uses a generated offline test identity, negotiates packs and acknowledges the world spawn.
The Python harness also checks that Sculith activated the player.

The observer also uses MIT-licensed Dragonfly at
`0f9a3a5736267db60bf46e92d408f81be1de9a64` to independently decode every voxel
and biome in all 25 spawn chunks. It resolves network hashes against that
implementation's full vanilla block registry rather than copying Sculith's five
hashes. Gophertunnel independently reads the plains biome definition and empty
creative catalog. Missing or malformed startup data fails the probe.
It also requires all four structure registries before StartGame, reproducing
the protocol requirement behind the official client's MissingStructureData/148 failure.

Go dependencies are pinned by go.mod and go.sum. The test overrides the upstream's retracted
compress version with v1.20.4. No Go code or runtime is linked into the installed Rust server.
The test client is not included in the Windows preview ZIP.
Dragonfly and its embedded test registry are only in this development client,
never in the Rust server binary.

Build from this directory with Go 1.27.4:

    go build -mod=readonly -o ../../../target/gophertunnel-probe .

From the repository root:

    python3 tests/interop/gophertunnel_smoke.py --client target/gophertunnel-probe

On Windows use an .exe output path. CI runs this against the portable release executable.
This is an independent headless protocol test, not official Minecraft rendering verification.

Player membership/pose lifecycle (two Java peers, one native peer):

    python3 tests/interop/peer_smoke.py --client target/gophertunnel-probe

This also checks a Korean/emoji native name through Java's bounded ASCII profile
name and original display-name component. `--peer-visual` is a 30-second companion
for a separately running stock Java game; it skips flat-voxel checks on that
existing world and reports received Java movement. See [scope](../../../docs/internals/peers.md).

Held-item projection and false claim correction:

    python3 tests/interop/equipment_smoke.py --client target/gophertunnel-probe

`--equipment-visual` extends the actual-game companion with timed native hotbar
changes (Dirt after 7 seconds, empty after 14) and reports both sides' equipment.
It remains a headless native implementation, not official Bedrock rendering.

Local session load (Linux loopback):

    python3 scripts/bench/native_sessions.py --server target/release/sculith --client target/gophertunnel-probe --clients 32 --seconds 30 --output /tmp/sculith-native-load

The output directory must be new. Each client receives 25 chunks, sends a
67-packet burst, empty-hand/air-use transactions and movement at a requested 20 Hz, breaks one visible ground
block and checks latency fences through the final input. Separate loopback
source addresses preserve the server's four-sessions-per-IP limit; at most eight
clients negotiate concurrently. Limits are 64 clients and 300 active seconds.
The runner retains client results, server logs, bounded owner timings and Linux
process CPU/RSS counters. It verifies that all sent movement reached the owner.
This measures the current flat-world workload; it does not establish official
GDK rendering, internet latency, TPS, plugins or production player capacity.
