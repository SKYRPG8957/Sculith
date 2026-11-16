use super::*;
use sculith_world::Region;

fn position(x: i32, y: i32, z: i32) -> WorldPosition {
    WorldPosition { x, y, z }
}
fn block(x: i32, name: &str) -> WorldBlock {
    WorldBlock {
        position: position(x, 4, 1),
        block: name.into(),
    }
}
fn edit(revision: u64, blocks: Vec<WorldBlock>) -> WorldOperation {
    WorldOperation::Edit {
        region_id: 0,
        expected_revision: revision,
        blocks,
    }
}
fn rejected(result: (WorldResult, Option<CommittedBatch>), expected: WorldError) {
    assert!(matches!(result, (WorldResult::Rejected { reason }, None) if reason == expected));
}

#[test]
fn reads_discover_a_consistent_region_without_loading_chunks_or_changing_revisions() {
    let mut game = GameState::new(Region::flat_origin(), 20).unwrap();
    let now = Instant::now();
    let request = WorldOperation::Read {
        positions: vec![position(-1, 4, 1), position(17, 4, -1)],
    };
    let (result, notification) = game.plugin_world(&request, now);
    let WorldResult::Snapshot { region, blocks } = result else {
        panic!("missing snapshot")
    };
    assert_eq!((region.id, region.revision), (0, 0));
    assert_eq!(region.min, position(-64, -64, -64));
    assert_eq!(region.max_exclusive, position(64, 320, 64));
    assert_eq!(blocks.len(), 2);
    assert!(blocks.iter().all(|b| b.block == "minecraft:air"));
    assert!(notification.is_none());
    assert_eq!(game.region.loaded_chunks(), 0);
    let (result, _) = game.plugin_world(&WorldOperation::Read { positions: vec![] }, now);
    assert!(matches!(result, WorldResult::Snapshot { blocks, .. } if blocks.is_empty()));
    assert_eq!(game.revision(), 0);
}

#[test]
fn edits_are_atomic_across_chunks_and_refuse_bad_bounds_names_duplicates_and_regions() {
    let mut game = GameState::new(Region::flat_origin(), 20).unwrap();
    let now = Instant::now();
    rejected(
        game.plugin_world(
            &edit(
                0,
                vec![
                    block(1, "minecraft:stone"),
                    block(i32::MAX, "minecraft:dirt"),
                ],
            ),
            now,
        ),
        WorldError::OutsideRegion,
    );
    rejected(
        game.plugin_world(
            &edit(
                0,
                vec![block(1, "minecraft:stone"), block(17, "minecraft:glass")],
            ),
            now,
        ),
        WorldError::UnsupportedBlock,
    );
    rejected(
        game.plugin_world(
            &edit(
                0,
                vec![block(1, "minecraft:stone"), block(1, "minecraft:dirt")],
            ),
            now,
        ),
        WorldError::InvalidRequest,
    );
    rejected(
        game.plugin_world(
            &WorldOperation::Edit {
                region_id: 99,
                expected_revision: 0,
                blocks: vec![block(1, "minecraft:stone")],
            },
            now,
        ),
        WorldError::UnknownRegion,
    );
    rejected(
        game.plugin_world(&edit(0, vec![]), now),
        WorldError::InvalidRequest,
    );
    assert_eq!((game.revision(), game.region.loaded_chunks()), (0, 0));
    let before = game.capture();
    let (result, committed) = game.plugin_world(
        &edit(
            0,
            vec![block(-1, "minecraft:stone"), block(17, "minecraft:dirt")],
        ),
        now,
    );
    let WorldResult::Updated {
        region,
        previous_revision,
        changes,
    } = result
    else {
        panic!("missing update")
    };
    assert_eq!(
        (region.revision, previous_revision, changes.len()),
        (1, 0, 2)
    );
    assert_eq!(committed.unwrap().changes.len(), 2);
    assert_eq!(game.region.loaded_chunks(), 2);
    assert_eq!(before.revision(), 0);
    assert!(before.chunks().is_empty());
    assert_eq!(
        game.region
            .get_block(BlockPos { x: -1, y: 4, z: 1 })
            .unwrap(),
        BlockKind::Stone
    );
}

#[test]
fn compare_and_set_and_noop_edits_preserve_revisions_snapshots_and_notifications() {
    let mut game = GameState::new(Region::flat_origin(), 20).unwrap();
    let now = Instant::now();
    let (result, notification) = game.plugin_world(&edit(0, vec![block(1, "minecraft:air")]), now);
    assert!(
        matches!(result,WorldResult::Updated {region,changes,..} if region.revision==0 && changes.is_empty())
    );
    assert!(notification.is_none());
    assert_eq!(game.region.loaded_chunks(), 0);
    game.plugin_world(&edit(0, vec![block(1, "minecraft:stone")]), now);
    let snapshot = game.capture();
    rejected(
        game.plugin_world(&edit(0, vec![block(1, "minecraft:dirt")]), now),
        WorldError::RevisionConflict {
            expected: 0,
            actual: 1,
        },
    );
    let (_, notification) = game.plugin_world(&edit(1, vec![block(1, "minecraft:stone")]), now);
    assert!(notification.is_none());
    assert_eq!(game.revision(), 1);
    let (result, notification) = game.plugin_world(&edit(1, vec![block(1, "minecraft:air")]), now);
    assert!(matches!(result,WorldResult::Updated {region,..} if region.revision==2));
    assert!(notification.is_some());
    assert_eq!(snapshot.chunks()[0].get(1, 4, 1).unwrap(), BlockKind::Stone);
}

#[test]
fn world_work_budget_charges_blocks_and_refills_from_the_owner_clock() {
    let mut game = GameState::new(Region::flat_origin(), 20).unwrap();
    let now = Instant::now();
    let operation = WorldOperation::Read {
        positions: vec![position(1, 4, 1); 64],
    };
    for _ in 0..128 {
        assert!(matches!(
            game.plugin_world(&operation, now).0,
            WorldResult::Snapshot { .. }
        ));
    }
    let info = WorldOperation::Read { positions: vec![] };
    rejected(game.plugin_world(&info, now), WorldError::RateLimited);
    let later = now + std::time::Duration::from_millis(1);
    for _ in 0..4 {
        assert!(matches!(
            game.plugin_world(&info, later).0,
            WorldResult::Snapshot { .. }
        ));
    }
    rejected(game.plugin_world(&info, later), WorldError::RateLimited);
    rejected(game.plugin_world(&info, now), WorldError::RateLimited);
    assert!(matches!(
        game.plugin_world(&info, now + std::time::Duration::from_secs(1))
            .0,
        WorldResult::Snapshot { .. }
    ));
    assert_eq!((game.revision(), game.region.loaded_chunks()), (0, 0));
}
