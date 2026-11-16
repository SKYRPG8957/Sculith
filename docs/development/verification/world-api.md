# Rust world operations

Runtime source `4bc54bb9d58d1e02d0473b1f0f988036b16b6096` introduces API v10 world operations.
Local checks pass 231 project tests, Clippy, ownership doctests and the architecture
check. Existing inventory, packet hooks, forms, sounds, models, Java/native
connections and configured pack-proxy checks pass.

[Linux run 43947207268](https://github.com/SKYRPG8957/Sculith/actions/runs/43947207268)
completes successfully at `0f20d6f`. The matching native
[Windows run 43947207304](https://github.com/SKYRPG8957/Sculith/actions/runs/43947207304)
passes every configured build, runtime and interoperability step, including the
release server and Rust world fixture. At this observation only cache/checkout
cleanup remains. The previous Windows trial used the wrong fixture directory;
`0f20d6f` corrects the check to use `target/release`. No runtime change was needed.
These workflows upload no packages or build artifacts.

A separate local release workspace build also passes the world API probe using
the release server and release Rust fixture.

## Independent clients

A real Rust fixture uses the public SDK to discover the region before player
requests, then edits two chunks. Java and a separate Go/Pion native peer observe
the same two build deltas, two removal deltas and one subsequent player edit.
Their shared region revision progresses from 0 through 3. A further plugin edit
advances it to 4; orderly restart restores all three resulting blocks and that
revision. The Java decoder verifies the restored voxels, heightmaps and vertical
skylight across both edited chunks.

Stale revisions, invalid bounds, duplicate positions, unsupported blocks and
unchanged edits produce no public block changes. Separate observer processes
verify that private responses stay with their requesting plugin, plugin edits
carry the plugin's identity, and the legacy callback still receives only the
player edit. A worker that exceeds its work quota is disabled while TCP status
and the private-response guard remain operational.

These fixtures live under `tests/fixtures` and `tests/interop`. They add no engine
commands or default plugins. Reads/edits currently cover one owned region and
five block states; they do not provide general entities or multi-region routing.

## Stock Java 26.3

A fresh unmodified game profile joins by address and accepts the normal required
pack prompt. Through the game's command screen, it invokes the explicitly loaded
Rust test fixture. Its committed block appears with the shared pack's texture
beside the existing shared static model. A separate byte-level connection decodes
the edited spawn chunks and reads back region revision **1**, Stone/Dirt/Air at
the three probe positions. The graphical session receives no injected gameplay
packets and uses no client mod.

- Client JAR SHA-1: `e877b6a07acd633fb3bb475002175cec036e7b87`.
- Server SHA-256: `2a74cfc991592770be7ca6df8259b06b528cf294f724b68f619a0853a78cd11b`.
- Pack: 11,911 bytes, SHA-1 `4ec0f94603861aeceb633e2384b12d7cc3213135`.
- Before edit capture SHA-256: `882cad9565d435c6660a5c8dfdb389a9f5113fab1770920042dfcffedac0a2dd`.
- Committed block/model capture SHA-256: `8a435d6fa72d8275bdd3253f6ea98ee523598c48dfff198664f788ca5ed7f87c`.

The private captures, downloaded assets and logs remain outside the repository;
all three owned game/server/display processes are stopped. This is Linux
local/offline evidence. Successful real-account entry, official Windows GDK
rendering and Internet/NAT deployment remain separate verification work.
