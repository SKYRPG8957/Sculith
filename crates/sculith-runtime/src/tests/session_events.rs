use super::*;

#[tokio::test]
async fn lagged_peers_recover_current_membership_pose_equipment_and_continuation() {
    use crate::peers::{Change, Peers};
    use tokio::sync::broadcast::error::TryRecvError;
    let (stop, receiver) = watch::channel(false);
    let (world, task) = create(receiver);
    let task = tokio::spawn(task);
    let viewer = world
        .reserve(profile("resync-viewer"), Permissions::default())
        .await
        .unwrap();
    world.activate(viewer.player.id).await.unwrap();
    let leaving = world
        .reserve(profile("resync-leaving"), Permissions::default())
        .await
        .unwrap();
    let leaving_id = leaving.player.id;
    let leaving_uuid = leaving.player.profile.uuid;
    world.activate(leaving_id).await.unwrap();
    let mover = world
        .reserve(profile("resync-mover"), Permissions::default())
        .await
        .unwrap();
    world.activate(mover.player.id).await.unwrap();
    let observation = world.observe_players(viewer.player.id).await.unwrap();
    let mut old_updates = observation.updates;
    let mut peers = Peers::new(viewer.player.id, observation.players).unwrap();
    let interest = [(ChunkPos { x: 0, z: 0 }, 0)].into();
    assert_eq!(peers.initial(&interest).len(), 4);
    for tick in 1..=512 {
        assert!(matches!(
            world
                .move_player(
                    mover.player.id,
                    tick,
                    Position {
                        x: 0.5 + (tick % 2) as f64 * 0.0001,
                        y: 4.0,
                        z: 0.5,
                    },
                    Rotation::default()
                )
                .await
                .unwrap(),
            MoveOutcome::Accepted { .. }
        ));
    }
    drop(leaving);
    let joined = world
        .reserve(profile("resync-joined"), Permissions::default())
        .await
        .unwrap();
    world.activate(joined.player.id).await.unwrap();
    world.select_slot(mover.player.id, 1).await.unwrap();
    assert!(matches!(
        old_updates.try_recv(),
        Err(TryRecvError::Lagged(_))
    ));
    let mut fresh = world.observe_players(viewer.player.id).await.unwrap();
    let changes = peers.synchronize(fresh.players, &interest).unwrap();
    assert!(
        matches!(changes.as_slice(), [Change::Despawn { id }, Change::InfoRemove { uuid },
        Change::Equipment(equipped), Change::Move { player: moved, tick: 512 }, Change::InfoAdd(added), Change::Spawn(spawned)]
        if *id == leaving_id && *uuid == leaving_uuid && equipped.id == mover.player.id && equipped.selected_slot == 1
        && moved.movement_tick == Some(512) && added.id == joined.player.id && spawned.id == joined.player.id)
    );
    world
        .move_player(
            mover.player.id,
            513,
            Position {
                x: 0.501,
                y: 4.0,
                z: 0.5,
            },
            Rotation::default(),
        )
        .await
        .unwrap();
    let event = fresh.updates.recv().await.unwrap();
    assert!(matches!(
        peers.event(event.as_ref(), &interest).unwrap().as_slice(),
        [Change::Move { tick: 513, .. }]
    ));
    assert_eq!(world.capture().await.unwrap().revision(), 0);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn movement_uses_the_peer_stream_without_evicting_session_block_updates() {
    use tokio::sync::broadcast::error::TryRecvError;
    let (stop, receiver) = watch::channel(false);
    let (world, task) = create(receiver);
    let task = tokio::spawn(task);
    let mut session = world.subscribe_session();
    let actor = world
        .reserve(profile("session-queue"), Permissions { can_build: true })
        .await
        .unwrap();
    let id = actor.player.id;
    world.activate(id).await.unwrap();
    let mut all = world.subscribe();
    for tick in 1..=512 {
        assert!(matches!(
            world
                .move_player(
                    id,
                    tick,
                    Position {
                        x: 0.5 + (tick % 2) as f64 * 0.0001,
                        y: 4.0,
                        z: 0.5
                    },
                    Rotation::default()
                )
                .await
                .unwrap(),
            MoveOutcome::Accepted { .. }
        ));
    }
    assert!(matches!(session.try_recv(), Err(TryRecvError::Empty)));
    assert!(
        matches!(all.try_recv(), Err(TryRecvError::Lagged(_))),
        "the full observer stream remains bounded and includes movement"
    );
    assert!(matches!(
        world
            .break_block(id, BlockPos { x: 1, y: 3, z: 1 })
            .await
            .unwrap(),
        BlockOutcome::Committed { .. }
    ));
    assert!(
        matches!(session.try_recv().unwrap().as_ref(), WorldEvent::Blocks { player_id, .. } if *player_id == id)
    );
    assert!(matches!(session.try_recv(), Err(TryRecvError::Empty)));
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}
