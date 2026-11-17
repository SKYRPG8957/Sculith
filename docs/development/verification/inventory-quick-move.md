# Player inventory interactions — 2026-11-17 KST

The source-specific results below retain their original ID-allocation behavior.
Current source preserves IDs on moves and count changes instead; see
[inventory batches](inventory-batches.md) for the BDS reference and updated checks.

## Main/hotbar quick moves

Source: `0fc76d34b3642d415b3c9f785e5129ac18280c79`.

Protocol-777 `QUICK_MOVE` requests with either mouse button now move between
player main inventory and hotbar. The shared owner merges matching stacks in
ascending slot order before using an empty slot. A partial move leaves the
remainder in its source; a full destination is a no-op. Occupied changed slots
receive fresh IDs. Exhaustion rolls back every merge and ID allocation. Cursor,
created output, selected slot and request history are unchanged.

Menu ranges and direction were checked against the official Java 26.3
`InventoryMenu.quickMoveStack` bytecode with `javap`. The published server tool
JAR has SHA-1 `33680f5f2ac32864d6d7cf5e56a705fdb3e05f4c`. No Minecraft
implementation or game asset is copied into the engine. Crafting, armour,
offhand and other containers remain outside this operation.

## Protocol and owner checks

Local validation passes 290 project tests, ownership doctests, Clippy,
architecture checks and debug/release builds. Tests cover both directions,
merge-before-empty order, partial/full destinations, empty sources, invalid
slots, authorization, allocator rollback and retained cursor/output/history.

The extended independent Java/native inventory probe submits stale state and
forged predictions while verifying exact counts. The native peer observes the
Java hand change through Grass ×1, Dirt ×64, empty, Dirt ×34 and Dirt ×64. Existing Rust
inventory/equipment hooks receive committed changes; no-op moves publish none.
The region revision is unchanged. Debug and release mixed-edition probes pass.

Complete [Linux](https://github.com/SKYRPG8957/Sculith/actions/runs/44019126675)
and [native Windows/static-CRT](https://github.com/SKYRPG8957/Sculith/actions/runs/44019126486)
workflows pass at this source. Neither run uploads a build artifact.

Java block-pick ID 36 and native ID 34 now report `Gameplay` rather than
`Unsupported` in packet observations. Unit checks and live Rust plugin trace
assertions cover both classifications. Registration counts and SDK version are
unchanged; this does not add gameplay handlers for unrelated registered IDs.

```sh
cargo build --workspace --locked
python3 tests/interop/inventory_smoke.py --client target/gophertunnel-probe
python3 tests/interop/block_pick_smoke.py --client target/gophertunnel-probe
```

## Stock Java screen

A fresh unmodified Java 26.3 profile joins the exact-source debug server with
no plugins or resource packs. Actual mouse input selects Grass1 from the vanilla
catalogue and Shift-clicks it from hotbar slot 0 to main slot 9. The displayed
hand becomes empty and Grass appears in the main inventory.

This creative screen sends three `SetCreativeModeSlot` requests, confirmed by
the world profile, rather than `QUICK_MOVE`. It verifies the ordinary creative
screen regression; the independent wire probe above exercises the new owner
quick-move path. A separate observer helper stopped on an uninitialized local
`peer_entities` attribute before submitting its return-click action. No return
movement or independent peer assertion is claimed for that graphical attempt.

- Client JAR SHA-1: `e877b6a07acd633fb3bb475002175cec036e7b87`.
- Server SHA-256: `a58855ac76d2e752e23c34df66be19c6918d2a3d9a273d374b9ed4fd8c0b821d`.
- Before image SHA-256: `2425654c677fae47041d25828dd2f8f43b55c14188607e58f71cdbc3ff91da56`.
- Moved image SHA-256: `8a934c63e3b4256405ded9dddb05476f4b4cafe0bc0921739a3de09ead136917`.

Logs, profiles and unedited captures remain outside the repository under
`/tmp/sculith-quick-move-stock-java`. All three owned display/server/client
processes have stopped. This is offline Linux evidence; no positive official
account or official Windows GDK gameplay verification is added. No performance
claim, release or package accompanies this change.

## Cursor collection

Source: `dd5bdb3dcee06ed9166e991b973424965438f685`.

Protocol-777 `PICKUP_ALL` (mode 6) now collects matching items into the owner's
cursor. The clicked player-inventory slot must currently be empty. Partial
stacks precede full stacks; forward order traverses main inventory then hotbar,
and the alternate button reverses that order. Collection stops at 64 items.
Created output and cursor are excluded as sources. Empty/full cursors, occupied
clicked slots and inventories without matching items do not change items or IDs.

These rules were checked against `AbstractContainerMenu.doClick` for
`ContainerInput.PICKUP_ALL` in the same pinned Java 26.3 server JAR. The Rust
implementation operates only on the existing supported player inventory and
four block-item kinds; no container, crafting, drop or drag support is implied.

Local validation passes 296 project tests, ownership doctests, Clippy,
architecture checks and debug/release builds. Unit checks cover both scan
directions, partial-before-full priority, matching-output exclusion, whole-grid
coverage, conservation, authorization and complete allocator rollback. A
whole-grid candidate that drains 35 sources must also roll back if allocation
of its final cursor ID fails. History, created output and selection are preserved.
No-op collection emits no semantic inventory/equipment event; raw receipts
remain observable. The region revision does not change.

The existing independent inventory probe now sends collection with forged
predictions and a stale state number, checks exact cursor/source counts, and
checks reverse collection and a full-cursor no-op. The native peer observes
Java's selected Dirt stack fall from 64 to 34 after a partial stack is consumed
first. The same script runs against debug and release executables and is part
of both existing CI workflows. The earlier graphical capture above uses the
creative screen's slot-replacement path; it is not proof of a new graphical
PICKUP_ALL attempt. No new pack, plugin or client mod is introduced.

The complete [Linux workflow](https://github.com/SKYRPG8957/Sculith/actions/runs/44023814632)
and [native Windows/static-CRT workflow](https://github.com/SKYRPG8957/Sculith/actions/runs/44023814547)
pass at this source. Both contain zero uploaded build artifacts; no release or
package is published.
