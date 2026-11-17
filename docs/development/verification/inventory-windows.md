# Native player inventory windows — 2026-11-17 KST

Source: `1d8b799e3a404c609dfbc5c889967048da6cb0a1`.

## Behavior and reference

An initialized Bedrock client can open its player inventory with a self-targeted
`Interact(OpenInventory)` request. Sculith replies once with `ContainerOpen`,
inventory type -1, actor -1 and the player's block position. A duplicate open
does not send another open. Matching client closes receive one non-forced
inventory acknowledgement. Wrong actors, stale window IDs, unrelated container
types and client claims of a server-initiated close cannot change the window.
IDs cycle through 2–100 with only one open window per connection.

This is connection metadata: closing preserves owner slots, cursor, creative
output, stack IDs, selection and region revision. It does not add an item grant,
command, container block, custom UI or plugin default. The activation result now
refreshes the connection's cached player snapshot, so its first play request can
open a window before any movement or inventory operation.

REA 6.9.0 captured official BDS 1.26.63.1 on loopback with a fresh private world.
Binary provenance and the earlier cursor observations are retained in
[inventory batches](inventory-batches.md). The pinned independent gophertunnel
codec is `e2251a60f71d`; no BDS implementation or assets are copied into Sculith.
The completed reference observes:

- Self-targeted open: window 2, type byte 255, actor -1, position `(0,-59,0)`.
- A duplicate open followed by a valid stack request still produces only the
  original open response throughout the sequence.
- Close/reopen retains Grass35/ID2 in the ordinary slot and Grass1/ID5 on the
  cursor. Reopen uses window 3; merging the original cursor ID5 succeeds and
  restores Grass36/ID2.
- BDS sends close responses with type bytes 255 and 247, both non-forced. The
  pinned codec names 247 `None`. Sculith emits one matching Inventory close;
  whether the additional BDS response is required by some GDK screen context
  remains unverified.

Reference evidence:
`ev_2de1a0fc73c425637f5f1a1792fbf79114838e873bf6de533a5ca1b161eeaffa`.
Raw capture outside the product:
`/tmp/sculith-native-inventory-reference/cursor/window-capture-run2.json`,
SHA-256 `923e99b88e6a8edc004bda55c97abdd819460bea072c215ec20346eb1465632a`.
The native response log SHA-256 is
`8aabe48bc7b8621f7dab294d95ae860fee43a872cc878e95112a82f248841a02`.
The first reference attempt waited for a client-origin latency reply that BDS
did not provide; the completed run instead uses a valid stack response to
observe ordering. REA reports normal exit and owned-process cleanup.

## Verification and bounds

Local validation passes 306 project tests, five ignored manual checks, ownership
doctests, Clippy, architecture checks and debug/release builds. Unit checks cover
actor/initialization guards, duplicate and stale notifications, container types,
server-close claims, negative-position flooring, ID reuse and budget refill.

Real RakNet and NetherNet tests open as the first play request, interleave
duplicates and invalid closes with ordered controls, then close/reopen while
the owner holds both a cursor stack and creative output. The complete owner
snapshot and world revision remain unchanged.

The independent Go NetherNet probe passes against exact-source debug and release
binaries. It closes/reopens during a 36-slot split and subsequently merges using
the retained cursor ID. Its ordered control fence verifies the exact number of
window replies. Existing Java/native inventory interactions, counted peer hands
and live Rust inventory/equipment hooks pass; Rust packet traces now classify
native open and close requests as Control. Unimplemented Interact actions stay
Unsupported. Packet registration counts and SDK version are unchanged.

The state is **32 bytes** on this Linux x86_64 target with pinned Rust 1.100.0;
the unit bound is at most 64 bytes. It holds no heap history or pending-window
queue. Opens have a 16-request burst and refill at eight per second. Duplicate
or invalid notifications allocate no reply, and a valid close remains usable
after the open budget is exhausted. At most one close reply follows each
accepted open. Existing owner-inventory budgets are unchanged. These are bounded
layout/work checks; no server TPS, process-RAM reduction or speedup is claimed.

```sh
. ./scripts/dev-env.sh
cargo test --locked -p sculith-server inventory_window
cargo test --locked -p sculith-server response_budget_keeps_close_usable -- --nocapture
python3 tests/interop/inventory_smoke.py --client target/gophertunnel-probe
```

The official Windows GDK's actual screen rendering/input is still unverified.
General block/entity containers, Java drag distribution, crafting, dropped-item
entities and persistent player inventories remain pending. The native screen
state does not gate ordinary owner-validated stack requests, which can also
originate outside a player-inventory screen.
