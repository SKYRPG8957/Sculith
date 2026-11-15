# Development status — 2026-11-16 KST

**Current Milestone:** Milestone 1, Bedrock-native login and first world spawn. The overall goal remains active.
Canonical repository: `SKYRPG8957/Sculith`. Existing MIT license and origin are preserved.
The full requirements are in [engineering-directive.md](engineering-directive.md).

## Implemented

- Existing Java TCP and Bedrock UDP discovery, trusted executable Rust plugin prototype and explicit PNG conversion retained.
- Opt-in RakNet sessions using pinned Rust libraries, compression, signed identity verification, encryption and ordered empty-pack negotiation.
- Bounded session/message queues with byte budgets, login/spawn timeout and owned session cleanup.
- Protocol-independent five-block registry, uniform/palette sections, a single region owner, revision-checked atomic edits and COW snapshots.
- One bounded world-owner mailbox used by the Bedrock adapter; 24-section chunk encoding, 25 flat spawn chunks and local-player initialization.
- Versioned/checksummed immutable region snapshots with validated disk reload and conservative atomic file replacement. Server autosave integration is pending.
- Read-only Adventure preview with initialized session counting. Login deadlines do not terminate already initialized sessions.
- Explicit official online authentication initialization. Public-key lookup failure does not silently enable offline authentication.

## Verified

- Rust 1.100.0 workspace build and Clippy for all targets with warnings denied.
- 54 unit/integration-style tests passed; the region `!Sync` compile-fail doctest also passed as expected (55 total checks).
- Real UDP headless protocol-client tests cover compression/encryption, signed login rejection, stale protocol rejection,
  ordered packs, cancellation, spawn entity-ID validation, 25 decoded chunk packets, session lifetime and cleanup.
- Every voxel of a flat chunk (98,304 blocks) independently decoded and compared; negative sections, asymmetric edits,
  1/2/3-bit packing and the latest `LevelChunkPacket` codec roundtrip verified.
- Both built-executable smoke modes passed: Java/Bedrock discovery, Rust plugin, ping, malformed traffic,
  Java login rejection, shutdown and pack conversion; `--sessions` also confirms a real RakNet connection-request reply.
- Snapshot storage tests cover disk roundtrips, immutable captures, checksums, truncation, version/coordinate/revision validation,
  stale concurrent saves, and preservation of foreign/corrupt/read-only/symlink destinations.
- Latest-release reference check still reports Java `26.3 / 777` and Bedrock `1.26.60 / 2225`. These are references, not full gameplay support declarations.

## Failed / externally constrained

- Official discovery request to `client.discovery.minecraft-services.net` returned proxy HTTP 403.
  The running restricted network has no Minecraft-service domain allowed. No credentials are needed for this public-key request.
- Only that required domain was added to the environment draft; the save was confirmed and returned `requires_publish: true`.
  Runtime access has not changed. Applying it requires review/save/publication through environment settings; no publication was performed.
  Downstream OIDC/JWKS destinations still need to be discovered and verified when the first request is allowed.
- Official Java and Bedrock game clients have not been used to verify world entry or rendering.
- No automated test remains failing in the final recorded run. The expected compile-fail result above verifies ownership restrictions.

## Performance

Reproduction: `cargo run --release --locked -p sculith-world --example world_bench -- 10000`.
One Linux x86_64 cloud run with the pinned compiler measured:

| Operation | 10,000 operations |
| --- | --- |
| Immutable snapshot Arc clone | 101.153 µs |
| Flat chunk snapshots | 2.358297 ms |
| Single-block owner commits | 751.13 µs |
| Packed ground section scan, 4,096 blocks each | 26.136598 ms |

The flat chunk's internal section records total 1,077 bytes; this excludes object/Arc overhead and is not total RAM usage.
The separately encoded Bedrock payload is 1,282 bytes. These microbenchmarks do not establish TPS, network capacity,
an optimization percentage, or a performance advantage over a complete Dragonfly/Pumpkin server.

## Known Limitations

- Default mode remains discovery-only. Native spawn is an explicit loopback development preview until online and official-client checks pass.
- Online authentication has not been end-to-end verified. Offline test identities are restricted to loopback binds.
- Spawn preview is read-only: movement, block actions, inventories and physics do not yet commit authoritative game state.
- Other players/entities are not displayed, and Java cannot yet enter the world. No complete crossplay claim is made.
- NetherNet has not been integrated or tested. RakNet results do not imply NetherNet support.
- Only a single region owner is implemented; no parallel region tick scheduler, migration, merging or splitting exists yet.
- Plugins currently expose lifecycle/MOTD through trusted executable processes. WASI Component execution, game APIs,
  commands, permissions, region/entity scheduling and isolation limits are not implemented.
- Conversion remains explicit static PNG overrides. Sound/model/UI conversion, content IR, Companion and pack delivery are pending.
- Latest protocol types expose camera instruction, preset, aim-assist and spline packets, but Sculith has no camera API or scenario tests yet.
- REA/disassembler tools were searched in the actual connected-tool catalog and were unavailable. No REA commands or results are fabricated.

## Next Action

Connect validated movement/block intents to the single region owner and send committed deltas; preserve bounds and revision semantics.
Then wire the snapshot storage API into the runtime and implement Java login/registry/chunk mapping against that same owner, while advancing actual-client validation.
Online public-key access remains an independent environment prerequisite. Required gameplay/content features are not removed from the objective.
