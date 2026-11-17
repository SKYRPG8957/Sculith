# Bedrock block-use interoperability

This Go client uses pinned gophertunnel and Dragonfly implementations. It is a
test tool, not a server dependency. Use a scratch world and loopback listeners.
It connects through NetherNet; it does not verify Windows GDK rendering.

`--pick-check` also checks hotbar selection against the pick behavior in
Dragonfly `0f9a3a573626`: select an existing hotbar stack without moving it,
or create a new Creative item in the first empty hotbar slot. The check sends
different client hint values, including 9, moves a stack into storage through
`ItemStackRequest`, and picks it back into an occupied hand. It verifies all 36
inventory slots after each pick. Run through the combined harness with `--picking`.

`--actor-pick-check` picks a nearby pig, reuses its egg after selecting another
hotbar slot, and rejects a distant pig and a 64-bit ID that would alias the nearby
pig if truncated. The harness option `--actor-picking` prepares those entities.
The client-origin latency echo orders the rejection checks before the valid pick.
Actor data copying and entities without a mapped spawn egg are not implemented.

`--pick-events` and `--pick-mutation` are driven by the harness's `--pick-events`
option with the native connection fixture installed. They check cancellation and
inventory changes made during a Rust slot-selection callback. The combined
harness also exercises the same operations over the stock Java protocol.

Prepare a Java 26.3 flat world with grass at Y=-61. In the official Java server:

```
forceload add 0 0
setworldspawn 0 -60 0
setblock 2 -60 2 minecraft:diamond_block
save-all
stop
```

Wait for the force-loaded chunk before setting the block. Copy the stopped
world to the Sculith scratch directory, select `world.chunk.type = "anvil"`,
and start Sculith there. For this local test only, use Creative mode and disable
both edition authentication settings and `online_mode`. Set `spawn_protection = 0`
in this scratch configuration: the probe builds beside spawn. Restrict both
listeners to `127.0.0.1`. Leave the original map untouched.

```sh
go build -mod=readonly -o probe .
./probe http://127.0.0.1:19132
```

The probe picks the imported diamond block, places and breaks through standalone
and input-carried transactions, tests a negative predicted held-stack ID and an
input-carried slot journal, then leaves a new marker at `(3,-60,2)`. Updates are
checked against Dragonfly's block-state registry. A timeout or missing committed
update fails the probe.

Stop the server with `stop`, restart the same world, then:

```sh
./probe http://127.0.0.1:19132 --read-persisted
```

This must pick the newly saved diamond marker. To check Anvil output with the
official Java server, use another copy of the stopped world, force-load chunk
`0,0`, then run `execute if block 3 -60 2 minecraft:diamond_block run say
SCULITH_SAVED_MARKER`. That also checks a different implementation can read the
saved region.

The binary fixture beside the Rust PlayerAuthInput tests was independently
encoded by gophertunnel `e2251a60f71d` for protocol 2225. It includes the mainhand
byte previously missing from the input-carried item-use decoder.

For a shared-pack transfer and Java configuration regression test, from the
repository root:

```sh
python3 tests/interop/shared_resources.py /path/to/sculith /path/to/probe \
  --world /path/to/stopped-flat-world --delayed --building
```

This copies the world and test resource fixtures into a temporary directory. It
checks both archive transfers, a delayed and fragmented Java pack acknowledgement,
one configuration exchange, Java play entry, and the four native building paths.
It changes the source after startup to check that active transfers use the same
frozen snapshot. The copied test world disables spawn protection and removes mobs
before the building probe. No original world is changed. This is protocol testing;
it does not check rendering or audio playback.

With the Rust fixture installed, `./probe http://127.0.0.1:19132 --forms-check`
checks shared simple, modal, custom and dismissed forms. See the fixture README
for the combined `--forms --plugin` harness invocation.

The same harness accepts `--models` with the Rust fixture. It checks native actor
registration, spawn metadata, scale and position updates, removal and late
watchers against Java, then restarts the copied world and checks persistence.
The `--models-check` and `--model-snapshot` probe modes are driven by that harness;
`--model-unavailable` checks that a missing mapping does not leak a fallback actor.

`--packet-edits` checks ID and payload replacement, cancellation and an untouched
sentinel from the stable Rust Wasm fixture. `--connections-plugin` installs the
native connection fixture into the scratch server and checks the pre-player
stages. See the [connection fixture](../connections/README.md) for its build and
harness invocation.
