# Bedrock block-use interoperability

This Go client uses pinned gophertunnel and Dragonfly implementations. It is a
test tool, not a server dependency. Use a scratch world and loopback listeners.
It connects through NetherNet; it does not verify Windows GDK rendering.

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
both edition authentication settings and `online_mode`. Restrict both listeners
to `127.0.0.1`. Leave the original map untouched.

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
