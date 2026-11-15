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

    python3 scripts/gophertunnel_smoke.py --client target/gophertunnel-probe

On Windows use an .exe output path. CI runs this against the portable release executable.
This is an independent headless protocol test, not official Minecraft rendering verification.
