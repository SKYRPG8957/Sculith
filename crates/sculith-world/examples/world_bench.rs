//! Reproducible microbenchmarks, not a comparison with complete Minecraft servers.
use std::{hint::black_box, time::Instant};

use sculith_world::{BlockEdit, BlockKind, BlockPos, ChunkPos, FlatGenerator, Region, storage};

fn main() {
    let iterations = std::env::args()
        .nth(1)
        .map(|arg| arg.parse::<u32>().expect("iterations must be an integer"))
        .unwrap_or(10_000);
    assert!(
        (1..=1_000_000).contains(&iterations),
        "iterations must be 1..=1000000"
    );
    let generator = FlatGenerator::default();
    let snapshot = generator.snapshot(ChunkPos { x: 0, z: 0 }).unwrap();
    let serialized_bytes: usize = snapshot
        .sections()
        .map(|(_, section)| section.encode().len())
        .sum();
    println!(
        "flat chunk sections: 24; internal encoded section bytes: {serialized_bytes}; raw u8 block bytes: {}",
        24 * 4096
    );

    let start = Instant::now();
    for _ in 0..iterations {
        black_box(snapshot.clone());
    }
    println!(
        "snapshot Arc clone: {iterations} operations in {:?}",
        start.elapsed()
    );

    let start = Instant::now();
    for index in 0..iterations {
        black_box(
            generator
                .snapshot(ChunkPos {
                    x: index as i32 % 4096 - 2048,
                    z: -1,
                })
                .unwrap(),
        );
    }
    println!(
        "flat chunk snapshots: {iterations} operations in {:?}",
        start.elapsed()
    );

    let mut region = Region::flat_origin();
    let start = Instant::now();
    for index in 0..iterations {
        let block = if index % 2 == 0 {
            BlockKind::Stone
        } else {
            BlockKind::Air
        };
        black_box(
            region
                .commit(
                    region.revision(),
                    &[BlockEdit {
                        position: BlockPos {
                            x: -1,
                            y: 20,
                            z: -1,
                        },
                        block,
                    }],
                )
                .unwrap(),
        );
    }
    println!(
        "single-block owner commits: {iterations} operations in {:?}; final region revision {}",
        start.elapsed(),
        region.revision()
    );

    let start = Instant::now();
    for _ in 0..iterations {
        black_box(
            snapshot
                .section(0)
                .unwrap()
                .blocks()
                .filter(|block| *block != BlockKind::Air)
                .count(),
        );
    }
    println!(
        "packed ground section scan: {iterations} × 4096 blocks in {:?}",
        start.elapsed()
    );

    let captured = region.capture();
    let bytes = storage::encode_snapshot(&captured).unwrap();
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(storage::encode_snapshot(&captured).unwrap());
    }
    println!(
        "region snapshot encode: {iterations} operations in {:?}; {} bytes",
        start.elapsed(),
        bytes.len()
    );
    let start = Instant::now();
    for _ in 0..iterations {
        black_box(storage::decode_snapshot(&bytes).unwrap());
    }
    println!(
        "region snapshot validate/decode: {iterations} operations in {:?}",
        start.elapsed()
    );

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("region.bin");
    let disk_operations = iterations.min(20);
    let start = Instant::now();
    for index in 0..disk_operations {
        let block = if region
            .get_block(BlockPos {
                x: -1,
                y: 20,
                z: -1,
            })
            .unwrap()
            == BlockKind::Air
        {
            BlockKind::Stone
        } else {
            BlockKind::Air
        };
        region
            .commit(
                region.revision(),
                &[BlockEdit {
                    position: BlockPos {
                        x: -1,
                        y: 20,
                        z: -1,
                    },
                    block,
                }],
            )
            .unwrap();
        storage::save_snapshot(&path, &region.capture()).unwrap();
        assert_eq!(
            storage::load(&path).unwrap().revision(),
            region.revision(),
            "disk operation {index}"
        );
    }
    println!(
        "atomic region save + reload: {disk_operations} operations in {:?}",
        start.elapsed()
    );
}
