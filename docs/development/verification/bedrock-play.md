# Native play and session load — 2026-11-17 KST

The reported GDK connection sent 67 packets in one batch. The old session limit
of 64 rejected that batch after the codec had already decoded it. A real RakNet
regression reproduces closure before the fix. Sessions now use the existing
4 MiB decompression/4,096-frame codec bounds and yield while draining large
batches. Tests retain the last packet of 67/129/4,096-packet batches, verify a
subsequent block commit and reject 4,097 frames. Compression/encryption transition
and sub-client checks remain in place.

Visible ground faces were also rejected by rays aimed only at buried block
centres. Reach still uses the existing six-block check; visibility now also tests
the facing surfaces. Pick, place and break tests cover exposed distant ground.
The existing tall-wall occlusion, collision, permission, rate and world-bound
rejections still pass. Empty-hand solid-block clicks and air use are validated
no-ops for the four current block items, including the native no-face sentinel.
An independent transaction reproduces the former empty-hand disconnect.

Native peer presentation keeps at most 63 pending poses and emits the latest
pose per peer at 20 Hz. Equipment/lifecycle records flush preceding poses in
order. Small records share at most 64 packets per batch; skin entries remain
separate. Session block/chat/private queues exclude duplicate presence traffic;
the full observer and game-hook contracts remain available.

Both editions recover a lagged peer receiver from a new atomic owner snapshot
and continuation. Reconciliation removes departed actors/roster entries before
adding replacements and refreshes current equipment and positions. The snapshot
carries the last accepted movement sequence. Old pending native poses are
discarded on recovery. Queues remain bounded at 128 events; lost block/private
updates and closed streams still fail explicitly. A deterministic owner test
overflows an old receiver, changes membership/equipment, recovers the current
pose and verifies the next event on the new continuation.

Initial and newly visible chunks now start at the viewer's centre and expand
outward. Debug timings separate owner snapshot, encoding and transport awaits.
This remains an 8×8-chunk flat region, with a 25-chunk initial view; it is not
general terrain generation or unrestricted world travel.

## Validation

322 project tests, six ignored manual measurements, Clippy with warnings denied,
ownership doctests, architecture checks and release builds pass. Independent
Java/native membership, inventory, plugin grant/query and block-pick socket
checks pass. Dragonfly independently decodes every voxel and biome in all 25
initial chunks. The static-CRT MSVC executable builds, imports only Windows
system DLLs and runs `--version` under Wine. Native Windows CI is recorded
separately once complete; these checks do not establish official GDK gameplay.

## Local load

The pinned Go/Pion client negotiates real NetherNet connections, receives 25
chunks, sends a 67-packet burst, empty-hand/air-use transactions, movement at a
requested 20 Hz and latency fences through its final input. Each client breaks
one distinct visible ground block. The runner checks that every sent movement
reaches the owner. Separate loopback source addresses retain the four-sessions-
per-IP admission limit; at most eight joins negotiate concurrently.

Five logical CPUs on Linux 6.18.57/glibc 2.41; client and server share the host.
No plugins, packs or persistence. Runs are serial, without compilation or other
game servers during timing. These are 30-second flat-world checks, not a soak,
official-client rendering test, TPS measurement or production player capacity.

| Run | Clients | Owner movements | Session failures | Control RTT p95 | Process high-water RSS |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 64 | 38,198 | 0 | 111.954 ms | 33,026,048 B |
| 2 | 64 | 38,258 | 0 | 134.999 ms | 33,132,544 B |
| 3 | 64 | 38,365 | 0 | 71.339 ms | 33,136,640 B |

All 192 joins, burst tails and block commits pass; every sent movement reaches
the owner and no movement corrections are observed. The second run logs 32
snapshot recoveries without losing sessions. Latency remains material: the
worst control RTT is 362.015 ms. The preceding candidate, with the same probe,
lost seven sessions to peer backlog; a single earlier successful run did not
establish repeatability. No throughput or CPU/RAM speedup ratio is claimed.

The 25-chunk receive span has p95 5.645–10.576 ms across these runs. Server-side
per-session phase p95 stays within 1.302 ms for snapshot awaits, 0.800 ms for
encoding and 2.280 ms for transport awaits. These wall times do not measure GDK
rendering or unimplemented natural-terrain generation.

[Machine-readable timings, conditions and raw-report hashes](bedrock-play-load.json).
Raw reports, bounded owner profiles and logs remain under
`/tmp/sculith-native-recovery-64-{1,2,3}` in this workspace. Reproduce with:

```sh
python3 scripts/bench/native_sessions.py --server target/release/sculith \
  --client target/gophertunnel-probe --clients 64 --seconds 30 \
  --output /tmp/sculith-native-load
```

## Reference scope

Pumpkin source was inspected at
[`87f8822ac36da62102350977be2f62f155a303dc`](https://github.com/Pumpkin-MC/Pumpkin/tree/87f8822ac36da62102350977be2f62f155a303dc),
particularly `chunk_sender.rs` and its Bedrock input/inventory handlers.
Centre-first scheduling, delivery accounting and bounded work are relevant
design comparisons. Pumpkin is GPL-3.0; no implementation or assets were copied
into this MIT repository. Packet facts were checked against pinned MIT
gophertunnel `e2251a60f71d` and Dragonfly `0f9a3a573626`.

REA 6.9.0 captured controlled BDS 1.26.63.1 startup and normal shutdown, but the
new ground probe timed out during NetherNet negotiation. It supplies no positive
ground-interaction or GDK evidence. Previously completed BDS inventory references
remain scoped to their own records. Further official-client verification,
mixed-edition load, longer runs and terrain/physics/entity work remain open.
