//! Deterministic kernel baseline, not TPS or a comparison with complete servers.
use sculith_server::game::{
    BlockOutcome, Edition, GameState, MoveOutcome, Permissions, Position, Profile, Rotation,
};
use sculith_server::inventory::{Action, Request, Slot, SlotRef};
use sculith_world::{BlockPos, Region};
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

fn main() {
    let mut game = GameState::new(Region::flat_origin(), 20).unwrap();
    let clock = Instant::now();
    let player = game
        .reserve(
            Profile {
                subject: "benchmark".into(),
                name: "benchmark".into(),
                uuid: uuid::Uuid::from_u128(1),
                edition: Edition::Bedrock,
            },
            Permissions { can_build: true },
            clock,
        )
        .unwrap();
    game.activate(player.id).unwrap();
    let started = Instant::now();
    for tick in 1..=10_000 {
        let result = game.move_player(
            player.id,
            tick,
            Position {
                x: 0.5 + (tick % 2) as f64 * 0.01,
                y: 4.0,
                z: 0.5,
            },
            Rotation::default(),
            clock + Duration::from_millis(tick * 50),
        );
        assert!(matches!(result, MoveOutcome::Accepted { .. }));
        black_box(result);
    }
    println!(
        "Validated movement: 10000 operations in {:?}",
        started.elapsed()
    );
    let started = Instant::now();
    for index in 0..1000 {
        let now = clock + Duration::from_millis(500_000 + index * 100);
        let place = game.place_block(
            player.id,
            BlockPos { x: 1, y: 3, z: 1 },
            1,
            0,
            sculith_world::BlockKind::Stone,
            now,
        );
        assert!(matches!(place, BlockOutcome::Committed { .. }));
        black_box(place);
        let remove = game.break_block(player.id, BlockPos { x: 1, y: 4, z: 1 }, now);
        assert!(matches!(remove, BlockOutcome::Committed { .. }));
        black_box(remove);
    }
    println!(
        "Validated placement/break: 1000 pairs in {:?}; region revision {}",
        started.elapsed(),
        game.revision()
    );
    let mut snapshot = game.player(player.id).unwrap();
    let started = Instant::now();
    for index in 0..10_000 {
        let (source, destination) = if index % 2 == 0 { (0, 3) } else { (3, 0) };
        let reference = |slot: usize| SlotRef {
            slot: Slot::Inventory(slot as u8),
            expected_id: snapshot.inventory[slot].map_or(0, |stack| stack.net_id),
        };
        let outcome = game
            .inventory_request(
                player.id,
                Request {
                    id: -(index + 1),
                    actions: vec![Action::Transfer {
                        source: reference(source),
                        destination: reference(destination),
                        count: 1,
                    }],
                },
                clock + Duration::from_millis(700_000 + index as u64 * 25),
            )
            .unwrap();
        assert!(outcome.response.result.is_ok() && !outcome.response.replayed);
        black_box(&outcome);
        snapshot = outcome.player;
    }
    assert_eq!(snapshot.inventory[0].unwrap().count, 64);
    assert!(snapshot.inventory[3].is_none());
    assert_eq!(game.revision(), 2000);
    println!(
        "Validated inventory transfers: 10000 operations in {:?}; region revision {}",
        started.elapsed(),
        game.revision()
    );
    println!(
        "Synthetic monotonic clock; no sockets, rendering, physics tick or persistence workload."
    );
}
