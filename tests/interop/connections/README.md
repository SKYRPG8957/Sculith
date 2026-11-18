# Native event fixture

This native Rust plugin observes Java handshake, status, login and configuration
packets, and Bedrock network-settings and login packets. It replaces the Java
status response using cancellation and an extra packet. The native client probe
uses the current wire schema with a deliberately changed protocol proposal; the
plugin rewrites that proposal before admission. This is not an older-client
compatibility test.

Build the fixture and server with the same Rust toolchain and source revision:

```sh
cargo build --locked -p sculith-interop-connections -p pumpkin
```

Pass the resulting shared library to the resource harness:

```sh
python3 tests/interop/shared_resources.py /path/to/sculith /path/to/bedrock-probe \
  --world /path/to/stopped-flat-world \
  --connections-plugin target/debug/libsculith_interop_connections.so
```

On Windows the library is `sculith_interop_connections.dll`; on macOS it is
`libsculith_interop_connections.dylib`. The harness copies it into its scratch
server and allows that unsigned fixture explicitly. It is never installed by
the default server. For play-phase mutation checks, also pass `--packet-edits`
and the optimized Wasm fixture through `--plugin`.

`--pick-events` uses this fixture to cancel automatic slot changes from block
and mob picking on both editions. Dedicated `Hold*` clients test reused stacks,
new blocks and spawn eggs. `Mutate*` clients change slot 0 inside the callback;
the server must preserve that change and reject the stale pick plan. The fixture
reads the inventory during callbacks to catch locks held across event dispatch.
These callbacks apply only to the named test clients.

`--tracking-events` also removes a nearby pig from its outgoing spawn callback,
then removes another pig from its velocity callback. Both packets are cancelled.
The client checks the despawn and a subsequent latency reply; the fixture logs
confirm that each removal finished. These cases catch entity-map and viewer-list
locks retained across plugin packet events. The callbacks only affect
`TrackerNative` and `TrackerDataNative`.

`--creative-events` cancels creative changes on both editions and reads the
player inventory and screen during each callback. The native request includes
a slot swap before creation, with a later slot event cancelling the whole
request. Cursor creation and world drops are also cancelled. `CreateMut*`
clients alter the destination in the callback; that stack must survive instead
of being overwritten. The Java client requires an explicit slot correction
before its following ping response. These creation callbacks affect only
`Create*` test clients.

Destruction cases use `EraseDeny*`, `EraseMut*` and `EraseAllow*` clients.
They veto, mutate or allow a creative slot clear. Native cases also check a
partial reduction, repeated reductions of one slot, a preceding move and cursor
destruction. Event payloads describe the final proposed stack, including air
with count zero for a clear. Callbacks read the inventory and player screen.
Native destruction follows click and interaction events; Java direct creative
slot assignment keeps its existing creative-only event path.

`--inventory-events` reads the inventory and player screen from
`InventoryInteractEvent` before a native swap. `InvDenyNative` cancels it;
`InvMutNative` writes a grass stack which must survive rejection of the stale
plan. `DropDenyNative` cancels `PlayerDropItemEvent` after a planned swap without
losing inventory or spawning a drop. `InvDenyJava` checks the existing Java
interaction veto and correction. These callbacks only affect the named clients.

`--legacy-events` cancels a verified legacy drop for `LegacyDropDeny` and an
implicit hotbar change for `LegacyHeldNative`. The inventory and player screen
remain readable in the drop callback. `LegacyTxnNative` logs its allowed hotbar
changes while running the ordinary legacy transaction checks.

`--click-events` reads inventory and screen state in `InventoryClickEvent`.
`ClickDenyNative` and `ClickDenyJava` cancel before the general interaction event;
`ClickMutNative` replaces an inventory stack with grass. `ClickCurNative` permits
moving onto the cursor, cancels its outside drop and permits returning it. Logs
check the original clicked item and cursor, screen indices and gesture metadata.
`LegacyTxnNative` observes no click while making its non-UI transactions.

`--item-use-events` observes right-click-air for `Use*` and `Eat*` test clients.
`*Deny*` cancels use, `*Mut*` replaces the held item with grass, and `*Allow*`
provides the normal behavior control. Logs capture the original hand and screen
counts, empty head slot and inactive use timer. Callbacks must run before helmet
equipment or food consumption starts, without retaining inventory locks.
