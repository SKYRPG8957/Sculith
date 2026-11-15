# Third-party components

Sculith's existing source license remains MIT (`LICENSE`). No project license was changed.

The following upstream libraries are used. The NetherNet core has a small, reproducible local interoperability patch; the other entries remain Git dependencies:

| Project | Locked source | License |
| --- | --- | --- |
| bedrock-rs | [5221fcae8cc8468f523f3f58f129cb308e9d3369](https://github.com/bedrock-crustaceans/bedrock-rs/tree/5221fcae8cc8468f523f3f58f129cb308e9d3369) | Apache-2.0 |
| RakNet | [9bd968ed3b72092f0817c29c6d065e026d9b2f1b](https://github.com/bedrock-crustaceans/raknet/tree/9bd968ed3b72092f0817c29c6d065e026d9b2f1b) | Apache-2.0 |
| nbtx | [9bcb986cc7f17d9904c760947ca3491243b8e0b6](https://github.com/bedrock-crustaceans/nbtx/tree/9bcb986cc7f17d9904c760947ca3491243b8e0b6) | Apache-2.0 |
| NetherNet (vendored sans-IO core; Git Tokio wrapper for test clients only) | [ed552b0b60dc6f8d54e0d7d42a9c66a9b9bdfb99](https://github.com/bedrock-crustaceans/nethernet/tree/ed552b0b60dc6f8d54e0d7d42a9c66a9b9bdfb99), [local changes](vendor/nethernet/SCULITH_PATCHES.md) | Apache-2.0 |

The Apache license is included in `LICENSES/Apache-2.0.txt`. These libraries are credited to their respective contributors.
`Cargo.lock` records the complete Rust dependency graph. Binary distribution must also retain the applicable notices for the other transitive packages.

The MIT-licensed [Dragonfly](https://github.com/df-mc/dragonfly/tree/0f9a3a5736267db60bf46e92d408f81be1de9a64)
was consulted for documented network palette, canonical block hash, chunk encoding and starter item ID facts.
Sculith's chunk encoder independently implements those protocol facts. Only the current five internal block states are mapped;
the original Minecraft block palette or game assets are not bundled.

The MIT-licensed [gophertunnel](https://github.com/Sandertv/gophertunnel/tree/e2251a60f71d5faa9784d6a4bc817c117cb6af78)
was consulted for the meanings of ability bits in `minecraft/protocol/ability.go`.
Sculith independently encodes a limited creative-builder permission set; operator, teleport and noclip permissions are not granted.

Pumpkin, Folia and other reference projects are architectural references, not linked or copied server implementations.
The example pack contains an original generated checkerboard PNG.

Windows preview packages include `THIRD_PARTY_LICENSES.txt`, generated from the locked dependency graph and its source license files.
For dual MIT/Apache-2.0 packages whose crate tarballs omit license texts (including legacy `MIT/Apache-2.0` declarations), the distribution selects their declared Apache-2.0 option and retains its full text.
The `mutants` 0.0.3 crate's omitted MIT text is retained in `LICENSES/package-fallbacks/cargo-mutants-MIT.txt` from
[cargo-mutants commit 9685de346012afb9c379432eaf9c6960e22185df](https://github.com/sourcefrog/cargo-mutants/blob/9685de346012afb9c379432eaf9c6960e22185df/LICENSE).

The development-only `tests/interop/gophertunnel` client links the pinned MIT-licensed gophertunnel (e2251a60f71d5faa9784d6a4bc817c117cb6af78) and Go/Pion stack through its own `go.mod`/`go.sum`. Its executable and Go runtime are not included in the Windows server ZIP. The MIT-licensed [go-nethernet 4c06b550](https://github.com/df-mc/go-nethernet/tree/4c06b550759d8b488ee209eb09ae87cefd6fe849) was consulted for DTLS-only Bedrock transport behavior and signed-key binding.

That development client also links the pinned MIT Dragonfly above to decode chunks
and resolve block hashes with its full vanilla registry. Its embedded registry is
only in the test client. Sculith's Rust server independently encodes the plains
biome parameters and loading controls checked against these versioned references;
no reference server implementation or full Minecraft palette is shipped in the server ZIP.
