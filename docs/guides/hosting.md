# Network hosting

Build from source with `cargo build --release --locked`. Keep authentication
enabled for remote players. Current gameplay and content limits are listed in
[development status](../development/status.md).

Save this as `sculith.toml`, replacing the example IP with the server's reachable
public or LAN IP:

```toml
[server]
motd = "Sculith"
max_players = 20

[world]
snapshot_path = "world/region.sculith"

[java]
sessions = true
require_authentication = true

[bedrock]
sessions = true
require_authentication = true

[network.java]
bind = "0.0.0.0:25565"

[network.bedrock]
bind = "0.0.0.0:19132"

[bedrock.nethernet_media]
ports = [19133, 19196]
advertised_ip = "203.0.113.10"
```

The media table enables NetherNet on a non-loopback listener unless explicitly
disabled. Offline identities remain restricted to loopback. The inclusive UDP
range must contain 1–64 ports, cover `max_players` and exclude the discovery UDP
port. A concrete signaling bind can supply the advertised IP when that field is
omitted; wildcard binds require an explicit IP of the same address family.

Allow or forward these ports:

| Protocol | Port | Use |
| --- | --- | --- |
| TCP | 25565 | Java |
| TCP | 19132 | Bedrock signaling |
| UDP | 19132 | Bedrock discovery and RakNet |
| UDP | 19133–19196 | Bedrock NetherNet media |

Use one-to-one UDP forwarding when behind NAT. Media sockets are allocated per
connection from this range; busy ports are preserved and an exhausted range
returns an unavailable response. Signaling must reach the server directly: media
destinations and sources are restricted to the actual TCP peer IP. Forwarded
headers, TURN and signaling HTTP proxies are not supported by this path.
IPv4 and IPv6 must each use matching signaling/media address families.

Validate and run:

```sh
target/release/sculith check --config sculith.toml
target/release/sculith run --config sculith.toml
```

On Windows, use `target\release\sculith.exe`. Keep outbound HTTPS available for
Minecraft account verification and signing-key refresh. Preserve
`world/nethernet-identity.pem`; the stable operator key signs Bedrock answers and
can trigger a first-use trust prompt in the game.

Java players enter the address with port 25565; Bedrock players use port 19132.
For automatic shared texture, sound and model assets, configure
[pack delivery](packs.md) with a client-reachable `content.public_url`. Both
editions still use the same authored pack and world. No client mod is required;
the game may ask players to accept the server pack.

Tests cover non-loopback HTTP, fixed UDP ports, ICE/DTLS/SCTP, test-issued account
verification, key binding and world entry. Official-account positive entry and
Internet/NAT deployment remain unverified. Stop with Ctrl+C to save the region;
player persistence, periodic saving and crash recovery remain unfinished.
