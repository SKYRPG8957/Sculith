use super::*;

impl GameState {
    // Explicit fixture setup: the production reservation always starts empty.
    fn reserve_fixture(
        &mut self,
        profile: Profile,
        permissions: Permissions,
        now: Instant,
    ) -> Result<PlayerSnapshot> {
        let reserved = self.reserve(profile, permissions, now)?;
        let player = self.players.get_mut(&reserved.id).unwrap();
        for kind in [BlockKind::Stone, BlockKind::Dirt, BlockKind::Grass] {
            player.inventory.give(kind, 64)?;
        }
        Ok(player
            .finish_inventory(crate::inventory::Response {
                request_id: 0,
                result: Ok(vec![]),
                replayed: false,
            })
            .player)
    }
}

#[test]
fn fresh_reservations_and_activation_do_not_create_gameplay_items() {
    let now = Instant::now();
    let mut game = GameState::new(Region::flat_origin(), 20).unwrap();
    let pending = game
        .reserve(profile(1), Permissions { can_build: true }, now)
        .unwrap();
    for player in [pending.clone(), game.activate(pending.id).unwrap()] {
        assert!(player.inventory.iter().all(Option::is_none));
        assert!(player.hotbar.iter().all(Option::is_none));
        assert!(
            player.held().is_none() && player.cursor.is_none() && player.created_output.is_none()
        );
    }
    game.creative_slot(pending.id, 1, Some(BlockKind::Dirt), 5, now)
        .unwrap();
    let selected = game.select_slot(pending.id, 1).unwrap();
    assert_eq!(
        (
            selected.held().unwrap().kind,
            selected.held().unwrap().count
        ),
        (BlockKind::Dirt, 5)
    );
    game.retire(pending.id);
    let rejoined = game
        .reserve(profile(1), Permissions { can_build: true }, now)
        .unwrap();
    assert!(rejoined.inventory.iter().all(Option::is_none));
}

#[test]
fn plugin_inventory_is_live_bounded_and_separate_from_client_permission_and_budget() {
    use sculith_core::inventory::{InventoryError, InventoryOperation};
    let now = Instant::now();
    let mut game = GameState::new(Region::flat_origin(), 20).unwrap();
    let player = game
        .reserve_fixture(profile(1001), Permissions::default(), now)
        .unwrap();
    let give = InventoryOperation::Give {
        item: "minecraft:bedrock".into(),
        count: 65,
    };
    assert!(matches!(
        game.plugin_inventory(player.id, &give, now),
        Err(InventoryError::PlayerUnavailable)
    ));
    game.activate(player.id).unwrap();
    let result = game.plugin_inventory(player.id, &give, now).unwrap();
    assert_eq!(result.player.inventory[3].unwrap().count, 64);
    assert_eq!(result.player.inventory[4].unwrap().count, 1);
    let view = result.player.inventory_view();
    assert_eq!(view.slots.len(), 36);
    assert_eq!(view.slots[4].as_ref().unwrap().item, "minecraft:bedrock");
    assert!(!game.player(player.id).unwrap().permissions.can_build);
    let unknown = InventoryOperation::Give {
        item: "minecraft:unknown".into(),
        count: 1,
    };
    assert!(matches!(
        game.plugin_inventory(player.id, &unknown, now),
        Err(InventoryError::UnsupportedItem)
    ));
    for _ in 0..14 {
        game.plugin_inventory(player.id, &InventoryOperation::Get, now)
            .unwrap();
    }
    assert!(matches!(
        game.plugin_inventory(player.id, &InventoryOperation::Get, now),
        Err(InventoryError::RateLimited)
    ));
    game.plugin_inventory(
        player.id,
        &InventoryOperation::Get,
        now + Duration::from_millis(125),
    )
    .unwrap();
    assert!(matches!(
        game.plugin_inventory(player.id, &give, now),
        Err(InventoryError::RateLimited)
    ));
    let client = game
        .inventory_request(
            player.id,
            crate::inventory::Request {
                id: -1,
                actions: vec![crate::inventory::Action::Destroy {
                    source: crate::inventory::SlotRef {
                        slot: crate::inventory::Slot::Inventory(0),
                        expected_id: 1,
                    },
                    count: 1,
                }],
            },
            now + Duration::from_millis(125),
        )
        .unwrap();
    assert_eq!(
        client.response.result,
        Err(crate::inventory::RejectReason::Unauthorized)
    );
    assert_eq!(game.revision(), 0);
    game.retire(player.id);
    assert!(matches!(
        game.plugin_inventory(player.id, &InventoryOperation::Get, now),
        Err(InventoryError::PlayerUnavailable)
    ));
}
use std::time::Duration;

fn profile(index: u128) -> Profile {
    Profile {
        subject: format!("verified:{index}"),
        name: format!("player{index}"),
        uuid: Uuid::from_u128(index),
        edition: Edition::Bedrock,
    }
}

fn active(region: Region, now: Instant) -> (GameState, u64) {
    let mut game = GameState::new(region, 20).unwrap();
    let player = game
        .reserve_fixture(profile(1), Permissions { can_build: true }, now)
        .unwrap();
    game.activate(player.id).unwrap();
    (game, player.id)
}

#[test]
fn models_require_registered_live_players_and_owner_clock_without_world_mutation() {
    let now = Instant::now();
    let mut state = GameState::with_content(
        Region::flat_origin(),
        20,
        Default::default(),
        ["sculith:demo_statue".into()].into(),
    )
    .unwrap();
    let player = state
        .reserve_fixture(profile(123), Permissions { can_build: true }, now)
        .unwrap();
    let id = player.id;
    assert!(
        state
            .show_model(id, "sculith:demo_statue".into(), now)
            .is_err()
    );
    assert!(state.hide_model(id).is_err());
    state.activate(id).unwrap();
    let revision = state.capture().revision();
    assert!(state.show_model(id, "sculith:missing".into(), now).is_err());
    let first = state
        .show_model(id, "sculith:demo_statue".into(), now)
        .unwrap();
    assert_ne!(first.entity_id, id);
    assert_eq!(
        first.position,
        Position {
            x: 0.5,
            y: 4.0,
            z: 3.5
        }
    );
    assert!(
        state
            .show_model(id, "sculith:demo_statue".into(), now)
            .is_err()
    );
    let second = state
        .show_model(
            id,
            "sculith:demo_statue".into(),
            now + Duration::from_secs(1),
        )
        .unwrap();
    assert_eq!(
        (first.entity_id, first.uuid),
        (second.entity_id, second.uuid)
    );
    assert!(
        state
            .show_model(id, "sculith:demo_statue".into(), now)
            .is_err()
    );
    assert_eq!(state.hide_model(id).unwrap(), Some(first.entity_id));
    assert_eq!(state.hide_model(id).unwrap(), None);
    let third = state
        .show_model(
            id,
            "sculith:demo_statue".into(),
            now + Duration::from_secs(2),
        )
        .unwrap();
    assert_ne!(third.entity_id, first.entity_id);
    state.players.get_mut(&id).unwrap().snapshot.feet.x = 63.5;
    assert!(
        state
            .show_model(
                id,
                "sculith:demo_statue".into(),
                now + Duration::from_secs(3)
            )
            .is_err()
    );
    assert_eq!(
        state.players[&id].model.as_ref().unwrap().entity_id,
        third.entity_id
    );
    assert_eq!(state.capture().revision(), revision);
    state.retire(id);
    assert!(
        state
            .show_model(
                id,
                "sculith:demo_statue".into(),
                now + Duration::from_secs(4)
            )
            .is_err()
    );
    assert!(state.hide_model(id).is_err());
}

#[test]
fn message_budget_is_per_initialized_player_and_uses_the_owner_clock() {
    let now = Instant::now();
    let (mut state, id) = active(Region::flat_origin(), now);
    for _ in 0..8 {
        state.permit_message(id, now).unwrap();
    }
    assert!(state.permit_message(id, now).is_err());
    assert!(
        state
            .permit_message(id, now - Duration::from_millis(1))
            .is_err()
    );
    assert!(
        state
            .permit_message(id, now + Duration::from_millis(249))
            .is_err()
    );
    state
        .permit_message(id, now + Duration::from_millis(250))
        .unwrap();
    let other = state
        .reserve_fixture(profile(2), Permissions { can_build: true }, now)
        .unwrap();
    assert!(state.permit_message(other.id, now).is_err());
    state.activate(other.id).unwrap();
    state.permit_message(other.id, now).unwrap();
    state.retire(id);
    assert!(
        state
            .permit_message(id, now + Duration::from_secs(10))
            .is_err()
    );
    assert_eq!(state.revision(), 0);
}

#[test]
fn forms_bind_replies_to_live_sessions_and_owner_without_replay_or_world_changes() {
    let now = Instant::now();
    let (mut state, id) = active(Region::flat_origin(), now);
    let other = state
        .reserve_fixture(profile(2), Permissions::default(), now)
        .unwrap();
    let form = sculith_core::SimpleForm {
        title: "Menu".into(),
        body: "Choose".into(),
        buttons: vec!["A".into(), "B".into()],
    };
    assert!(
        state
            .show_form(other.id, "owner".into(), 7, &form, now)
            .is_err()
    );
    state.activate(other.id).unwrap();
    let first = state.show_form(id, "owner".into(), 7, &form, now).unwrap();
    assert!(
        state
            .show_form(id, "another".into(), 8, &form, now + Duration::from_secs(1))
            .is_err()
    );
    assert!(state.submit_form(other.id, first, Some(0), now).is_err());
    assert!(state.submit_form(id, first + 1, Some(0), now).is_err());
    assert!(state.submit_form(id, first, Some(2), now).is_err());
    assert!(
        state
            .submit_form(id, first, Some(0), now - Duration::from_millis(1))
            .is_err()
    );
    let result = state.submit_form(id, first, Some(1), now).unwrap();
    assert_eq!(
        result,
        FormSubmission {
            player_id: id,
            plugin: "owner".into(),
            request_id: 7,
            selection: Some(1)
        }
    );
    assert!(state.submit_form(id, first, Some(0), now).is_err());
    assert!(
        state
            .show_form(
                id,
                "owner".into(),
                8,
                &form,
                now + Duration::from_millis(999)
            )
            .is_err()
    );
    let second = state
        .show_form(id, "owner".into(), 8, &form, now + Duration::from_secs(1))
        .unwrap();
    assert_ne!(first, second);
    assert!(
        state
            .submit_form(id, first, Some(0), now + Duration::from_secs(1))
            .is_err()
    );
    assert!(
        state
            .submit_form(id, second, None, now + Duration::from_secs(1))
            .unwrap()
            .selection
            .is_none()
    );
    let third = state
        .show_form(id, "owner".into(), 9, &form, now + Duration::from_secs(2))
        .unwrap();
    assert!(
        state
            .submit_form(id, third, Some(0), now + Duration::from_secs(63))
            .is_err()
    );
    let fourth = state
        .show_form(id, "owner".into(), 10, &form, now + Duration::from_secs(64))
        .unwrap();
    state.retire(id);
    assert!(
        state
            .submit_form(id, fourth, None, now + Duration::from_secs(64))
            .is_err()
    );
    state.next_form_id = u32::MAX;
    assert!(
        state
            .show_form(other.id, "owner".into(), 0, &form, now)
            .is_err()
    );
    assert_eq!(state.revision(), 0);
}

fn accepted(outcome: MoveOutcome) -> PlayerSnapshot {
    match outcome {
        MoveOutcome::Accepted { player, .. } => player,
        other => panic!("movement rejected: {other:?}"),
    }
}

fn rejected_move(
    game: &mut GameState,
    id: u64,
    tick: u64,
    feet: Position,
    rotation: Rotation,
    now: Instant,
    expected: RejectReason,
) {
    let before = game.player(id);
    let budget = game
        .players
        .get(&id)
        .map(|player| (player.last_tick, player.movement, player.building));
    let world = sculith_world::storage::encode_snapshot(&game.capture()).unwrap();
    assert!(
        matches!(game.move_player(id, tick, feet, rotation, now), MoveOutcome::Rejected { reason, .. } if reason == expected)
    );
    assert_eq!(game.player(id), before, "rejection changed player snapshot");
    assert_eq!(
        game.players
            .get(&id)
            .map(|player| (player.last_tick, player.movement, player.building)),
        budget,
        "rejection changed anti-cheat state"
    );
    assert_eq!(
        sculith_world::storage::encode_snapshot(&game.capture()).unwrap(),
        world,
        "rejection changed world state/revision/cache"
    );
}

fn rejected_block(
    game: &mut GameState,
    outcome: BlockOutcome,
    expected: RejectReason,
    before: &RegionSnapshot,
) {
    assert!(
        matches!(outcome, BlockOutcome::Rejected { reason, .. } if reason == expected),
        "{outcome:?}"
    );
    assert_eq!(
        sculith_world::storage::encode_snapshot(&game.capture()).unwrap(),
        sculith_world::storage::encode_snapshot(before).unwrap()
    );
}

#[test]
fn lifecycle_reservations_capacity_namespaced_identity_and_cleanup_are_authoritative() {
    let now = Instant::now();
    let mut game = GameState::new(Region::flat_origin(), 2).unwrap();
    let first = game
        .reserve_fixture(profile(1), Permissions::default(), now)
        .unwrap();
    assert_eq!(game.online_count(), 0);
    assert!(!first.initialized);
    let mut duplicate = profile(2);
    duplicate.subject = first.profile.subject.clone();
    assert!(
        game.reserve_fixture(duplicate.clone(), Permissions::default(), now)
            .is_err()
    );
    duplicate.edition = Edition::Java;
    let second = game
        .reserve_fixture(duplicate, Permissions::default(), now)
        .unwrap();
    assert_ne!(first.id, second.id);
    assert!(first.id != 0 && second.id <= i64::MAX as u64);
    assert!(
        game.reserve_fixture(profile(3), Permissions::default(), now)
            .is_err()
    );
    game.activate(first.id).unwrap();
    game.activate(first.id).unwrap();
    assert_eq!(game.online_count(), 1);
    assert!(!game.retire(second.id).unwrap().initialized);
    assert!(game.retire(second.id).is_none());
    assert!(game.retire(first.id).unwrap().initialized);
    assert_eq!(game.online_count(), 0);
    assert!(
        game.reserve_fixture(profile(1), Permissions::default(), now)
            .is_ok()
    );
}

#[test]
fn invalid_profiles_capacity_and_id_exhaustion_fail_without_world_changes() {
    let now = Instant::now();
    assert!(GameState::new(Region::flat_origin(), 0).is_err());
    assert!(GameState::new(Region::flat_origin(), 65).is_err());
    let mut game = GameState::new(Region::flat_origin(), 1).unwrap();
    for mut candidate in [profile(1), profile(2), profile(3), profile(4)] {
        match candidate.uuid.as_u128() {
            1 => candidate.subject.clear(),
            2 => candidate.name = "forged\nname".into(),
            3 => candidate.uuid = Uuid::nil(),
            _ => candidate.subject = "x".repeat(257),
        }
        assert!(
            game.reserve_fixture(candidate, Permissions::default(), now)
                .is_err()
        );
    }
    assert!(game.players.is_empty());
    assert_eq!(game.region.loaded_chunks(), 0);
    let counter = AtomicU64::new(i64::MAX as u64);
    assert_eq!(allocate_id(&counter).unwrap(), i64::MAX as u64);
    assert!(allocate_id(&counter).is_err());
    assert_eq!(counter.load(Ordering::Relaxed), i64::MAX as u64 + 1);
}

#[test]
fn pending_players_cannot_move_build_or_select_inventory() {
    let now = Instant::now();
    let mut game = GameState::new(Region::flat_origin(), 2).unwrap();
    let player = game
        .reserve_fixture(profile(1), Permissions { can_build: true }, now)
        .unwrap();
    rejected_move(
        &mut game,
        player.id,
        1,
        player.feet,
        Rotation::default(),
        now,
        RejectReason::NotInitialized,
    );
    let before = game.capture();
    let result = game.break_block(player.id, BlockPos { x: 1, y: 3, z: 0 }, now);
    rejected_block(&mut game, result, RejectReason::NotInitialized, &before);
    assert!(game.select_slot(player.id, 1).is_err());
    rejected_move(
        &mut game,
        u64::MAX,
        1,
        player.feet,
        Rotation::default(),
        now,
        RejectReason::UnknownPlayer,
    );
}

#[test]
fn wall_clock_budget_cannot_be_bypassed_by_client_tick_jumps() {
    let now = Instant::now();
    let (mut game, id) = active(Region::flat_origin(), now);
    let rotation = Rotation {
        yaw: 450.0,
        pitch: 10.0,
        head_yaw: 720.0,
    };
    let first = accepted(game.move_player(
        id,
        1,
        Position {
            x: 1.5,
            y: 4.0,
            z: 0.5,
        },
        rotation,
        now,
    ));
    assert_eq!(
        first.rotation,
        Rotation {
            yaw: 90.0,
            pitch: 10.0,
            head_yaw: 0.0
        }
    );
    rejected_move(
        &mut game,
        id,
        u64::MAX,
        Position {
            x: 3.5,
            y: 4.0,
            z: 0.5,
        },
        rotation,
        now,
        RejectReason::SpeedLimit,
    );
    accepted(game.move_player(
        id,
        1_000_000_000,
        Position {
            x: 2.5,
            y: 4.0,
            z: 0.5,
        },
        rotation,
        now,
    ));
    rejected_move(
        &mut game,
        id,
        1_000_000_001,
        Position {
            x: 2.6,
            y: 4.0,
            z: 0.5,
        },
        rotation,
        now,
        RejectReason::SpeedLimit,
    );
    accepted(game.move_player(
        id,
        1_000_000_001,
        Position {
            x: 3.1,
            y: 4.0,
            z: 0.5,
        },
        rotation,
        now + Duration::from_millis(50),
    ));
    assert_eq!(game.revision(), 0, "movement is not a block revision");
}

#[test]
fn invalid_movement_and_replayed_ticks_leave_all_state_unchanged() {
    let now = Instant::now();
    let (mut game, id) = active(Region::flat_origin(), now);
    let feet = game.player(id).unwrap().feet;
    accepted(game.move_player(id, 10, feet, Rotation::default(), now));
    for position in [
        Position {
            x: f64::NAN,
            ..feet
        },
        Position {
            y: f64::INFINITY,
            ..feet
        },
        Position {
            x: f64::from(WORLD_BORDER),
            ..feet
        },
        Position { y: 319.0, ..feet },
    ] {
        rejected_move(
            &mut game,
            id,
            11,
            position,
            Rotation::default(),
            now,
            RejectReason::InvalidPosition,
        );
    }
    rejected_move(
        &mut game,
        id,
        11,
        Position { x: 64.0, ..feet },
        Rotation::default(),
        now,
        RejectReason::OutsideRegion,
    );
    for rotation in [
        Rotation {
            yaw: f64::NAN,
            ..Rotation::default()
        },
        Rotation {
            pitch: 90.1,
            ..Rotation::default()
        },
        Rotation {
            head_yaw: f64::INFINITY,
            ..Rotation::default()
        },
    ] {
        rejected_move(
            &mut game,
            id,
            11,
            feet,
            rotation,
            now,
            RejectReason::InvalidRotation,
        );
    }
    rejected_move(
        &mut game,
        id,
        10,
        feet,
        Rotation::default(),
        now,
        RejectReason::StaleTick,
    );
    rejected_move(
        &mut game,
        id,
        9,
        feet,
        Rotation::default(),
        now,
        RejectReason::StaleTick,
    );
    rejected_move(
        &mut game,
        id,
        11,
        feet,
        Rotation::default(),
        now - Duration::from_millis(1),
        RejectReason::ClockRegression,
    );
}

#[test]
fn swept_body_rejects_wall_tunneling_and_floor_penetration_but_allows_touching() {
    let now = Instant::now();
    let mut region = Region::flat_origin();
    region
        .commit(
            0,
            &[BlockEdit {
                position: BlockPos { x: 1, y: 5, z: 0 },
                block: BlockKind::Stone,
            }],
        )
        .unwrap();
    let (mut game, id) = active(region, now);
    rejected_move(
        &mut game,
        id,
        1,
        Position {
            x: 2.5,
            y: 4.0,
            z: 0.5,
        },
        Rotation::default(),
        now,
        RejectReason::Collision,
    );
    rejected_move(
        &mut game,
        id,
        1,
        Position {
            x: 0.5,
            y: 3.99,
            z: 0.5,
        },
        Rotation::default(),
        now,
        RejectReason::Collision,
    );
    accepted(game.move_player(
        id,
        1,
        Position {
            x: 0.7,
            y: 4.0,
            z: 0.5,
        },
        Rotation::default(),
        now,
    ));
    rejected_move(
        &mut game,
        id,
        2,
        Position {
            x: 0.70001,
            y: 4.0,
            z: 0.5,
        },
        Rotation::default(),
        now,
        RejectReason::Collision,
    );
    accepted(game.move_player(
        id,
        2,
        Position {
            x: 0.7,
            y: 4.0,
            z: 1.5,
        },
        Rotation::default(),
        now,
    ));
}

#[test]
fn player_body_must_fit_region_not_only_its_center() {
    let now = Instant::now();
    let bounds =
        sculith_world::RegionBounds::new(ChunkPos { x: 0, z: 0 }, ChunkPos { x: 1, z: 1 }).unwrap();
    let (mut game, id) = active(Region::new(sculith_world::RegionId(1), bounds), now);
    rejected_move(
        &mut game,
        id,
        1,
        Position {
            x: 0.2,
            y: 4.0,
            z: 0.5,
        },
        Rotation::default(),
        now,
        RejectReason::OutsideRegion,
    );
    accepted(game.move_player(
        id,
        1,
        Position {
            x: 0.3,
            y: 4.0,
            z: 0.5,
        },
        Rotation::default(),
        now,
    ));
}

#[test]
fn new_reservations_find_clearance_after_persisted_or_recent_spawn_builds() {
    let now = Instant::now();
    let mut region = Region::flat_origin();
    region
        .commit(
            0,
            &[BlockEdit {
                position: BlockPos { x: 0, y: 4, z: 0 },
                block: BlockKind::Stone,
            }],
        )
        .unwrap();
    let mut game = GameState::new(region, 2).unwrap();
    let first = game
        .reserve_fixture(profile(1), Permissions::default(), now)
        .unwrap();
    assert_eq!(first.feet.y, 5.0);
    game.region
        .commit(
            1,
            &[BlockEdit {
                position: BlockPos { x: 0, y: 5, z: 0 },
                block: BlockKind::Stone,
            }],
        )
        .unwrap();
    let second = game
        .reserve_fixture(profile(2), Permissions::default(), now)
        .unwrap();
    assert_eq!(second.feet.y, 6.0);
}

#[test]
fn creative_place_break_and_noop_use_one_world_and_server_inventory() {
    let now = Instant::now();
    let (mut game, id) = active(Region::flat_origin(), now);
    let old = game.snapshot(ChunkPos { x: 0, z: 0 }).unwrap();
    let target = BlockPos { x: 1, y: 4, z: 0 };
    let BlockOutcome::Committed { batch } =
        game.place_block(id, BlockPos { y: 3, ..target }, 1, 0, BlockKind::Stone, now)
    else {
        panic!("place failed")
    };
    assert_eq!((batch.previous_revision, batch.revision), (0, 1));
    assert_eq!(batch.changes[0].after, BlockKind::Stone);
    assert_eq!(old.get(1, 4, 0).unwrap(), BlockKind::Air);
    assert_eq!(game.region.get_block(target).unwrap(), BlockKind::Stone);
    let before = game.capture();
    let result = game.place_block(id, BlockPos { y: 3, ..target }, 1, 0, BlockKind::Stone, now);
    rejected_block(&mut game, result, RejectReason::Occupied, &before);
    assert!(matches!(
        game.break_block(id, target, now),
        BlockOutcome::Committed { .. }
    ));
    let remaining = game.players[&id].building;
    assert_eq!(
        game.break_block(id, target, now),
        BlockOutcome::NoChange {
            position: target,
            current: BlockKind::Air
        }
    );
    assert_eq!(game.players[&id].building, remaining);
    assert_eq!(game.revision(), 2);
    assert_eq!(game.player(id).unwrap().hotbar[0], Some(BlockKind::Stone));
}

#[test]
fn permissions_and_immutable_bedrock_are_enforced_before_committing() {
    let now = Instant::now();
    let mut region = Region::flat_origin();
    region
        .commit(
            0,
            &(1..=3)
                .map(|y| BlockEdit {
                    position: BlockPos { x: 0, y, z: 0 },
                    block: BlockKind::Air,
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
    let (mut game, id) = active(region, now);
    let before = game.capture();
    let result = game.break_block(id, BlockPos { x: 0, y: 0, z: 0 }, now);
    rejected_block(&mut game, result, RejectReason::Unbreakable, &before);
    let other = game
        .reserve_fixture(profile(2), Permissions::default(), now)
        .unwrap();
    game.activate(other.id).unwrap();
    let result = game.break_block(other.id, BlockPos { x: 1, y: 3, z: 0 }, now);
    rejected_block(&mut game, result, RejectReason::PermissionDenied, &before);
}

#[test]
fn reach_occlusion_bounds_and_checked_face_offsets_refuse_bad_builds() {
    let now = Instant::now();
    let mut region = Region::flat_origin();
    let mut edits: Vec<_> = (4..=6)
        .map(|y| BlockEdit {
            position: BlockPos { x: 1, y, z: 0 },
            block: BlockKind::Stone,
        })
        .collect();
    edits.push(BlockEdit {
        position: BlockPos { x: 2, y: 4, z: 0 },
        block: BlockKind::Stone,
    });
    region.commit(0, &edits).unwrap();
    let (mut game, id) = active(region, now);
    let before = game.capture();
    for (position, reason) in [
        (BlockPos { x: 2, y: 4, z: 0 }, RejectReason::Occluded),
        (BlockPos { x: 10, y: 3, z: 0 }, RejectReason::TooFar),
        (BlockPos { x: 64, y: 3, z: 0 }, RejectReason::OutsideRegion),
        (
            BlockPos {
                x: i32::MAX,
                y: 3,
                z: 0,
            },
            RejectReason::InvalidPosition,
        ),
    ] {
        let result = game.break_block(id, position, now);
        rejected_block(&mut game, result, reason, &before);
    }
    for (against, face, reason) in [
        (
            BlockPos { x: 1, y: 3, z: 1 },
            255,
            RejectReason::InvalidFace,
        ),
        (
            BlockPos {
                x: i32::MAX,
                y: 3,
                z: 0,
            },
            5,
            RejectReason::InvalidPosition,
        ),
        (
            BlockPos { x: -1, y: 4, z: 0 },
            1,
            RejectReason::InvalidTarget,
        ),
    ] {
        let result = game.place_block(id, against, face, 0, BlockKind::Stone, now);
        rejected_block(&mut game, result, reason, &before);
    }
}

#[test]
fn selected_slot_and_server_held_kind_must_match_client_claims() {
    let now = Instant::now();
    let (mut game, id) = active(Region::flat_origin(), now);
    let against = BlockPos { x: 1, y: 3, z: 0 };
    let before = game.capture();
    let mut forged_copy = game.player(id).unwrap();
    forged_copy.hotbar[0] = Some(BlockKind::Bedrock);
    let result = game.place_block(id, against, 1, 0, BlockKind::Bedrock, now);
    rejected_block(&mut game, result, RejectReason::WrongHeldItem, &before);
    assert!(game.select_slot(id, 9).is_err());
    game.select_slot(id, 1).unwrap();
    let result = game.place_block(id, against, 1, 0, BlockKind::Stone, now);
    rejected_block(&mut game, result, RejectReason::InvalidSlot, &before);
    let result = game.place_block(id, against, 1, 1, BlockKind::Stone, now);
    rejected_block(&mut game, result, RejectReason::WrongHeldItem, &before);
    game.select_slot(id, 8).unwrap();
    let result = game.place_block(id, against, 1, 8, BlockKind::Stone, now);
    rejected_block(&mut game, result, RejectReason::WrongHeldItem, &before);
    game.select_slot(id, 1).unwrap();
    assert!(matches!(
        game.place_block(id, against, 1, 1, BlockKind::Dirt, now),
        BlockOutcome::Committed { .. }
    ));
    assert_eq!(game.player(id).unwrap().hotbar[1], Some(BlockKind::Dirt));
}

#[test]
fn placing_cannot_intersect_any_active_or_pending_player_body() {
    let now = Instant::now();
    let (mut game, id) = active(Region::flat_origin(), now);
    let second = game
        .reserve_fixture(profile(2), Permissions::default(), now)
        .unwrap();
    game.activate(second.id).unwrap();
    accepted(game.move_player(
        second.id,
        1,
        Position {
            x: 1.5,
            y: 4.0,
            z: 0.5,
        },
        Rotation::default(),
        now,
    ));
    let before = game.capture();
    let result = game.place_block(
        id,
        BlockPos { x: 1, y: 3, z: 0 },
        1,
        0,
        BlockKind::Stone,
        now,
    );
    rejected_block(&mut game, result, RejectReason::Collision, &before);
    accepted(game.move_player(
        id,
        1,
        Position {
            x: 2.5,
            y: 4.0,
            z: 0.5,
        },
        Rotation::default(),
        now,
    ));
    game.reserve_fixture(profile(3), Permissions::default(), now)
        .unwrap();
    let result = game.place_block(
        id,
        BlockPos { x: 0, y: 3, z: 0 },
        1,
        0,
        BlockKind::Stone,
        now,
    );
    rejected_block(&mut game, result, RejectReason::Collision, &before);
}

#[test]
fn creative_build_rate_uses_server_time_and_rejections_do_not_consume_budget() {
    let now = Instant::now();
    let (mut game, id) = active(Region::flat_origin(), now);
    for (x, z) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
        assert!(matches!(
            game.place_block(id, BlockPos { x, y: 3, z }, 1, 0, BlockKind::Stone, now),
            BlockOutcome::Committed { .. }
        ));
    }
    let before = game.capture();
    let budget = game.players[&id].building;
    let against = BlockPos { x: 1, y: 3, z: 1 };
    let result = game.place_block(id, against, 1, 0, BlockKind::Stone, now);
    rejected_block(&mut game, result, RejectReason::RateLimited, &before);
    assert_eq!(game.players[&id].building, budget);
    assert!(matches!(
        game.place_block(
            id,
            against,
            1,
            0,
            BlockKind::Stone,
            now + Duration::from_millis(50)
        ),
        BlockOutcome::Committed { .. }
    ));
    let before = game.capture();
    let result = game.break_block(id, BlockPos { y: 4, ..against }, now);
    rejected_block(&mut game, result, RejectReason::ClockRegression, &before);
}
