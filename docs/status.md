# Development status — 2026-11-16 KST

**Current Milestone:** Milestone 1, Bedrock-native login, spawn and authoritative creative-builder preview. The overall goal remains active.
Canonical repository: `SKYRPG8957/Sculith`. Existing MIT license and origin are preserved.
The full requirements are in [engineering-directive.md](engineering-directive.md).

## Implemented

- Existing Java TCP and Bedrock UDP discovery, trusted executable Rust plugin prototype and explicit PNG conversion retained.
- Opt-in RakNet sessions using pinned Rust libraries, compression, signed identity verification, encryption and ordered empty-pack negotiation.
- Bounded session/message queues with byte budgets, login/spawn timeout and owned session cleanup.
- Local NetherNet HTTP signaling and sans-IO WebRTC transport with signed answers and persistent operator keys, sharing the existing login/session and world owner. HTTP remains loopback-only; media candidates and sources are restricted to OS-confirmed same-machine addresses. SCTP initiation is separated from DTLS role negotiation in the pinned vendored core. [Bounds and trust scope](nethernet.md).
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

- [Preview 5](https://github.com/SKYRPG8957/Sculith/releases/tag/v0.1.0-preview.5) is public. Its tag matches the exact tested source; a fresh unauthenticated HTTPS asset download passes the SHA-256 and metadata verifier. [Publication run 43760740320](https://github.com/SKYRPG8957/Sculith/actions/runs/43760740320) publishes the existing immutable artifact. The default installer and download links now select Preview 5; public Windows installation verification is pending.
- Preview 5's transport-bound DTLS login passes native Windows validation in [run 43760016108](https://github.com/SKYRPG8957/Sculith/actions/runs/43760016108), exact source `3b0113b6579e3eecd3d26c2c00027b90c94b5c3c`. Both independent clients pass against the release EXE: aiortc/OpenSSL reaches pack negotiation and gophertunnel/Go/Pion reaches world spawn and server-side activation. Workspace/vendor tests, Clippy, both executable smoke modes and extracted-launcher preservation checks pass. The ZIP is 5,226,562 bytes, SHA-256 `0c6b31a28145e41efeb3bd65b9407a8e5893e7d0bf458c910c9c54532560a91d`; exact metadata and dependency licenses were inspected. PE imports remain Windows system DLLs only. Official Minecraft world entry/rendering still awaits a successful retest.
- Preview 4's same-machine media policy and SCTP initiation correction passed native Windows checks in [run 43754939284](https://github.com/SKYRPG8957/Sculith/actions/runs/43754939284), source `7b0160a`. The release EXE also passes the independent aiortc/OpenSSL probe, both existing smoke modes, packaging and launcher checks. Its 5,205,853-byte ZIP and exact metadata/SHA-256 were independently verified; PE imports remain Windows system DLLs only.
- [Preview 4](https://github.com/SKYRPG8957/Sculith/releases/tag/v0.1.0-preview.4) is public, and a fresh public asset download passes the exact-source/checksum verifier. Windows PowerShell 5.1 installation, both launcher checks, refusal to overwrite an existing directory and the documented one-command installation pass in [run 43755787979](https://github.com/SKYRPG8957/Sculith/actions/runs/43755787979). The official retest below reached the application-encryption failure; world entry was not verified.
- Rust 1.100.0 workspace build and Clippy for all targets with warnings denied.
- Preview 3's NetherNet implementation and loopback routing correction passed native Windows Clippy, unit/doc tests, HTTP/WebRTC shared-world tests, both release-executable smoke checks, package/launcher checks and dependency-license packaging in [run 43751492841](https://github.com/SKYRPG8957/Sculith/actions/runs/43751492841), source `ed8bc8c`. Its ZIP is 5,176,423 bytes; metadata and SHA-256 were independently verified. PE imports remain Windows system DLLs only.
- [Preview 3](https://github.com/SKYRPG8957/Sculith/releases/tag/v0.1.0-preview.3) is public. Its release tag matches the exact tested source, and a fresh public asset download passed the same checksum/metadata verifier. Windows PowerShell 5.1 installation, both launcher configuration checks, existing-directory preservation and the documented one-command installation passed in [run 43752505074](https://github.com/SKYRPG8957/Sculith/actions/runs/43752505074). Official Minecraft entry remains unverified.
- Native Windows x64 build, Clippy, unit/doc tests, both release-executable network/persistence smoke checks and package validation passed for `0e782d5` in [Windows preview run 43741364940](https://github.com/SKYRPG8957/Sculith/actions/runs/43741364940).
- The extracted ZIP launcher passes configuration checks from a path with spaces and preserves edited user settings. Its x64 PE imports only Windows system DLLs; no separate VC++ runtime installation is required.
- Preview 2's diagnostics and byte-level handshake test passed native Windows validation in [run 43745002586](https://github.com/SKYRPG8957/Sculith/actions/runs/43745002586), source `077bb56`. Its public release is a diagnostic update, not a confirmed fix for the reported official-client failure.
- Preview 2's public download, PowerShell 5.1 installer, `start.cmd` and `diagnose.cmd` configuration checks, one-command install, and existing-directory preservation passed in [run 43747012301](https://github.com/SKYRPG8957/Sculith/actions/runs/43747012301).
- [Windows preview v0.1.0-preview.1](https://github.com/SKYRPG8957/Sculith/releases/tag/v0.1.0-preview.1) is public (3,858,243-byte ZIP plus SHA-256 checksums). Windows PowerShell 5.1 installation, refusal to overwrite an existing directory, and the documented one-command installation passed in [run 43742465897](https://github.com/SKYRPG8957/Sculith/actions/runs/43742465897).
- Latest Linux and Preview 5 native Windows implementation: 95 project unit/integration-style tests, the region `!Sync` compile-fail doctest and 256 vendored transport tests pass.
- Real UDP headless protocol-client tests cover compression/encryption, signed login rejection, stale protocol rejection,
  ordered packs, cancellation, spawn entity-ID validation, 25 decoded chunk packets, session lifetime and cleanup.
- Two encrypted UDP clients receive the same accepted placement/break delta. A rejected Bedrock break is corrected only for the requester; a forged held-item claim cannot commit. Valid eye-offset movement commits, while a large tick/teleport request is corrected.
- A real UDP client crossing a negative chunk boundary retains its negotiated one-chunk radius; only the three newly visible chunks are sent, with no overlap retransmission.
- An independent aiortc 1.14.0/OpenSSL client now completes ICE, DTLS, SCTP, opens both data channels, verifies the server JWT/fingerprint signatures and receives network settings and direct DTLS-protected login success with resource-pack negotiation. A separate pinned gophertunnel/Go/Pion client completes login, pack negotiation, world spawn and server-side player activation. It reproduced the interface-bound socket failure and then the SCTP initiation-role mismatch before their corrections. The probe is part of Linux and Windows CI; official gameplay is still separate.
- Independent real TCP tests verify NetherNet shared status, unsigned/tampered offer rejection, fixed/chunked body limits, signed answers carrying the persistent key, and listener cleanup. A real HTTP/WebRTC headless client and RakNet client enter the same owner and receive the same committed block change.
- An independent byte-level UDP client completes the cookie-checked OpenConnectionRequest1/2 exchange and confirms an invalid cookie is not admitted. It does not use the server's RakNet codec.
- Domain tests exercise replay/speed/collision/reach/permission/budget checks, disconnected-player queue cleanup and immutable captures. Changed runtime state survives snapshot reload.
- Every voxel of a flat chunk (98,304 blocks) independently decoded and compared; negative sections, asymmetric edits,
  1/2/3-bit packing and the latest `LevelChunkPacket` codec roundtrip verified.
- Both built-executable smoke modes passed: Java/Bedrock discovery, Rust plugin, ping, malformed traffic,
  Java login rejection, shutdown, snapshot save/restart/load, existing foreign-file protection and pack conversion; `--sessions` also confirms a real RakNet connection-request reply, NetherNet HTTP status and unsigned-offer rejection, and stable operator keys after restart.
- Snapshot storage tests cover disk roundtrips, immutable captures, checksums, truncation, version/coordinate/revision validation,
  stale concurrent saves, and preservation of foreign/corrupt/read-only/symlink destinations.
- Latest-release reference check still reports Java `26.3 / 777` and Bedrock `1.26.60 / 2225`. These are references, not full gameplay support declarations.

## Failed / externally constrained

- Preview 4 official retest reached WebRTC and network settings with client/server protocol 2225, then failed at `encryption-handshake`, `InitialConnection-66`. NetherNet was incorrectly sent the RakNet cipher handshake. The current source uses DTLS directly, binds login to the signed transport key and rejects a valid login signed with another key. Independent clients now pass this stage and the Go/Pion client reaches the world; official-client retesting is pending.
- Preview 3 official retest still failed: the user accepted the new trust/continue prompt, GET and POST reached Sculith, but Windows GDK returned `Door / InitialConnection-83` before a WebRTC-ready log. Two independently reproduced interoperability gaps are corrected in the current source; an official-client retest remains necessary.
- The cloud Chromium probe gathered no UDP candidates because of its managed `WebRtcIPHandling` policy. The policy was preserved; the independent OpenSSL/aiortc runner was used instead.
- The initial native Windows NetherNet test timed out during WebRTC setup, while HTTP status/signature/rejection tests passed. [Diagnostic run 43751174972](https://github.com/SKYRPG8957/Sculith/actions/runs/43751174972) confirmed the loopback socket failed on a LAN candidate with `os error 10051`. A regression test reproduces the former routing error; candidate selection now stays on loopback and the corrected Windows run above passes. The failed builds are not published.
- The first official-client attempt reported by the user failed: Windows GDK `1.26.63`, `Terracotta`, `InitialConnection-149`. Account sign-in was already complete and other servers connect successfully. Subsequent retests reached the more specific transport/login stages above. Successful headless tests are not treated as a substitute for official-client verification.
- The first Preview 5 Windows build failed a TLS test-fixture assertion: the client rejected the untrusted certificate, but the server's buffered write could finish before the rejection arrived. The fixture now checks that the client sends no application request through rejected TLS; the corrected native Windows run passes. That failed build was not published.
- The first Windows smoke run found console shutdown killing a plugin before its lifecycle hook. Windows workers now run without the host console; the corrected run verifies the Stopping event and final world save.
- Official discovery initially returned proxy HTTP 403 under the earlier restricted network. With the current unrestricted networking, Python's normally verified HTTPS client received HTTP 200 for discovery, the official OIDC configuration and its JWKS (three public signing keys). This is public-metadata retrieval; actual account login remains unverified.
- The former authenticated startup failed with TLS `UnknownIssuer` in the Rust HTTP client. Authentication metadata now uses OS trust with HTTPS-only requests and a 15-second bound. A fresh executable run fetched official discovery/OIDC/JWKS and reached authenticated listeners. Real-socket tests reject plaintext and a fresh untrusted certificate. Actual online account login remains unverified.
- Official Bedrock was attempted by the user and failed during initial connection. Official Java-client testing remains unrun; neither edition has verified world entry/rendering.
- The latest Linux and native Windows automated checks pass after the routing correction. The expected compile-fail result verifies ownership restrictions.

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
- NetherNet uses loopback HTTP signaling and same-machine media sources only. Offer fingerprint ownership and expiry are checked; the signaling token issuer/signature is not authenticated in this preview. The inner Bedrock login policy remains in force. Official NetherNet client entry and public token trust remain unverified.
- Both NetherNet channels now reach the validated owner. A real test commits valid unreliable movement and corrects a large tick/teleport attempt over the reliable channel. Official-client movement/rendering still require verification.
- Only a single region owner is implemented; no parallel region tick scheduler, migration, merging or splitting exists yet.
- Plugins currently expose lifecycle/MOTD through trusted executable processes. WASI Component execution, game APIs,
  commands, permissions, region/entity scheduling and isolation limits are not implemented.
- Conversion remains explicit static PNG overrides. Sound/model/UI conversion, content IR, Companion and pack delivery are pending.
- Latest protocol types expose camera instruction, preset, aim-assist and spline packets, but Sculith has no camera API or scenario tests yet.
- REA/disassembler tools were searched in the actual connected-tool catalog and were unavailable. No REA commands or results are fabricated.

## Next Action

Publish the verified Preview 5 artifact and validate its public installer, then retest the official Windows GDK client past the corrected application-encryption stage. Resume Java login/registry/chunk mapping against the same region owner and peer entity visibility after the Bedrock connection path is established.
Continue online authentication and actual-client verification beyond the now-accessible discovery endpoint. Required gameplay/content features are not removed from the objective.
