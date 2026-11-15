# Development status — 2026-11-16 KST

**Current Milestone:** Milestone 1, Bedrock-native login, spawn and authoritative creative-builder preview. The overall goal remains active.
Canonical repository: `SKYRPG8957/Sculith`. Existing MIT license and origin are preserved.
The full requirements are in [engineering-directive.md](engineering-directive.md).

## Implemented

- Existing Java TCP and Bedrock UDP discovery, trusted executable Rust plugin prototype and explicit PNG conversion retained.
- Opt-in RakNet sessions using pinned Rust libraries, compression, signed identity verification, encryption and ordered empty-pack negotiation.
- Bounded session/message queues with byte budgets, login/spawn timeout and owned session cleanup.
- Protocol-independent five-block registry, uniform/palette sections, a single region owner, revision-checked atomic edits and COW snapshots.
- One bounded world-owner mailbox used by the Bedrock adapter; 24-section chunk encoding, 25 flat spawn chunks, bounded view updates and local-player initialization.
- Server-owned player state, hotbar selection and limited creative placement/breaking, validated before revision commits.
- Movement uses monotonic client ticks plus a server-clock speed budget, swept body collision and region bounds. Building checks permission, rate, reach, line of sight, selected slot, held item and player-body overlap; Bedrock is immutable.
- Only committed block deltas reach interested sessions; stale events are skipped, revision gaps trigger snapshot resynchronization and a lagging subscriber disconnects explicitly.
- Versioned/checksummed immutable region snapshots with validated disk reload and conservative atomic file replacement. Optional runtime loading and orderly-shutdown saving are wired; periodic autosaving is pending.
- Creative-builder preview with initialized session counting and cancellation-safe player leases. Login deadlines do not terminate already initialized sessions.
- Explicit official online authentication initialization. Public-key lookup failure does not silently enable offline authentication.
- Windows preview packaging: a portable executable with static CRT, double-click launcher, local configuration, SHA-256 checksums, build metadata and locked dependency license texts. The PowerShell installer preserves existing directories and verifies the archive before installing.

## Verified

- Rust 1.100.0 workspace build and Clippy for all targets with warnings denied.
- Native Windows x64 build, Clippy, unit/doc tests, both release-executable network/persistence smoke checks and package validation passed for `0e782d5` in [Windows preview run 43741364940](https://github.com/SKYRPG8957/Sculith/actions/runs/43741364940).
- The extracted ZIP launcher passes configuration checks from a path with spaces and preserves edited user settings. Its x64 PE imports only Windows system DLLs; no separate VC++ runtime installation is required.
- [Windows preview v0.1.0-preview.1](https://github.com/SKYRPG8957/Sculith/releases/tag/v0.1.0-preview.1) is public (3,858,243-byte ZIP plus SHA-256 checksums). Windows PowerShell 5.1 installation, refusal to overwrite an existing directory, and the documented one-command installation passed in [run 43742465897](https://github.com/SKYRPG8957/Sculith/actions/runs/43742465897).
- 83 unit/integration-style tests passed; the region `!Sync` compile-fail doctest also passed as expected (84 total checks).
- Real UDP headless protocol-client tests cover compression/encryption, signed login rejection, stale protocol rejection,
  ordered packs, cancellation, spawn entity-ID validation, 25 decoded chunk packets, session lifetime and cleanup.
- Two encrypted UDP clients receive the same accepted placement/break delta. A rejected Bedrock break is corrected only for the requester; a forged held-item claim cannot commit. Valid eye-offset movement commits, while a large tick/teleport request is corrected.
- A real UDP client crossing a negative chunk boundary retains its negotiated one-chunk radius; only the three newly visible chunks are sent, with no overlap retransmission.
- An independent byte-level UDP client completes the cookie-checked OpenConnectionRequest1/2 exchange and confirms an invalid cookie is not admitted. It does not use the server's RakNet codec.
- Domain tests exercise replay/speed/collision/reach/permission/budget checks, disconnected-player queue cleanup and immutable captures. Changed runtime state survives snapshot reload.
- Every voxel of a flat chunk (98,304 blocks) independently decoded and compared; negative sections, asymmetric edits,
  1/2/3-bit packing and the latest `LevelChunkPacket` codec roundtrip verified.
- Both built-executable smoke modes passed: Java/Bedrock discovery, Rust plugin, ping, malformed traffic,
  Java login rejection, shutdown, snapshot save/restart/load, existing foreign-file protection and pack conversion; `--sessions` also confirms a real RakNet connection-request reply.
- Snapshot storage tests cover disk roundtrips, immutable captures, checksums, truncation, version/coordinate/revision validation,
  stale concurrent saves, and preservation of foreign/corrupt/read-only/symlink destinations.
- Latest-release reference check still reports Java `26.3 / 777` and Bedrock `1.26.60 / 2225`. These are references, not full gameplay support declarations.

## Failed / externally constrained

- The first official-client attempt reported by the user failed: Windows GDK `1.26.63`, `Terracotta`, `InitialConnection-149`. Account sign-in was already complete. The exact transport/login failure is unresolved; successful headless tests are not treated as a substitute. Connection-stage and bounded handshake diagnostics are being added for the next preview.
- The first Windows smoke run found console shutdown killing a plugin before its lifecycle hook. Windows workers now run without the host console; the corrected run verifies the Stopping event and final world save.
- Official discovery initially returned proxy HTTP 403 under the earlier restricted network. The current environment reports enforced unrestricted networking; a fresh request to `client.discovery.minecraft-services.net` returned HTTP 200. Downstream OIDC/JWKS and actual account login remain unverified. No user credential is needed for the public discovery request.
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

Authoritative kernel reproduction: `cargo run --release --locked -p sculith-server --example authority_bench`.
One run measured 10,000 accepted movement operations in **750.358 µs** and 1,000 accepted placement/break pairs in **388.134 µs** (2,000 region revisions).
This uses a synthetic monotonic clock and excludes sockets, rendering, physics ticks and persistence. It is a kernel baseline, not a gameplay load or TPS measurement.

## Known Limitations

- Default mode remains discovery-only. Native spawn is an explicit loopback development preview until online and official-client checks pass.
- Online authentication has not been end-to-end verified. Offline test identities are restricted to loopback binds.
- Preview gameplay is limited to validated movement and creative building with fixed Stone/Dirt/Grass starter slots. General inventory transactions, gravity, simulation ticks, survival, combat and crafting are not implemented.
- Region persistence runs only on orderly shutdown; periodic/crash recovery saving and persistent player state are pending.
- Other players/entities are not displayed, and Java cannot yet enter the world. No complete crossplay claim is made.
- NetherNet has not been integrated or tested. RakNet results do not imply NetherNet support.
- Only a single region owner is implemented; no parallel region tick scheduler, migration, merging or splitting exists yet.
- Plugins currently expose lifecycle/MOTD through trusted executable processes. WASI Component execution, game APIs,
  commands, permissions, region/entity scheduling and isolation limits are not implemented.
- Conversion remains explicit static PNG overrides. Sound/model/UI conversion, content IR, Companion and pack delivery are pending.
- Latest protocol types expose camera instruction, preset, aim-assist and spline packets, but Sculith has no camera API or scenario tests yet.
- REA/disassembler tools were searched in the actual connected-tool catalog and were unavailable. No REA commands or results are fabricated.

## Next Action

Implement Java login/registry/chunk mapping against the same region owner, and add peer entity visibility, while advancing actual-client validation.
Continue online authentication and actual-client verification beyond the now-accessible discovery endpoint. Required gameplay/content features are not removed from the objective.
