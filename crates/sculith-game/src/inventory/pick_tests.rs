use super::*;
use crate::{Edition, GameState, Permissions, Profile};
use sculith_world::{BlockPos, Region};
use std::time::Instant;

#[test]
fn picks_create_one_reuse_counts_and_preserve_displaced_items() {
    let mut inventory = Inventory::default();
    let (slot, changes) = inventory.pick(BlockKind::Grass, 8, true).unwrap();
    assert_eq!(slot, 0);
    assert_eq!(changes.len(), 1);
    assert_eq!(inventory.slots()[0].unwrap().count, 1);
    let unchanged = inventory.clone();
    assert_eq!(
        inventory.pick(BlockKind::Grass, 0, true).unwrap(),
        (0, vec![])
    );
    assert_eq!(inventory, unchanged);
    assert_eq!(inventory.pick(BlockKind::Dirt, 0, true).unwrap().0, 1);
    assert_eq!(inventory.pick(BlockKind::Grass, 1, true).unwrap().0, 0);
    for slot in 0..9 {
        inventory
            .replace_creative(slot, Some(BlockKind::Stone), 64, true)
            .unwrap();
    }
    inventory
        .replace_creative(19, Some(BlockKind::Grass), 7, true)
        .unwrap();
    let before = inventory.slots()[19].unwrap();
    let (selected, changes) = inventory.pick(BlockKind::Grass, 2, true).unwrap();
    assert_eq!(selected, 2);
    assert_eq!(changes.len(), 2);
    assert_eq!(
        (
            inventory.slots()[2].unwrap().kind,
            inventory.slots()[2].unwrap().count
        ),
        (BlockKind::Grass, 7)
    );
    assert_eq!(inventory.slots()[2].unwrap().net_id, before.net_id);
    assert_eq!(
        (
            inventory.slots()[19].unwrap().kind,
            inventory.slots()[19].unwrap().count
        ),
        (BlockKind::Stone, 64)
    );
    inventory.pick(BlockKind::Dirt, 2, true).unwrap();
    assert_eq!(
        (
            inventory.slots()[9].unwrap().kind,
            inventory.slots()[9].unwrap().count
        ),
        (BlockKind::Grass, 7)
    );
    assert_eq!(
        (
            inventory.slots()[2].unwrap().kind,
            inventory.slots()[2].unwrap().count
        ),
        (BlockKind::Dirt, 1)
    );
}

#[test]
fn picks_preserve_cursor_output_history_and_roll_back_allocator_exhaustion() {
    let mut inventory = Inventory::default();
    inventory
        .replace_creative(0, Some(BlockKind::Stone), 13, true)
        .unwrap();
    inventory
        .click(
            Click::Pickup {
                slot: 0,
                right: true,
            },
            true,
        )
        .unwrap();
    inventory
        .apply(
            &Request {
                id: -1,
                actions: vec![Action::CraftCreative { catalogue_id: 4 }],
            },
            true,
        )
        .result
        .unwrap();
    let cursor = inventory.cursor();
    let output = inventory.created_output();
    let history = inventory.history.clone();
    inventory.pick(BlockKind::Grass, 0, true).unwrap();
    assert_eq!(inventory.cursor(), cursor);
    assert_eq!(inventory.created_output(), output);
    assert_eq!(inventory.history, history);
    inventory
        .replace_creative(20, Some(BlockKind::Dirt), 17, true)
        .unwrap();
    let dirt_id = inventory.slots()[20].unwrap().net_id;
    inventory.state.next_stack_id = i32::MAX as u64 + 1;
    inventory.pick(BlockKind::Dirt, 0, true).unwrap();
    assert_eq!(inventory.slots()[0].unwrap().net_id, dirt_id);
    assert_eq!(inventory.state.next_stack_id, i32::MAX as u64 + 1);
    let before = inventory.clone();
    assert_eq!(
        inventory.pick(BlockKind::Bedrock, 0, true),
        Err(RejectReason::StackIdExhausted)
    );
    assert_eq!(inventory, before);
    for (kind, selected, authorized, reason) in [
        (BlockKind::Grass, 0, false, RejectReason::Unauthorized),
        (BlockKind::Grass, 9, true, RejectReason::InvalidSlot),
        (BlockKind::Air, 0, true, RejectReason::UnsupportedItem),
    ] {
        assert_eq!(inventory.pick(kind, selected, authorized), Err(reason));
        assert_eq!(inventory, before);
    }
}

#[test]
fn completely_full_creative_inventory_replaces_only_the_selected_slot() {
    let mut inventory = Inventory::default();
    for slot in 0..36 {
        inventory
            .replace_creative(slot, Some(BlockKind::Stone), 64, true)
            .unwrap();
    }
    let before = *inventory.slots();
    let (selected, changes) = inventory.pick(BlockKind::Grass, 7, true).unwrap();
    assert_eq!(selected, 7);
    assert_eq!(changes.len(), 1);
    for (slot, item) in inventory.slots().iter().enumerate() {
        if slot == 7 {
            assert_eq!(item.unwrap().count, 1);
            assert_eq!(item.unwrap().kind, BlockKind::Grass);
        } else {
            assert_eq!(*item, before[slot]);
        }
    }
}

fn player(game: &mut GameState, index: u128, build: bool, now: Instant) -> u64 {
    let player = game
        .reserve(
            Profile {
                subject: format!("pick:{index}"),
                name: format!("pick{index}"),
                uuid: uuid::Uuid::from_u128(index),
                edition: Edition::Java,
            },
            Permissions { can_build: build },
            now,
        )
        .unwrap();
    player.id
}

#[test]
fn owner_checks_initialization_permission_reach_occlusion_and_rate_without_world_edits() {
    let now = Instant::now();
    let mut game = GameState::new(Region::flat_origin(), 20).unwrap();
    let id = player(&mut game, 1, true, now);
    let target = BlockPos { x: 1, y: 3, z: 1 };
    assert!(game.pick_block(id, target, now).is_err());
    game.activate(id).unwrap();
    for position in [
        BlockPos { x: 1, y: 2, z: 1 },
        BlockPos { x: 63, y: 3, z: 63 },
        BlockPos { x: 64, y: 3, z: 0 },
        BlockPos { x: 0, y: 320, z: 0 },
        BlockPos { x: 1, y: 4, z: 1 },
    ] {
        let before = game.player(id).unwrap();
        let outcome = game.pick_block(id, position, now).unwrap();
        assert!(outcome.response.result.is_err());
        assert_eq!(outcome.player, before);
    }
    let outcome = game.pick_block(id, target, now).unwrap();
    assert_eq!(outcome.player.held().unwrap().count, 1);
    let read_only = player(&mut game, 2, false, now);
    game.activate(read_only).unwrap();
    assert_eq!(
        game.pick_block(read_only, target, now)
            .unwrap()
            .response
            .result,
        Err(RejectReason::Unauthorized)
    );
    let mut limited = false;
    for _ in 0..100 {
        if game.pick_block(id, target, now).unwrap().response.result
            == Err(RejectReason::RateLimited)
        {
            limited = true;
            break;
        }
    }
    assert!(limited);
    assert_eq!(game.revision(), 0);
    game.retire(id);
    assert!(game.pick_block(id, target, now).is_err());
}
