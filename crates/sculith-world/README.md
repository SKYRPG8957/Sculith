# sculith-world

Protocol-independent flat chunks and an exclusively mutable region owner, with
bounded region snapshot storage. This crate has no unsafe code. It does not yet
implement physics, lighting, collision, entities, a region manager, complete world
storage management, or parallel region ticking.

```rust
use sculith_world::{BlockEdit, BlockKind, BlockPos, ChunkPos, Region};

let mut owner = Region::flat_origin();
let before = owner.snapshot(ChunkPos { x: -1, z: 0 })?;
let changes = owner.commit(owner.revision(), &[BlockEdit {
    position: BlockPos { x: -1, y: 4, z: 0 },
    block: BlockKind::Stone,
}])?;
let after = owner.snapshot(ChunkPos { x: -1, z: 0 })?;
assert_eq!(before.get(15, 4, 0)?, BlockKind::Air);
assert_eq!(after.get(15, 4, 0)?, BlockKind::Stone);
assert_eq!(changes.revision, 1);
```

- `FlatGenerator::snapshot(ChunkPos)` generates an immutable chunk without a
  mutable owner. Default terrain has bedrock at Y=0, dirt at Y=1..2, grass at Y=3,
  and air elsewhere. Its world spawn is the feet position `(0.5, 4.0, 0.5)`;
  edition-specific eye offsets belong to the protocol adapter.
- Vertical block bounds are -64..319 and section coordinates -4..19.
  `snapshot.sections()` traverses these 24 sections in ascending Y.
- `Section::get(x,y,z)` uses local coordinates 0..15. `Section::blocks()` is
  **X-fast, Z, Y** (`x + 16*z + 256*y`). A Bedrock encoder using X/Z/Y-fast ordering
  must perform its own coordinate traversal; this crate's packed bytes are not a
  Minecraft packet format.
- `ChunkSnapshot` is an O(1) Arc clone. Owner commits copy the chunk metadata and
  edited section only when snapshots still share them. Unchanged sections remain
  shared.
- `Region` is `Send`, not `Sync` or `Clone`. Mutations require `&mut Region`.
  `RegionBounds` assigns an explicit half-open rectangle of at most 64 chunks.
  A future region manager must ensure distinct owners do not logically overlap.
  `flat_origin()` covers chunk coordinates -4..3 on both axes.
- Commits use an expected region revision and at most 4096 unique positions.
  Invalid ownership, coordinates, duplicates or revisions reject the whole batch.
  Effective changes retain input order; no-op batches do not allocate chunks or
  advance revisions. Region revisions advance once per effective batch and each
  touched chunk's revision advances once.
- Sections use a uniform value or at most five palette entries in 1..3-bit packed
  storage. A section that becomes uniform again releases its packed storage.
  `Section::encode/decode` is a bounded internal storage record (2..1544 bytes),
  not a complete world storage format. Its IDs are internal `BlockKind` IDs.

## Region snapshot storage

`Region::capture()` returns immutable `RegionSnapshot` metadata and Arc-backed
loaded chunks. It does no encoding or file I/O. Pass that `Send + Sync` capture to
a blocking I/O worker and call `storage::save_snapshot(path, &capture)` there.
`storage::load(path)` verifies and restores an independently mutable `Region`.
`storage::save(path, &region)` is a convenience API; server owners should use the
capture form so they do not perform I/O during game processing.

The binary record includes file, internal-block-registry and flat-generator
versions, region ID/bounds/revision, loaded chunk coordinates/revisions and all 24
section records. It is limited to 64 chunks and 2,375,732 bytes and ends with a
CRC32 checksum. Bad sizes, versions, checksums, coordinates, duplicates, section
data, revisions and trailing bytes are rejected. CRC32 is an accidental-damage
check, not an authenticity guarantee. Modified chunks are currently never evicted;
the format validates revisions under that invariant.

Saves refuse foreign/corrupt files, directories, symlinks, read-only snapshots,
different regions and stale or conflicting captures. A persistent format-marked
`.sculith-snapshot.lock` OS lock serializes cooperating writers. New files use
no-clobber persistence; updates use a same-directory tempfile, file sync and atomic
replacement. On Unix the parent directory is synced. An old recovery copy is kept
if directory sync fails after replacement, and the error reports that replacement
already committed. Pre-commit failures leave the old destination intact.

Use a server-managed directory: standard portable filesystem APIs cannot perform
a conditional replacement against hostile processes that ignore the lock and
have the same filesystem rights. Windows file/rename support is implemented but
not tested here; directory power-loss durability is not claimed on Windows.

Verification and a reproducible microbenchmark:

```sh
cargo test -p sculith-world
cargo clippy -p sculith-world --all-targets -- -D warnings
cargo run -p sculith-world --example world_bench --release -- 10000
```

The benchmark measures this crate's snapshots, edits and region storage only. Its encoded
section byte count excludes object/Arc overhead, metadata and network encoding;
it is not an RSS result or a comparison with complete Minecraft servers.
