# Connection and inventory event fixture

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
