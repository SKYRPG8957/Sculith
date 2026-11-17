# Creative block picking

Implementation source: `5fac2b0668353a37a0f646810b5044e0c8ac3392`.
Java 26.3 / 777 and Bedrock 1.26.63 / 2225 use the same owner operation.
Only the current core block registry is supported; entity picking, arbitrary
block-entity data and general survival inventory behavior remain outside this change.
No plugin, resource pack or gameplay command is needed to pick a block.

## Behavior and validation

Picking first reuses an existing inventory stack, preserving its count. A matching
hotbar stack selects that slot; a matching main-inventory stack swaps with the
selected slot. If no match exists, creative mode creates one item in an empty
hotbar slot. With a full hotbar, the displaced item moves to an empty main slot
when possible. A completely full creative inventory replaces only the held slot.
Cursor, created output and request history remain separate.

The owner checks initialization, build permission, inventory rate, region bounds,
reach and occlusion. Selection comes from owner state; a native packet's legacy
slot/count hint cannot select arbitrary inventory storage. Ordinary rejected picks
resynchronize inventory instead of disconnecting. Native responses include the
pick UI hint and explicit hotbar selection; Java sends its counted inventory and
selected-slot correction. Current blocks have no block-entity NBT to copy.

Moved occupied stacks receive fresh positive IDs in one candidate transaction.
Allocator exhaustion rolls back all changes. Repeated picks reuse existing stacks
and do not duplicate committed inventory/equipment events. Picking does not change
world revisions. Existing raw packet observations cover the two request IDs.

## Versioned behavior references

REA CLI **6.9.0**, process provider v3, ran an isolated official BDS **1.26.63.1**
reference with explicit offline loopback settings. A separate pinned gophertunnel
client decoded 73 inventory/selection observations. Console commands prepared the
blocks and inventory, and the reference process stopped through its console. This is controlled
process and wire observation, not decompiled implementation or official GDK UI verification.

- Binary SHA-256: `dc9b3c566a014cf53930eb1b54c6b599a99a81cc32c68aba06a5d73a0fd63179`
- REA evidence: `ev_1a5fd82bfb39b2a422e2639339ce54ef1d0ebf3394457aadce32e09d40ee0de5`
- Capture SHA-256: `50bea7ae7e0d78fec1fa79dab937d30b12dc0e1065c1794c3f63f36fb51f5ec4`
- Decoded observation log SHA-256: `6c382303be856be19084eb8e3bc075cec1423e27c4ee3156f53f903bc524350b`

Observed new Grass/Dirt/Stone picks create count 1. Re-picking Grass selects its
existing slot. A count-7 Grass stack in main inventory swaps with the held count-1
Stone stack; a later new Dirt pick preserves that Grass stack in an empty main
slot. Raw hint values 0 and 9 do not override the authoritative selection.
`GUIDataPickItem`, main inventory and selecting `PlayerHotBar` follow that order.
The failed first launch had an executable file-mode error; a private executable
copy corrected it without modifying the original reference file.

Protocol layouts are independently encoded with pinned bedrock-rs `5221fcae` and
decoded with gophertunnel `e2251a60`. The MIT Dragonfly `0f9a3a57` block-pick handler
and player behavior were read as an additional behavior reference. No official
binary, asset or implementation code is included in Sculith.

## Checks

281 project tests, ownership doctests, Clippy, architecture checks and debug/release
builds pass. New domain cases cover creation/reuse, full inventory, displaced
counts, cursor/output/history preservation, allocator rollback, initialization,
permissions, reach, occlusion, rate limits and retirement. The owner test checks
post-commit inventory/equipment delivery, no duplicate events and no world edits.

`tests/interop/block_pick_smoke.py` uses independent Java and native codecs on real
sockets, with no starter inventory. It checks count-1 creation, count-preserving
hotbar/main reuse, nonzero selection, canonical IDs, rejected targets/hints,
packet observations and real Rust inventory/equipment callbacks. It passes against
debug and release servers. Existing inventory, plugin inventory, Java gameplay
and private console command probes pass.

An exact-source unmodified Java 26.3 client, fresh profile and **no server plugins,
resource packs or client mods**, joins by address and uses actual mouse clicks to
pick Grass and place it. The world profile records one `pick_block` and one
`place`; manual saving writes revision 1. The rendered block and held Grass are
visible, and all owned client/server/Xvfb processes stop. This is offline Linux
loopback, not positive Microsoft authentication or Windows GDK rendering.

- Server SHA-256: `cc83b5f8c4c00e176f5dfe84035b3ac07082e59211092d2e3a04f5f29b3c2961`
- Placed screenshot SHA-256: `392d6c0e0bce40b5c18a628e8a9498f7fd6b75c68c74af6634aa7952f215b988`

Timing, allocation counts and scaling are not benchmarked for this new behavior.
Queues, slot/action bounds and the existing single-owner model remain in place.
No package, release, tag or executable artifact is published.

Complete workflows pass for the implementation source:
[Linux](https://github.com/SKYRPG8957/Sculith/actions/runs/44010428132) and
[native Windows static CRT](https://github.com/SKYRPG8957/Sculith/actions/runs/44010427981).
