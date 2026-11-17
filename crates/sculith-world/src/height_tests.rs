use super::*;

fn check_counts(section: &Section, reference: &[BlockKind]) {
    for kind in BlockKind::ALL {
        assert_eq!(
            section.count(kind) as usize,
            reference.iter().filter(|&&b| b == kind).count()
        );
    }
    assert_eq!(
        BlockKind::ALL
            .iter()
            .map(|&b| usize::from(section.count(b)))
            .sum::<usize>(),
        SECTION_BLOCKS
    );
}

#[test]
fn occupancy_survives_palette_growth_decode_removed_entries_and_collapse() {
    let mut reference = vec![BlockKind::Air; SECTION_BLOCKS];
    let mut section = Section::uniform(BlockKind::Air);
    check_counts(&section, &reference);
    let mut random = 0x16c7_9a35_u32;
    for step in 0..6000 {
        random = random.wrapping_mul(1664525).wrapping_add(1013904223);
        let index = random as usize % SECTION_BLOCKS;
        let kind = BlockKind::ALL[(random >> 16) as usize % BlockKind::ALL.len()];
        reference[index] = kind;
        section.set_index(index, kind);
        if step % 97 == 0 {
            check_counts(&section, &reference);
            check_counts(&Section::decode(&section.encode()).unwrap(), &reference);
        }
    }
    for kind in [BlockKind::Stone, BlockKind::Air] {
        for (index, value) in reference.iter_mut().enumerate() {
            *value = kind;
            section.set_index(index, kind);
        }
        assert!(section.stats().uniform);
        check_counts(&section, &reference);
        check_counts(&Section::decode(&section.encode()).unwrap(), &reference);
    }
}

fn check_heights(snapshot: &ChunkSnapshot) {
    let heights = snapshot.non_air_heights();
    for z in 0..16u8 {
        for x in 0..16u8 {
            let expected = (MIN_Y..=MAX_Y)
                .rev()
                .find(|&y| snapshot.get(x, y, z).unwrap() != BlockKind::Air)
                .map_or(MIN_Y, |y| y + 1);
            assert_eq!(
                heights[x as usize + 16 * z as usize],
                expected,
                "column ({x},{z})"
            );
        }
    }
}

#[test]
fn heights_match_voxel_oracle_at_boundaries_in_empty_sparse_dense_and_solid_chunks() {
    for pattern in 0..6 {
        let sections = std::array::from_fn(|index| {
            let mut blocks = vec![BlockKind::Air; SECTION_BLOCKS];
            for y in 0..16 {
                let world_y = (MIN_SECTION_Y + index as i32) * 16 + y;
                for z in 0..16 {
                    for x in 0..16 {
                        let kind = match pattern {
                            0 => BlockKind::Air,
                            1 => BlockKind::Stone,
                            2 => {
                                BlockKind::ALL
                                    [((x + 3 * z) as i32 + world_y * 7).rem_euclid(5) as usize]
                            }
                            3 if (x + z) % 7 != 0
                                && world_y == MIN_Y + ((x * 37 + z * 19) % 384) as i32 =>
                            {
                                BlockKind::Grass
                            }
                            4 if index == 1 => BlockKind::Dirt,
                            4 if (x, z, world_y) == (15, 15, MAX_Y)
                                || (x, z, world_y) == (0, 0, MIN_Y) =>
                            {
                                BlockKind::Stone
                            }
                            5 if (x, z, world_y) == (0, 0, MIN_Y) => BlockKind::Stone,
                            _ => BlockKind::Air,
                        };
                        blocks[x + 16 * z + 256 * y as usize] = kind;
                    }
                }
            }
            let section = Section::from_blocks(&blocks).unwrap();
            Arc::new(Section::decode(&section.encode()).unwrap())
        });
        let snapshot = ChunkSnapshot {
            data: Arc::new(ChunkData {
                position: ChunkPos { x: -1, z: -2 },
                revision: 0,
                sections,
            }),
        };
        check_heights(&snapshot);
    }
}

#[test]
fn heights_follow_commits_removal_and_immutable_old_snapshots() {
    let mut owner = Region::flat_origin();
    let position = ChunkPos { x: -1, z: -1 };
    let flat = owner.snapshot(position).unwrap();
    let block = BlockPos {
        x: -1,
        y: MAX_Y,
        z: -16,
    };
    owner
        .commit(
            0,
            &[BlockEdit {
                position: block,
                block: BlockKind::Stone,
            }],
        )
        .unwrap();
    let raised = owner.snapshot(position).unwrap();
    assert_eq!(raised.non_air_heights()[15], MAX_Y + 1);
    owner
        .commit(
            1,
            &[BlockEdit {
                position: block,
                block: BlockKind::Air,
            }],
        )
        .unwrap();
    let removed = owner.snapshot(position).unwrap();
    assert_eq!(flat.non_air_heights(), [4; 256]);
    assert_eq!(removed.non_air_heights(), [4; 256]);
    assert_eq!(raised.non_air_heights()[15], MAX_Y + 1);
    for snapshot in [flat, raised, removed] {
        check_heights(&snapshot);
    }
}
